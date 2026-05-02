import { readFile, readdir, stat } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';

import {
  parseClaudeSession,
  parseClaudeSessionIncremental,
  parseCodexSessionIncremental,
  parseOpencodeSessionIncremental,
  readCodexSessionIdHint,
  reconcileClaudeSessionRelationships,
} from '@relayburn/reader';
import type {
  CodexResumeState,
  ContentRecord,
  ContentStoreMode,
  ReconcileClaudeRelationshipsInput,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';
import {
  appendCompactions,
  appendContent,
  appendRelationships,
  appendToolResultEvents,
  appendTurns,
  appendUserTurns,
  listContentSessionIds,
  loadConfig,
  loadCursors,
  queryUserTurns,
  saveCursorChanges,
  type ClaudeCursor,
  type CodexCursor,
  type FileCursor,
  type OpencodeCursor,
} from '@relayburn/ledger';

import { walkJsonl, walkOpencodeSessions } from './walk.js';
import {
  cleanupStalePendingStamps,
  resolvePendingStampsForSession,
} from './pending-stamps.js';

// Resolved per-call so tests can swap HOME between runs. Cheap (string join).
function claudeProjectsDir(): string {
  return path.join(homedir(), '.claude', 'projects');
}
function codexSessionsDir(): string {
  return path.join(homedir(), '.codex', 'sessions');
}
function opencodeStorageDir(): string {
  return path.join(homedir(), '.local', 'share', 'opencode', 'storage');
}
function opencodeSessionRoot(): string {
  return path.join(opencodeStorageDir(), 'session');
}
function opencodeMessageRoot(): string {
  return path.join(opencodeStorageDir(), 'message');
}

export interface IngestReport {
  scannedSessions: number;
  ingestedSessions: number;
  appendedTurns: number;
}

export interface IngestOptions {
  onProgress?: (message: string) => void;
  // Receives the body of a gap warning (no leading symbol, no trailing
  // newline). Pass `task.warn` here to route the warning through an active
  // ora spinner so it lands as a yellow ⚠ banner that pauses and resumes
  // the spinner instead of tearing it.
  onWarn?: (body: string) => void;
}

// Per-adapter content-capture gap tracker. A session is "affected" iff a parse
// pass observed `tool_use` blocks for it in `contentMode === 'full'` mode
// without any observed `tool_result` ContentRecord — the load-bearing kind
// for `burn hotspots`'s tool-call attribution.
//
// Tracking is per-process and per-session (not per-call counts), so the set
// shrinks as later passes pick up the missing tool_result lines. The most
// common cause of a gap is a session that was still running when ingest
// observed the assistant tool_use line — the tool_result line gets flushed
// shortly after and the next pass heals the session. Sessions that were
// killed mid-call stay flagged permanently, which is the signal we want.
//
// Suppression: a warning fires only when the current affected set includes a
// session that was not present in the last emitted warning. Steady-state or
// shrinking sets stay silent, but churn that introduces a fresh affected
// session still re-warns even if the net count stays flat. After the set
// decays back to zero the suppression marker is cleared so a fresh gap from a
// future regression triggers a new warning.
type AdapterName = 'claude' | 'codex' | 'opencode';

interface GapTrackerState {
  // Sessions currently flagged as missing tool_result content, per adapter.
  affectedSessions: Map<AdapterName, Set<string>>;
  // Cumulative orphan tool-call count for each flagged session. Removed
  // alongside the session when it heals.
  orphanCallsPerSession: Map<AdapterName, Map<string, number>>;
  // Sessions known to have emitted ≥1 tool_result content record in this
  // process. Once a session is here it can never be re-flagged (capture
  // proved itself for that session at least once). Bounded by the number of
  // distinct sessionIds the process has touched.
  healedSessions: Map<AdapterName, Set<string>>;
  // Sessions included in the most recent emitted warning, per adapter. Used
  // to suppress repeats unless a newly affected session appears.
  warnedAffectedSessions: Map<AdapterName, Set<string>>;
  // Default sink for gap warnings when the caller has not provided an
  // `onWarn` (e.g. plain stderr contexts like `burn run` or watch loops).
  // Receives the warning body and is responsible for whatever framing the
  // sink wants — the default prepends the ⚠ glyph and a trailing newline.
  // Tests inject a buffer-backed sink so they can assert on the body
  // without scribbling on stderr.
  write: (body: string) => void;
}

const moduleGapState: GapTrackerState = {
  affectedSessions: new Map(),
  orphanCallsPerSession: new Map(),
  healedSessions: new Map(),
  warnedAffectedSessions: new Map(),
  write: (body) => process.stderr.write(`⚠ ${body}\n`),
};

// Test-only: clear per-process gap state. Safe to call from prod code too
// (it's a no-op when nothing has been observed yet).
export function resetIngestGapWarnings(): void {
  moduleGapState.affectedSessions.clear();
  moduleGapState.orphanCallsPerSession.clear();
  moduleGapState.healedSessions.clear();
  moduleGapState.warnedAffectedSessions.clear();
}

// Test-only: replace the warning sink. Returns the previous sink so callers
// can restore it.
export function setIngestGapWriter(write: (msg: string) => void): (msg: string) => void {
  const prev = moduleGapState.write;
  moduleGapState.write = write;
  return prev;
}

function getOrInit<K, V>(map: Map<K, V>, key: K, init: () => V): V {
  let v = map.get(key);
  if (v === undefined) {
    v = init();
    map.set(key, v);
  }
  return v;
}

// Update the process-wide gap state for one parse pass on one session.
// Called from each adapter's ingest loop after `parse*Incremental` returns,
// regardless of whether the pass produced new turns — `content` arriving
// without new turns is the heal case (tool_result line landed after its
// assistant tool_use was already cursored past).
function recordSessionGap(
  adapter: AdapterName,
  sessionId: string,
  newToolCalls: number,
  newToolResults: number,
  state: GapTrackerState,
): void {
  if (!sessionId) return;
  const affected = getOrInit(state.affectedSessions, adapter, () => new Set<string>());
  const orphans = getOrInit(state.orphanCallsPerSession, adapter, () => new Map<string, number>());
  const healed = getOrInit(state.healedSessions, adapter, () => new Set<string>());
  if (newToolResults > 0) {
    // Any tool_result on this session proves capture works for it. Drop
    // orphan detail and immunize against future re-flags in this process,
    // trading per-call precision for stable warning behavior.
    affected.delete(sessionId);
    orphans.delete(sessionId);
    healed.add(sessionId);
    return;
  }
  if (newToolCalls === 0) return;
  // Once a session has shown that capture works for it, don't re-flag on a
  // later mid-flight observation; the tool_result will arrive on the next
  // pass and we'd just be flapping.
  if (healed.has(sessionId)) return;
  affected.add(sessionId);
  orphans.set(sessionId, (orphans.get(sessionId) ?? 0) + newToolCalls);
}

// Sum tool calls across the parsed turns of one batch.
function countNewToolCalls(turns: readonly TurnRecord[]): number {
  let n = 0;
  for (const t of turns) n += t.toolCalls.length;
  return n;
}

// Count `tool_result` ContentRecords in the parsed batch.
function countNewToolResults(content: readonly ContentRecord[]): number {
  let n = 0;
  for (const c of content) if (c.kind === 'tool_result') n++;
  return n;
}

export async function ingestClaudeProjects(opts: IngestOptions = {}): Promise<IngestReport> {
  opts.onProgress?.('cleaning pending spawn stamps');
  await cleanupStalePendingStamps();
  opts.onProgress?.('loading ingest cursors');
  const cursors = await loadCursors();
  const before = cloneCursors(cursors);
  const report = emptyReport();
  opts.onProgress?.('loading content settings');
  const contentMode = await resolveContentMode();
  opts.onProgress?.('scanning Claude Code sessions');
  await ingestClaudeInto(cursors, report, contentMode);
  emitGapWarning('claude', contentMode, moduleGapState, opts.onWarn);
  opts.onProgress?.('saving ingest cursors');
  await saveCursorChanges(before, cursors);
  return report;
}

export async function ingestCodexSessions(opts: IngestOptions = {}): Promise<IngestReport> {
  opts.onProgress?.('cleaning pending spawn stamps');
  await cleanupStalePendingStamps();
  opts.onProgress?.('loading ingest cursors');
  const cursors = await loadCursors();
  const before = cloneCursors(cursors);
  const report = emptyReport();
  opts.onProgress?.('loading content settings');
  const contentMode = await resolveContentMode();
  opts.onProgress?.('scanning Codex sessions');
  await ingestCodexInto(cursors, report, contentMode);
  emitGapWarning('codex', contentMode, moduleGapState, opts.onWarn);
  opts.onProgress?.('saving ingest cursors');
  await saveCursorChanges(before, cursors);
  return report;
}

export async function ingestOpencodeSessions(opts: IngestOptions = {}): Promise<IngestReport> {
  opts.onProgress?.('cleaning pending spawn stamps');
  await cleanupStalePendingStamps();
  opts.onProgress?.('loading ingest cursors');
  const cursors = await loadCursors();
  const before = cloneCursors(cursors);
  const report = emptyReport();
  opts.onProgress?.('loading content settings');
  const contentMode = await resolveContentMode();
  opts.onProgress?.('scanning OpenCode sessions');
  await ingestOpencodeInto(cursors, report, contentMode);
  emitGapWarning('opencode', contentMode, moduleGapState, opts.onWarn);
  opts.onProgress?.('saving ingest cursors');
  await saveCursorChanges(before, cursors);
  return report;
}

// Per-session fast-path used by the claude harness adapter after a `burn run`
// exits. Unlike the directory-scanning ingest functions above, the caller
// already knows the sessionId from the spawn plan, so we go straight to that
// one JSONL and persist a cursor at EOF — a later `ingestAll` sweep then
// skips it instead of re-parsing and duplicating its content sidecar.
export async function ingestClaudeSession(cwd: string, sessionId: string): Promise<IngestReport> {
  const encoded = cwd.replace(/\//g, '-');
  const file = path.join(claudeProjectsDir(), encoded, `${sessionId}.jsonl`);
  let st: Awaited<ReturnType<typeof stat>>;
  try {
    st = await stat(file);
    if (!st.isFile()) return emptyReport();
  } catch {
    process.stderr.write(`[burn] no session file found at ${file}\n`);
    return emptyReport();
  }
  const contentMode = await resolveContentMode();
  const { turns, content, events, relationships, toolResultEvents, userTurns } =
    await parseClaudeSession(file, {
      sessionPath: file,
      contentMode,
    });
  if (turns.length === 0) return { scannedSessions: 1, ingestedSessions: 0, appendedTurns: 0 };
  await appendTurns(turns);
  if (content.length > 0) await appendContent(content);
  if (events.length > 0) await appendCompactions(events);
  if (relationships.length > 0) await appendRelationships(relationships);
  if (toolResultEvents.length > 0) await appendToolResultEvents(toolResultEvents);
  if (userTurns.length > 0) await appendUserTurns(userTurns);

  const cursors = await loadCursors();
  const before = cloneCursors(cursors);
  const cursor: ClaudeCursor = {
    kind: 'claude',
    inode: st.ino,
    offsetBytes: st.size,
    mtimeMs: st.mtimeMs,
  };
  cursors[file] = cursor;
  await saveCursorChanges(before, cursors);

  return { scannedSessions: 1, ingestedSessions: 1, appendedTurns: turns.length };
}

export async function ingestAll(opts: IngestOptions = {}): Promise<IngestReport> {
  opts.onProgress?.('cleaning pending spawn stamps');
  await cleanupStalePendingStamps();
  opts.onProgress?.('loading ingest cursors');
  const cursors = await loadCursors();
  const before = cloneCursors(cursors);
  const report = emptyReport();
  opts.onProgress?.('loading content settings');
  const contentMode = await resolveContentMode();
  opts.onProgress?.('scanning Claude Code sessions');
  await ingestClaudeInto(cursors, report, contentMode);
  opts.onProgress?.('scanning Codex sessions');
  await ingestCodexInto(cursors, report, contentMode);
  opts.onProgress?.('scanning OpenCode sessions');
  await ingestOpencodeInto(cursors, report, contentMode);
  emitGapWarning('claude', contentMode, moduleGapState, opts.onWarn);
  emitGapWarning('codex', contentMode, moduleGapState, opts.onWarn);
  emitGapWarning('opencode', contentMode, moduleGapState, opts.onWarn);
  opts.onProgress?.('saving ingest cursors');
  await saveCursorChanges(before, cursors);
  return report;
}

// Count tool calls in committed turns while no tool_result ContentRecord was
// captured for the session. Returns { sessionAffected, orphanCount } — a
// session is "affected" iff (a) it produced ≥1 turn with ≥1 tool call and (b)
// no tool_result records were captured for it. Per the issue, we ignore the
// `text`/`thinking`/`tool_use` content kinds because their absence is not
// load-bearing for `burn hotspots` attribution.
export function countToolCallGaps(
  turns: readonly TurnRecord[],
  content: readonly ContentRecord[],
): { sessionAffected: boolean; orphanToolCalls: number } {
  let toolCallsObserved = 0;
  for (const t of turns) {
    toolCallsObserved += t.toolCalls.length;
  }
  if (toolCallsObserved === 0) return { sessionAffected: false, orphanToolCalls: 0 };
  let toolResults = 0;
  for (const c of content) {
    if (c.kind === 'tool_result') toolResults++;
  }
  if (toolResults > 0) return { sessionAffected: false, orphanToolCalls: 0 };
  return { sessionAffected: true, orphanToolCalls: toolCallsObserved };
}

function emitGapWarning(
  adapter: AdapterName,
  contentMode: ContentStoreMode,
  state: GapTrackerState,
  onWarn: ((body: string) => void) | undefined,
): void {
  if (contentMode !== 'full') return;
  const affected = state.affectedSessions.get(adapter);
  if (affected === undefined || affected.size === 0) {
    // Set decayed back to empty — clear the suppression marker so a fresh
    // gap from a future regression triggers a new warning.
    state.warnedAffectedSessions.delete(adapter);
    return;
  }
  const prior = state.warnedAffectedSessions.get(adapter);
  let hasFreshAffectedSession = prior === undefined;
  if (prior !== undefined) {
    for (const sessionId of affected) {
      if (!prior.has(sessionId)) {
        hasFreshAffectedSession = true;
        break;
      }
    }
  }
  if (!hasFreshAffectedSession) return;
  state.warnedAffectedSessions.set(adapter, new Set(affected));
  let totalCalls = 0;
  const orphans = state.orphanCallsPerSession.get(adapter);
  if (orphans) for (const n of orphans.values()) totalCalls += n;
  const count = affected.size;
  const sessions = `${count} session${count === 1 ? '' : 's'}`;
  const calls = `${totalCalls} tool call${totalCalls === 1 ? '' : 's'}`;
  const body =
    `${adapter}: ${sessions} logged tool calls without any observed tool_result content (${calls}).\n` +
    `  Likely cause: still running (result line not yet flushed) or killed mid-call.\n` +
    `  Counts decay as later ingest passes pick up the result lines; sized hotspots\n` +
    `  attribution falls back to user-turn block sizes (or even-split) until they heal.`;
  if (onWarn) onWarn(body);
  else state.write(body);
}

async function resolveContentMode(): Promise<ContentStoreMode> {
  const cfg = await loadConfig();
  return cfg.content.store;
}

async function ingestClaudeInto(
  cursors: Record<string, FileCursor>,
  report: IngestReport,
  contentMode: ContentStoreMode,
): Promise<void> {
  const projects = await listDirs(claudeProjectsDir());
  // Cross-file relationship reconciliation. Collect per-file evidence
  // from every successful parse this pass and run one reconciliation step at
  // the end so fork / continuation rows that need cross-file knowledge get
  // emitted alongside the per-file `root` / `subagent` / `/resume` rows.
  const reconcileInputs: ReconcileClaudeRelationshipsInput[] = [];
  for (const projectDir of projects) {
    const files = await listJsonlFiles(projectDir);
    for (const file of files) {
      report.scannedSessions++;
      try {
        const st = await stat(file);
        const prior = cursors[file];
        const priorClaude = prior?.kind === 'claude' ? prior : undefined;
        const rotated =
          !priorClaude ||
          priorClaude.inode !== st.ino ||
          st.mtimeMs < priorClaude.mtimeMs ||
          st.size < priorClaude.offsetBytes;
        const startOffset = rotated ? 0 : priorClaude.offsetBytes;

        if (!rotated && startOffset >= st.size) {
          // Nothing new; refresh mtime bookkeeping and skip reconciliation
          // evidence — the file's relationships were emitted on the pass
          // that last touched it, and the writer's `relationshipIdHash`
          // dedup keeps subsequent passes idempotent. Cross-file detection
          // for an unchanged-vs-changed pair runs on the changed file's
          // pass when both happen to be active in the same window; one-off
          // late-arriving relationships rely on a future modification of
          // either file (or an explicit re-scan) to surface.
          priorClaude.mtimeMs = st.mtimeMs;
          continue;
        }

        const parseOpts: Parameters<typeof parseClaudeSessionIncremental>[1] = {
          startOffset,
          sessionPath: file,
          contentMode,
        };
        const priorUserText = rotated ? undefined : priorClaude?.lastUserText;
        if (priorUserText) parseOpts.lastUserText = priorUserText;
        const {
          turns,
          content,
          events,
          relationships,
          toolResultEvents,
          userTurns,
          endOffset,
          lastUserText,
          evidence,
        } = await parseClaudeSessionIncremental(file, parseOpts);
        if (turns.length > 0) {
          await appendTurns(turns);
          report.appendedTurns += turns.length;
          report.ingestedSessions++;
        }
        if (contentMode === 'full') {
          // Claude session files are 1:1 with sessionId, so the first
          // parsed record's id covers the whole incremental batch.
          const sessionId = turns[0]?.sessionId ?? content[0]?.sessionId ?? '';
          recordSessionGap(
            'claude',
            sessionId,
            countNewToolCalls(turns),
            countNewToolResults(content),
            moduleGapState,
          );
        }
        if (content.length > 0) {
          await appendContent(content);
        }
        if (events.length > 0) {
          await appendCompactions(events);
        }
        if (relationships.length > 0) {
          await appendRelationships(relationships);
        }
        if (toolResultEvents.length > 0) {
          await appendToolResultEvents(toolResultEvents);
        }
        if (userTurns.length > 0) {
          await appendUserTurns(userTurns);
        }
        // The incremental call only returned evidence for what it just read;
        // for cross-file reconciliation we want the full picture, so re-derive
        // evidence from the prefix when this pass started past offset 0.
        // The `firstParentUuid` / `seenUuids` carried by the prescan are
        // already populated when startOffset > 0, so the returned `evidence`
        // is whole — no second pass needed.
        reconcileInputs.push({ evidence });
        const next: ClaudeCursor = {
          kind: 'claude',
          inode: st.ino,
          offsetBytes: endOffset,
          mtimeMs: st.mtimeMs,
        };
        if (lastUserText) next.lastUserText = lastUserText;
        cursors[file] = next;
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        process.stderr.write(`[burn] skipping ${file}: ${msg}\n`);
      }
    }
  }
  // Cross-file reconciliation. Emits `fork` / `continuation` rows
  // beyond what each file's own parse pass could surface. The append writer's
  // `relationshipIdHash` dedup handles re-runs with identical inputs.
  if (reconcileInputs.length > 0) {
    const reconciled = reconcileClaudeSessionRelationships(reconcileInputs);
    if (reconciled.length > 0) await appendRelationships(reconciled);
  }
}

async function ingestCodexInto(
  cursors: Record<string, FileCursor>,
  report: IngestReport,
  contentMode: ContentStoreMode,
): Promise<void> {
  for (const file of await walkJsonl(codexSessionsDir())) {
    report.scannedSessions++;
    try {
      const st = await stat(file);
      const prior = cursors[file];
      const priorCodex = prior?.kind === 'codex' ? prior : undefined;
      const rotated =
        !priorCodex ||
        priorCodex.inode !== st.ino ||
        st.mtimeMs < priorCodex.mtimeMs ||
        st.size < priorCodex.offsetBytes;
      const startOffset = rotated ? 0 : priorCodex.offsetBytes;
      const resume: CodexResumeState | undefined = rotated
        ? undefined
        : {
            cumulative: { ...priorCodex.cumulative },
            sessionId: priorCodex.sessionId,
            turnContexts: { ...priorCodex.turnContexts },
            ...(priorCodex.sessionCwd !== undefined ? { sessionCwd: priorCodex.sessionCwd } : {}),
            ...(priorCodex.userTurnSlot !== undefined
              ? { userTurnSlot: priorCodex.userTurnSlot }
              : {}),
            rootSessionEmitted: priorCodex.rootSessionEmitted === true,
            nextEventIndex: priorCodex.nextEventIndex ?? 0,
            toolResultCounters: { ...(priorCodex.toolResultCounters ?? {}) },
            ...(priorCodex.lastCompletedTurn !== undefined
              ? { lastCompletedTurn: priorCodex.lastCompletedTurn }
              : {}),
          };

      if (!rotated && startOffset >= st.size) {
        priorCodex.mtimeMs = st.mtimeMs;
        continue;
      }

      const opts: Parameters<typeof parseCodexSessionIncremental>[1] = {
        startOffset,
        sessionPath: file,
        contentMode,
      };
      if (resume !== undefined) opts.resume = resume;
      const {
        turns,
        content,
        events,
        userTurns,
        relationships,
        toolResultEvents,
        endOffset,
        resume: nextResume,
      } = await parseCodexSessionIncremental(file, opts);
      let codexSessionId: string | undefined =
        nextResume.sessionId || turns[0]?.sessionId || content[0]?.sessionId;
      if (
        !codexSessionId &&
        (turns.length > 0 || (contentMode === 'full' && content.length > 0))
      ) {
        codexSessionId = (await deriveCodexSessionId(file)) ?? undefined;
      }
      if (turns.length > 0) {
        if (codexSessionId) {
          const candidate: Parameters<typeof resolvePendingStampsForSession>[0] = {
            harness: 'codex',
            sessionId: codexSessionId,
            sessionPath: file,
            sessionMtimeMs: st.mtimeMs,
          };
          const cwd = nextResume.sessionCwd ?? turns[0]!.project;
          if (cwd !== undefined) candidate.cwd = cwd;
          await resolvePendingStampsForSession(candidate);
        }
        await appendTurns(turns);
        report.appendedTurns += turns.length;
        report.ingestedSessions++;
      }
      if (contentMode === 'full') {
        recordSessionGap(
          'codex',
          codexSessionId ?? '',
          countNewToolCalls(turns),
          countNewToolResults(content),
          moduleGapState,
        );
      }
      if (content.length > 0) {
        await appendContent(content);
      }
      if (events.length > 0) {
        await appendCompactions(events);
      }
      if (relationships.length > 0) {
        await appendRelationships(relationships);
      }
      if (toolResultEvents.length > 0) {
        await appendToolResultEvents(toolResultEvents);
      }
      if (userTurns.length > 0) {
        await appendUserTurns(userTurns);
      }
      const next: CodexCursor = {
        kind: 'codex',
        inode: st.ino,
        offsetBytes: endOffset,
        mtimeMs: st.mtimeMs,
        cumulative: nextResume.cumulative,
        sessionId: nextResume.sessionId,
        turnContexts: nextResume.turnContexts,
      };
      if (nextResume.sessionCwd !== undefined) next.sessionCwd = nextResume.sessionCwd;
      if (nextResume.userTurnSlot !== undefined) next.userTurnSlot = nextResume.userTurnSlot;
      if (nextResume.rootSessionEmitted === true) next.rootSessionEmitted = true;
      if (nextResume.nextEventIndex !== undefined) next.nextEventIndex = nextResume.nextEventIndex;
      if (nextResume.toolResultCounters && Object.keys(nextResume.toolResultCounters).length > 0) {
        next.toolResultCounters = nextResume.toolResultCounters;
      }
      if (nextResume.lastCompletedTurn !== undefined) {
        next.lastCompletedTurn = nextResume.lastCompletedTurn;
      }
      cursors[file] = next;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] skipping ${file}: ${msg}\n`);
    }
  }
}

async function ingestOpencodeInto(
  cursors: Record<string, FileCursor>,
  report: IngestReport,
  contentMode: ContentStoreMode,
): Promise<void> {
  for (const file of await walkOpencodeSessions(opencodeSessionRoot())) {
    report.scannedSessions++;
    try {
      const sessionId = path.basename(file, '.json');
      const messageDir = path.join(opencodeMessageRoot(), sessionId);
      const messageMtime = await getDirMtime(messageDir);
      if (messageMtime === null) continue;

      const st = await stat(file);
      const prior = cursors[file];
      const priorOpencode = prior?.kind === 'opencode' ? prior : undefined;
      const rotated =
        !priorOpencode || priorOpencode.inode !== st.ino || messageMtime < priorOpencode.mtimeMs;
      const seenMessageIds = rotated
        ? new Set<string>()
        : new Set(priorOpencode.seenMessageIds);

      if (!rotated && messageMtime === priorOpencode.mtimeMs) {
        // nothing new
        continue;
      }

      const {
        turns,
        content,
        events,
        userTurns,
        relationships,
        toolResultEvents,
        seenMessageIds: nextSeen,
      } = await parseOpencodeSessionIncremental(file, {
        sessionPath: file,
        seenMessageIds,
        contentMode,
      });
      if (turns.length > 0) {
        const candidate: Parameters<typeof resolvePendingStampsForSession>[0] = {
          harness: 'opencode',
          sessionId,
          sessionPath: file,
          sessionMtimeMs: Math.max(st.mtimeMs, messageMtime),
        };
        if (turns[0]!.project !== undefined) candidate.cwd = turns[0]!.project;
        await resolvePendingStampsForSession(candidate);
        await appendTurns(turns);
        report.appendedTurns += turns.length;
        report.ingestedSessions++;
      }
      if (contentMode === 'full') {
        recordSessionGap(
          'opencode',
          sessionId,
          countNewToolCalls(turns),
          countNewToolResults(content),
          moduleGapState,
        );
      }
      if (content.length > 0) {
        await appendContent(content);
      }
      if (events.length > 0) {
        await appendCompactions(events);
      }
      if (relationships.length > 0) {
        await appendRelationships(relationships);
      }
      if (toolResultEvents.length > 0) {
        await appendToolResultEvents(toolResultEvents);
      }
      if (userTurns.length > 0) {
        await appendUserTurns(userTurns);
      }
      const next: OpencodeCursor = {
        kind: 'opencode',
        inode: st.ino,
        mtimeMs: messageMtime,
        seenMessageIds: [...nextSeen],
      };
      cursors[file] = next;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] skipping ${file}: ${msg}\n`);
    }
  }
}

