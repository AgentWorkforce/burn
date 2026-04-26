import { readFile, readdir, stat } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';

import {
  parseClaudeSessionIncremental,
  parseCodexSessionIncremental,
  parseOpencodeSessionIncremental,
} from '@relayburn/reader';
import type {
  CodexResumeState,
  ContentRecord,
  ContentStoreMode,
  TurnRecord,
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
  saveCursors,
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

// Per-adapter content-capture gap aggregator. A "gap" is a session that the
// parser emitted in `contentMode === 'full'` mode with at least one tool call
// in a committed turn but zero `tool_result` ContentRecords — the load-bearing
// kind for `burn waste`'s tool-call attribution. See #59 / #33.
//
// We accumulate per adapter across the ingest loop and emit a single warning
// at the end. Suppression is per-process: once an adapter has warned, later
// `ingestAll()` calls in the same `burn` invocation stay silent unless the
// adapter accumulates fresh affected sessions (which we re-check by
// comparing against the prior emit's session count).
interface GapStats {
  affectedSessions: number;
  orphanToolCalls: number;
}

interface GapTrackerState {
  // Adapters that have already emitted a warning at least once in this
  // process. Used to keep a flooding `--watch` loop quiet after the first
  // notice — if a second pass turns up *additional* affected sessions we
  // still warn, but a steady state stays silent.
  warnedAffectedSessions: Map<AdapterName, number>;
  // Override the writer used for warnings. Tests inject a buffer-backed sink
  // so they can assert on the formatted message without scribbling on
  // stderr.
  write: (msg: string) => void;
}

type AdapterName = 'claude' | 'codex' | 'opencode';

const moduleGapState: GapTrackerState = {
  warnedAffectedSessions: new Map(),
  write: (msg) => process.stderr.write(msg),
};

// Test-only: clear per-process suppression state. Safe to call from prod
// code too (it's a no-op when nothing has been warned yet).
export function resetIngestGapWarnings(): void {
  moduleGapState.warnedAffectedSessions.clear();
}

// Test-only: replace the warning sink. Returns the previous sink so callers
// can restore it.
export function setIngestGapWriter(write: (msg: string) => void): (msg: string) => void {
  const prev = moduleGapState.write;
  moduleGapState.write = write;
  return prev;
}

export async function ingestClaudeProjects(): Promise<IngestReport> {
  await cleanupStalePendingStamps();
  const cursors = await loadCursors();
  const report = emptyReport();
  const contentMode = await resolveContentMode();
  const gap: GapStats = { affectedSessions: 0, orphanToolCalls: 0 };
  await ingestClaudeInto(cursors, report, contentMode, gap);
  emitGapWarning('claude', contentMode, gap, moduleGapState);
  await saveCursors(cursors);
  return report;
}

export async function ingestCodexSessions(): Promise<IngestReport> {
  await cleanupStalePendingStamps();
  const cursors = await loadCursors();
  const report = emptyReport();
  const contentMode = await resolveContentMode();
  const gap: GapStats = { affectedSessions: 0, orphanToolCalls: 0 };
  await ingestCodexInto(cursors, report, contentMode, gap);
  emitGapWarning('codex', contentMode, gap, moduleGapState);
  await saveCursors(cursors);
  return report;
}

export async function ingestOpencodeSessions(): Promise<IngestReport> {
  await cleanupStalePendingStamps();
  const cursors = await loadCursors();
  const report = emptyReport();
  const contentMode = await resolveContentMode();
  const gap: GapStats = { affectedSessions: 0, orphanToolCalls: 0 };
  await ingestOpencodeInto(cursors, report, contentMode, gap);
  emitGapWarning('opencode', contentMode, gap, moduleGapState);
  await saveCursors(cursors);
  return report;
}

export async function ingestAll(): Promise<IngestReport> {
  await cleanupStalePendingStamps();
  const cursors = await loadCursors();
  const report = emptyReport();
  const contentMode = await resolveContentMode();
  const claudeGap: GapStats = { affectedSessions: 0, orphanToolCalls: 0 };
  const codexGap: GapStats = { affectedSessions: 0, orphanToolCalls: 0 };
  const opencodeGap: GapStats = { affectedSessions: 0, orphanToolCalls: 0 };
  await ingestClaudeInto(cursors, report, contentMode, claudeGap);
  await ingestCodexInto(cursors, report, contentMode, codexGap);
  await ingestOpencodeInto(cursors, report, contentMode, opencodeGap);
  emitGapWarning('claude', contentMode, claudeGap, moduleGapState);
  emitGapWarning('codex', contentMode, codexGap, moduleGapState);
  emitGapWarning('opencode', contentMode, opencodeGap, moduleGapState);
  await saveCursors(cursors);
  return report;
}

// Count tool calls in committed turns that lack a corresponding tool_result
// ContentRecord. Returns { sessionAffected, orphanCount } — a session is
// "affected" iff (a) it produced ≥1 turn with ≥1 tool call and (b) no
// tool_result records were captured for it. Per the issue, we ignore the
// `text`/`thinking`/`tool_use` content kinds because their absence is not
// load-bearing for `burn waste` attribution.
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
  stats: GapStats,
  state: GapTrackerState,
): void {
  if (contentMode !== 'full') return;
  if (stats.affectedSessions === 0) return;
  // Suppress if we've already warned for this adapter and no *additional*
  // affected sessions showed up since then. Without this, a `--watch` loop
  // would re-print the warning on every poll.
  const priorEmitted = state.warnedAffectedSessions.get(adapter);
  if (priorEmitted !== undefined && stats.affectedSessions <= priorEmitted) return;
  state.warnedAffectedSessions.set(adapter, stats.affectedSessions);
  state.write(
    `[burn] warning: ${adapter} parser produced 0 tool_result records for ${stats.affectedSessions} session${stats.affectedSessions === 1 ? '' : 's'} ` +
      `with ${stats.orphanToolCalls} tool call${stats.orphanToolCalls === 1 ? '' : 's'}. Content capture may not be implemented for this ` +
      `adapter, so burn waste will fall back to even-split attribution. See #33.\n`,
  );
}

async function resolveContentMode(): Promise<ContentStoreMode> {
  const cfg = await loadConfig();
  return cfg.content.store;
}

async function ingestClaudeInto(
  cursors: Record<string, FileCursor>,
  report: IngestReport,
  contentMode: ContentStoreMode,
  gap: GapStats,
): Promise<void> {
  const projects = await listDirs(claudeProjectsDir());
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
          // nothing new; refresh mtime bookkeeping
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
        } = await parseClaudeSessionIncremental(file, parseOpts);
        if (turns.length > 0) {
          await appendTurns(turns);
          report.appendedTurns += turns.length;
          report.ingestedSessions++;
          if (contentMode === 'full') {
            const { sessionAffected, orphanToolCalls } = countToolCallGaps(turns, content);
            if (sessionAffected) {
              gap.affectedSessions++;
              gap.orphanToolCalls += orphanToolCalls;
            }
          }
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
}

async function ingestCodexInto(
  cursors: Record<string, FileCursor>,
  report: IngestReport,
  contentMode: ContentStoreMode,
  gap: GapStats,
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
      const { turns, content, userTurns, endOffset, resume: nextResume } =
        await parseCodexSessionIncremental(file, opts);
      if (turns.length > 0) {
        const sessionId = nextResume.sessionId || turns[0]!.sessionId || (await deriveCodexSessionId(file));
        if (sessionId) {
          const candidate: Parameters<typeof resolvePendingStampsForSession>[0] = {
            harness: 'codex',
            sessionId,
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
        if (contentMode === 'full') {
          const { sessionAffected, orphanToolCalls } = countToolCallGaps(turns, content);
          if (sessionAffected) {
            gap.affectedSessions++;
            gap.orphanToolCalls += orphanToolCalls;
          }
        }
      }
      if (content.length > 0) {
        await appendContent(content);
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
  gap: GapStats,
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

      const { turns, content, userTurns, seenMessageIds: nextSeen } =
        await parseOpencodeSessionIncremental(file, {
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
        if (contentMode === 'full') {
          const { sessionAffected, orphanToolCalls } = countToolCallGaps(turns, content);
          if (sessionAffected) {
            gap.affectedSessions++;
            gap.orphanToolCalls += orphanToolCalls;
          }
        }
      }
      if (content.length > 0) {
        await appendContent(content);
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

export interface ReingestContentReport {
  scannedFiles: number;
  skippedExisting: number;
  reingestedSessions: number;
  appendedContent: number;
  failed: number;
}

// Re-parse source session files to populate missing content sidecars. Used by
// `burn rebuild --content` to fix up historical sessions ingested before the
// sidecar was written (or where the sidecar was pruned). Does NOT touch
// cursors, ledger turns, or compactions — only writes content records for
// sessions that currently have no sidecar on disk.
export async function reingestMissingContent(): Promise<ReingestContentReport> {
  const existing = await listContentSessionIds();
  const report: ReingestContentReport = {
    scannedFiles: 0,
    skippedExisting: 0,
    reingestedSessions: 0,
    appendedContent: 0,
    failed: 0,
  };
  await reingestClaudeContent(existing, report);
  await reingestCodexContent(existing, report);
  await reingestOpencodeContent(existing, report);
  return report;
}

async function reingestClaudeContent(
  existing: Set<string>,
  report: ReingestContentReport,
): Promise<void> {
  const projects = await listDirs(claudeProjectsDir());
  for (const projectDir of projects) {
    const files = await listJsonlFiles(projectDir);
    for (const file of files) {
      report.scannedFiles++;
      const sessionId = path.basename(file, '.jsonl');
      if (existing.has(sessionId)) {
        report.skippedExisting++;
        continue;
      }
      try {
        const { content } = await parseClaudeSessionIncremental(file, {
          startOffset: 0,
          sessionPath: file,
          contentMode: 'full',
        });
        const filtered = content.filter((c) => !existing.has(c.sessionId));
        if (filtered.length > 0) {
          await appendContent(filtered);
          report.appendedContent += filtered.length;
          report.reingestedSessions++;
          for (const c of filtered) existing.add(c.sessionId);
        }
      } catch (err) {
        report.failed++;
        const msg = err instanceof Error ? err.message : String(err);
        process.stderr.write(`[burn] reingest skipped ${file}: ${msg}\n`);
      }
    }
  }
}

async function reingestCodexContent(
  existing: Set<string>,
  report: ReingestContentReport,
): Promise<void> {
  for (const file of await walkJsonl(codexSessionsDir())) {
    report.scannedFiles++;
    const derived = await deriveCodexSessionId(file);
    if (derived && existing.has(derived)) {
      report.skippedExisting++;
      continue;
    }
    try {
      const { content } = await parseCodexSessionIncremental(file, {
        startOffset: 0,
        sessionPath: file,
        contentMode: 'full',
      });
      const filtered = content.filter((c) => !existing.has(c.sessionId));
      if (filtered.length > 0) {
        await appendContent(filtered);
        report.appendedContent += filtered.length;
        report.reingestedSessions++;
        for (const c of filtered) existing.add(c.sessionId);
      }
    } catch (err) {
      report.failed++;
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] reingest skipped ${file}: ${msg}\n`);
    }
  }
}

async function reingestOpencodeContent(
  existing: Set<string>,
  report: ReingestContentReport,
): Promise<void> {
  for (const file of await walkOpencodeSessions(opencodeSessionRoot())) {
    report.scannedFiles++;
    const sessionId = path.basename(file, '.json');
    if (existing.has(sessionId)) {
      report.skippedExisting++;
      continue;
    }
    try {
      const { content } = await parseOpencodeSessionIncremental(file, {
        sessionPath: file,
        seenMessageIds: new Set<string>(),
        contentMode: 'full',
      });
      const filtered = content.filter((c) => !existing.has(c.sessionId));
      if (filtered.length > 0) {
        await appendContent(filtered);
        report.appendedContent += filtered.length;
        report.reingestedSessions++;
        for (const c of filtered) existing.add(c.sessionId);
      }
    } catch (err) {
      report.failed++;
      const msg = err instanceof Error ? err.message : String(err);
      process.stderr.write(`[burn] reingest skipped ${file}: ${msg}\n`);
    }
  }
}

// Codex filenames are `rollout-<timestamp>-<uuid>.jsonl` where the UUID is the
// session id. Extract it for a cheap skip check before parsing. If the pattern
// doesn't match, return null and fall back to post-filtering.
export async function deriveCodexSessionId(file: string): Promise<string | null> {
  const base = path.basename(file, '.jsonl');
  const m = base.match(
    /([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$/,
  );
  if (m) return m[1]!;
  return readCodexSessionMetaId(file);
}

async function readCodexSessionMetaId(file: string): Promise<string | null> {
  let raw: string;
  try {
    raw = await readFile(file, 'utf8');
  } catch {
    return null;
  }
  for (const line of raw.split(/\r?\n/, 20)) {
    const text = line.trim();
    if (!text) continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      continue;
    }
    if (!parsed || typeof parsed !== 'object') continue;
    const rec = parsed as { type?: unknown; payload?: unknown };
    if (rec.type !== 'session_meta' || !rec.payload || typeof rec.payload !== 'object') continue;
    const id = (rec.payload as { id?: unknown }).id;
    return typeof id === 'string' && id.length > 0 ? id : null;
  }
  return null;
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