function emptyReport(): IngestReport {
  return { scannedSessions: 0, ingestedSessions: 0, appendedTurns: 0 };
}

function cloneCursors(cursors: Record<string, FileCursor>): Record<string, FileCursor> {
  return structuredClone(cursors) as Record<string, FileCursor>;
}

export interface ReingestContentReport {
  scannedFiles: number;
  skippedExisting: number;
  reingestedSessions: number;
  appendedContent: number;
  appendedUserTurns: number;
  failed: number;
}

// Re-parse source session files to populate missing content sidecars and
// user-turn rows. Used by `burn state rebuild content` to fix up historical
// sessions ingested before those derived records were written (or where the
// sidecar was pruned). Does NOT touch cursors, ledger turns, or compactions.
export async function reingestMissingContent(
  opts: IngestOptions = {},
): Promise<ReingestContentReport> {
  opts.onProgress?.('loading existing content records');
  const existingContent = await listContentSessionIds();
  opts.onProgress?.('loading existing user-turn records');
  const existingUserTurns = new Set(
    (await queryUserTurns()).map((userTurn) => userTurn.sessionId),
  );
  const report: ReingestContentReport = {
    scannedFiles: 0,
    skippedExisting: 0,
    reingestedSessions: 0,
    appendedContent: 0,
    appendedUserTurns: 0,
    failed: 0,
  };
  opts.onProgress?.('re-parsing Claude Code sessions for content');
  await reingestClaudeContent(existingContent, existingUserTurns, report);
  opts.onProgress?.('re-parsing Codex sessions for content');
  await reingestCodexContent(existingContent, existingUserTurns, report);
  opts.onProgress?.('re-parsing OpenCode sessions for content');
  await reingestOpencodeContent(existingContent, existingUserTurns, report);
  return report;
}

async function reingestClaudeContent(
  existingContent: Set<string>,
  existingUserTurns: Set<string>,
  report: ReingestContentReport,
): Promise<void> {
  const projects = await listDirs(claudeProjectsDir());
  for (const projectDir of projects) {
    const files = await listJsonlFiles(projectDir);
    for (const file of files) {
      report.scannedFiles++;
      const sessionId = path.basename(file, '.jsonl');
      if (existingContent.has(sessionId) && existingUserTurns.has(sessionId)) {
        report.skippedExisting++;
        continue;
      }
      try {
        const { content, userTurns } = await parseClaudeSessionIncremental(file, {
          startOffset: 0,
          sessionPath: file,
          contentMode: 'full',
        });
        await appendReingestedDerivedRecords(
          content,
          userTurns,
          existingContent,
          existingUserTurns,
          report,
        );
      } catch (err) {
        report.failed++;
        const msg = err instanceof Error ? err.message : String(err);
        process.stderr.write(`[burn] reingest skipped ${file}: ${msg}\n`);
      }
    }
  }
}

async function reingestCodexContent(
  existingContent: Set<string>,
  existingUserTurns: Set<string>,
  report: ReingestContentReport,
): Promise<void> {
  for (const file of await walkJsonl(codexSessionsDir())) {
    report.scannedFiles++;
    const derived = await deriveCodexSessionId(file);
    if (
      derived &&
      existingContent.has(derived) &&
      existingUserTurns.has(derived)
    ) {
      report.skippedExisting++;
      continue;
    }
    try {
      const { content, userTurns } = await parseCodexSessionIncremental(file, {
        startOffset: 0,
        sessionPath: file,
        contentMode: 'full',
      });
      await appendReingestedDerivedRecords(
        content,
        userTurns,
        existingContent,
        existingUserTurns,
        report,
      );
    } catch (err) {
      report.failed++;
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] reingest skipped ${file}: ${msg}\n`);
    }
  }
}

async function reingestOpencodeContent(
  existingContent: Set<string>,
  existingUserTurns: Set<string>,
  report: ReingestContentReport,
): Promise<void> {
  for (const file of await walkOpencodeSessions(opencodeSessionRoot())) {
    report.scannedFiles++;
    const sessionId = path.basename(file, '.json');
    if (existingContent.has(sessionId) && existingUserTurns.has(sessionId)) {
      report.skippedExisting++;
      continue;
    }
    try {
      const { content, userTurns } = await parseOpencodeSessionIncremental(file, {
        sessionPath: file,
        seenMessageIds: new Set<string>(),
        contentMode: 'full',
      });
      await appendReingestedDerivedRecords(
        content,
        userTurns,
        existingContent,
        existingUserTurns,
        report,
      );
    } catch (err) {
      report.failed++;
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] reingest skipped ${file}: ${msg}\n`);
    }
  }
}

async function appendReingestedDerivedRecords(
  content: readonly ContentRecord[],
  userTurns: readonly UserTurnRecord[],
  existingContent: Set<string>,
  existingUserTurns: Set<string>,
  report: ReingestContentReport,
): Promise<void> {
  const filteredContent = content.filter((c) => !existingContent.has(c.sessionId));
  const filteredUserTurns = userTurns.filter(
    (userTurn) => !existingUserTurns.has(userTurn.sessionId),
  );
  if (filteredContent.length === 0 && filteredUserTurns.length === 0) return;

  if (filteredContent.length > 0) {
    await appendContent(filteredContent);
    report.appendedContent += filteredContent.length;
    for (const c of filteredContent) existingContent.add(c.sessionId);
  }
  if (filteredUserTurns.length > 0) {
    await appendUserTurns([...filteredUserTurns]);
    report.appendedUserTurns += filteredUserTurns.length;
    for (const userTurn of filteredUserTurns) {
      existingUserTurns.add(userTurn.sessionId);
    }
  }

  const sessions = new Set<string>();
  for (const c of filteredContent) sessions.add(c.sessionId);
  for (const userTurn of filteredUserTurns) sessions.add(userTurn.sessionId);
  report.reingestedSessions += sessions.size;
}

// Codex filenames are `rollout-<timestamp>-<uuid>.jsonl` where the UUID is the
// session id. Extract it for a cheap skip check before parsing. If the pattern
// doesn't match, peek at Codex's first-line session_meta hint before falling
// back to post-filtering.
export async function deriveCodexSessionId(file: string): Promise<string | null> {
  const base = path.basename(file, '.jsonl');
  const m = base.match(
    /([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$/,
  );
  if (m) return m[1]!;
  return readCodexSessionIdHint(file);
}

async function listDirs(parent: string): Promise<string[]> {
  try {
    const entries = await readdir(parent, { withFileTypes: true });
    return entries.filter((e) => e.isDirectory()).map((e) => path.join(parent, e.name));
  } catch {
    return [];
  }
}

async function listJsonlFiles(dir: string): Promise<string[]> {
  try {
    const entries = await readdir(dir, { withFileTypes: true });
    return entries
      .filter((e) => e.isFile() && e.name.endsWith('.jsonl'))
      .map((e) => path.join(dir, e.name));
  } catch {
    return [];
  }
}

async function getDirMtime(dir: string): Promise<number | null> {
  try {
    const s = await stat(dir);
    return s.mtimeMs;
  } catch {
    return null;
  }
}
