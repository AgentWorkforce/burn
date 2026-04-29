import { createHash } from 'node:crypto';
import { stat } from 'node:fs/promises';

import { parseClaudeSessionIncremental } from '@relayburn/reader';
import type {
  SessionRelationshipRecord,
  ToolResultEventRecord,
  ToolResultEventSource,
  ToolResultStatus,
} from '@relayburn/reader';
import {
  appendCompactions,
  appendContent,
  appendRelationships,
  appendToolResultEvents,
  appendTurns,
  appendUserTurns,
  loadConfig,
  loadCursors,
  queryToolResultEvents,
  saveCursorChanges,
  withLock,
} from '@relayburn/ledger';
import type { ClaudeCursor } from '@relayburn/ledger';

import type { ParsedArgs } from '../args.js';

const INGEST_HELP = `burn ingest — hook-driven ingest from an agent harness

Usage:
  burn ingest --runtime claude [--quiet]

Reads a hook payload JSON from stdin and incrementally ingests the session
transcript it references. Safe to call from every Claude Code hook
(PreToolUse, PostToolUse, UserPromptSubmit, SubagentStop, SessionEnd) — the
ledger's cursor + dedup index keep re-invocations idempotent.
`;

interface ClaudeHookPayload extends Record<string, unknown> {
  session_id?: string;
  transcript_path?: string;
  hook_event_name?: string;
  tool_use_id?: string;
  tool_response?: unknown;
  message?: string;
  agent_id?: string;
  agent_type?: string;
  last_assistant_message?: string;
}

interface HookEventDraft {
  toolUseId: string;
  status: ToolResultStatus;
  eventSource: ToolResultEventSource;
  contentLength?: number;
  contentHash?: string;
  isError?: boolean;
  agentId?: string;
  subagentSessionId?: string;
}

export async function runIngest(args: ParsedArgs): Promise<number> {
  const runtime = typeof args.flags['runtime'] === 'string' ? args.flags['runtime'] : undefined;
  const quiet = args.flags['quiet'] === true;
  if (args.positional[0] === 'help' || args.flags['help'] === true) {
    process.stdout.write(INGEST_HELP);
    return 0;
  }
  if (!runtime) {
    process.stderr.write(`burn: ingest requires --runtime <claude>\n\n${INGEST_HELP}`);
    return 2;
  }
  if (runtime !== 'claude') {
    process.stderr.write(`burn: unsupported runtime: ${runtime}\n\n${INGEST_HELP}`);
    return 2;
  }

  const raw = await readStdin();
  return ingestClaudeHookPayload(raw, { quiet });
}

export async function ingestClaudeHookPayload(
  raw: string,
  opts: { quiet: boolean },
): Promise<number> {
  if (!raw.trim()) {
    if (!opts.quiet) process.stderr.write(`[burn] ingest: empty stdin payload, nothing to do\n`);
    return 0;
  }
  let payload: ClaudeHookPayload;
  try {
    payload = JSON.parse(raw) as ClaudeHookPayload;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[burn] ingest: invalid JSON payload: ${msg}\n`);
    return 1;
  }
  const sessionId = payload.session_id;
  const transcriptPath = payload.transcript_path;
  if (!sessionId || !transcriptPath) {
    if (!opts.quiet) {
      process.stderr.write(
        `[burn] ingest: payload missing session_id or transcript_path; ignoring\n`,
      );
    }
    return 0;
  }
  try {
    await ingestClaudeHookRecords(payload);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    // Never propagate failure back to Claude Code — a non-zero exit from a
    // hook command can block the tool call. Log and move on.
    process.stderr.write(`[burn] ingest: ${msg}\n`);
  }
  try {
    await ingestClaudeTranscript(transcriptPath, opts);
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    // Never propagate failure back to Claude Code — a non-zero exit from a
    // hook command can block the tool call. Log and move on.
    process.stderr.write(`[burn] ingest: ${msg}\n`);
  }
  return 0;
}

async function ingestClaudeHookRecords(payload: ClaudeHookPayload): Promise<void> {
  const sessionId = stringField(payload, 'session_id');
  if (!sessionId) return;

  const drafts = buildClaudeHookEventDrafts(payload);
  const relationships = buildClaudeHookRelationships(payload);
  if (drafts.length === 0 && relationships.length === 0) return;

  await withLock('ledger', async () => {
    if (relationships.length > 0) await appendRelationships(relationships);
    if (drafts.length === 0) return;

    const existing = await queryToolResultEvents({ sessionId, source: 'claude-code' });
    const records: ToolResultEventRecord[] = [];
    const visible = [...existing];
    for (const draft of drafts) {
      const record = allocateHookEvent(sessionId, draft, visible);
      if (!record) continue;
      records.push(record);
      visible.push(record);
    }
    if (records.length > 0) await appendToolResultEvents(records);
  });
}

function buildClaudeHookEventDrafts(payload: ClaudeHookPayload): HookEventDraft[] {
  switch (payload.hook_event_name) {
    case 'PreToolUse':
      return buildPreToolUseDraft(payload);
    case 'PostToolUse':
      return buildPostToolUseDraft(payload);
    case 'SubagentStop':
      return buildSubagentStopDraft(payload);
    case 'Notification':
      return buildNotificationDraft(payload);
    default:
      return [];
  }
}

function buildPreToolUseDraft(payload: ClaudeHookPayload): HookEventDraft[] {
  const toolUseId = extractToolUseId(payload);
  if (!toolUseId) return [];
  return [
    {
      toolUseId,
      status: 'running',
      eventSource: 'tool_result',
    },
  ];
}

function buildPostToolUseDraft(payload: ClaudeHookPayload): HookEventDraft[] {
  const toolUseId = extractToolUseId(payload);
  if (!toolUseId) return [];
  const isError = toolResponseIsError(payload.tool_response);
  const draft: HookEventDraft = {
    toolUseId,
    status: isError ? 'errored' : 'completed',
    eventSource: 'tool_result',
  };
  if (isError) draft.isError = true;
  const measured = measureHookContent(payload.tool_response);
  if (measured.length !== undefined) draft.contentLength = measured.length;
  if (measured.hash !== undefined) draft.contentHash = measured.hash;
  return [draft];
}

function buildSubagentStopDraft(payload: ClaudeHookPayload): HookEventDraft[] {
  const agentId = stringField(payload, 'agent_id');
  const toolUseId = extractParentToolUseId(payload) ?? agentId;
  if (!toolUseId) return [];
  const status = deriveTerminalHookStatus(payload);
  const draft: HookEventDraft = {
    toolUseId,
    status,
    eventSource: 'subagent_notification',
  };
  if (status === 'errored') draft.isError = true;
  if (agentId !== undefined) draft.agentId = agentId;
  const measured = measureHookContent(payload.last_assistant_message);
  if (measured.length !== undefined) draft.contentLength = measured.length;
  if (measured.hash !== undefined) draft.contentHash = measured.hash;
  return [draft];
}

function buildNotificationDraft(payload: ClaudeHookPayload): HookEventDraft[] {
  const toolUseId = extractToolUseId(payload);
  if (!toolUseId) return [];
  const draft: HookEventDraft = {
    toolUseId,
    status: 'unknown',
    eventSource: 'progress_event',
  };
  const measured = measureHookContent(payload.message);
  if (measured.length !== undefined) draft.contentLength = measured.length;
  if (measured.hash !== undefined) draft.contentHash = measured.hash;
  return [draft];
}

function buildClaudeHookRelationships(
  payload: ClaudeHookPayload,
): SessionRelationshipRecord[] {
  if (payload.hook_event_name !== 'SubagentStop') return [];
  const sessionId = stringField(payload, 'session_id');
  const agentId = stringField(payload, 'agent_id');
  if (!sessionId || !agentId) return [];
  const row: SessionRelationshipRecord = {
    v: 1,
    source: 'claude-code',
    sessionId,
    relationshipType: 'subagent',
    relatedSessionId: stringField(payload, 'parent_agent_id') ?? sessionId,
    agentId,
  };
  const parentToolUseId = extractParentToolUseId(payload);
  if (parentToolUseId !== undefined) row.parentToolUseId = parentToolUseId;
  const subagentType = stringField(payload, 'agent_type');
  if (subagentType !== undefined) row.subagentType = subagentType;
  return [row];
}

function allocateHookEvent(
  sessionId: string,
  draft: HookEventDraft,
  existing: readonly ToolResultEventRecord[],
): ToolResultEventRecord | undefined {
  if (findMatchingHookEvent(draft, existing)) return undefined;

  const callIndex = nextCallIndex(draft.toolUseId, existing);
  const eventIndex = nextEventIndex(existing);
  const record: ToolResultEventRecord = {
    v: 1,
    source: 'claude-code',
    sessionId,
    toolUseId: draft.toolUseId,
    callIndex,
    eventIndex,
    status: draft.status,
    eventSource: draft.eventSource,
  };
  if (draft.contentLength !== undefined) record.contentLength = draft.contentLength;
  if (draft.contentHash !== undefined) record.contentHash = draft.contentHash;
  if (draft.isError !== undefined) record.isError = draft.isError;
  if (draft.agentId !== undefined) record.agentId = draft.agentId;
  if (draft.subagentSessionId !== undefined) record.subagentSessionId = draft.subagentSessionId;
  return record;
}

function findMatchingHookEvent(
  draft: HookEventDraft,
  existing: readonly ToolResultEventRecord[],
): ToolResultEventRecord | undefined {
  return existing.find((record) => {
    if (record.toolUseId !== draft.toolUseId) return false;
    if (record.eventSource !== draft.eventSource) return false;
    if (record.status !== draft.status) return false;
    if (draft.agentId !== undefined && record.agentId !== draft.agentId) return false;
    if (draft.eventSource === 'progress_event' && draft.contentHash !== undefined) {
      return record.contentHash === draft.contentHash;
    }
    return true;
  });
}

function nextCallIndex(
  toolUseId: string,
  existing: readonly ToolResultEventRecord[],
): number {
  let max = -1;
  for (const record of existing) {
    if (record.toolUseId !== toolUseId) continue;
    if (typeof record.callIndex !== 'number') continue;
    if (record.callIndex > max) max = record.callIndex;
  }
  return max + 1;
}

function nextEventIndex(existing: readonly ToolResultEventRecord[]): number {
  let max = -1;
  for (const record of existing) {
    if (record.eventIndex > max) max = record.eventIndex;
  }
  return max + 1;
}

async function appendReconciledToolResultEvents(
  records: ToolResultEventRecord[],
): Promise<void> {
  if (records.length === 0) return;

  await withLock('ledger', async () => {
    const visibleBySession = new Map<string, ToolResultEventRecord[]>();
    const reconciled: ToolResultEventRecord[] = [];

    for (const record of records) {
      let visible = visibleBySession.get(record.sessionId);
      if (!visible) {
        visible = await queryToolResultEvents({
          sessionId: record.sessionId,
          source: record.source,
        });
        visibleBySession.set(record.sessionId, visible);
      }

      if (hasTerminalToolResultEvent(record, visible)) continue;

      const nextRecord =
        visible.length === 0 ? record : reindexToolResultEvent(record, visible);
      reconciled.push(nextRecord);
      visible.push(nextRecord);
    }

    if (reconciled.length > 0) await appendToolResultEvents(reconciled);
  });
}

function reindexToolResultEvent(
  record: ToolResultEventRecord,
  visible: readonly ToolResultEventRecord[],
): ToolResultEventRecord {
  return {
    ...record,
    callIndex: nextCallIndex(record.toolUseId, visible),
    eventIndex: nextEventIndex(visible),
  };
}

function hasTerminalToolResultEvent(
  record: ToolResultEventRecord,
  visible: readonly ToolResultEventRecord[],
): boolean {
  if (record.eventSource !== 'tool_result') return false;
  if (!isTerminalToolResultStatus(record.status)) return false;
  return visible.some(
    (prior) =>
      prior.toolUseId === record.toolUseId &&
      prior.eventSource === 'tool_result' &&
      isTerminalToolResultStatus(prior.status),
  );
}

function isTerminalToolResultStatus(status: ToolResultStatus): boolean {
  return status === 'completed' || status === 'errored' || status === 'cancelled';
}

function extractToolUseId(payload: ClaudeHookPayload): string | undefined {
  return (
    stringField(payload, 'tool_use_id') ??
    stringField(payload, 'toolUseId') ??
    stringField(payload, 'parent_tool_use_id') ??
    stringField(payload, 'parentToolUseId')
  );
}

function extractParentToolUseId(payload: ClaudeHookPayload): string | undefined {
  return (
    stringField(payload, 'parent_tool_use_id') ??
    stringField(payload, 'parentToolUseId') ??
    stringField(payload, 'tool_use_id') ??
    stringField(payload, 'toolUseId') ??
    stringField(payload, 'agent_tool_use_id') ??
    stringField(payload, 'task_tool_use_id')
  );
}

function stringField(
  record: Record<string, unknown>,
  key: string,
): string | undefined {
  const value = record[key];
  return typeof value === 'string' && value.length > 0 ? value : undefined;
}

function boolField(record: Record<string, unknown>, key: string): boolean | undefined {
  const value = record[key];
  return typeof value === 'boolean' ? value : undefined;
}

function toolResponseIsError(response: unknown): boolean {
  if (!response || typeof response !== 'object') return false;
  const rec = response as Record<string, unknown>;
  return (
    boolField(rec, 'is_error') === true ||
    boolField(rec, 'isError') === true ||
    boolField(rec, 'success') === false
  );
}

function deriveTerminalHookStatus(payload: ClaudeHookPayload): ToolResultStatus {
  const explicit = stringField(payload, 'status');
  if (explicit === 'completed' || explicit === 'errored' || explicit === 'cancelled') {
    return explicit;
  }
  if (explicit === 'error' || explicit === 'failed' || explicit === 'failure') return 'errored';
  if (explicit === 'canceled') return 'cancelled';
  if (
    boolField(payload, 'is_error') === true ||
    boolField(payload, 'isError') === true ||
    stringField(payload, 'error') !== undefined ||
    stringField(payload, 'error_message') !== undefined
  ) {
    return 'errored';
  }
  if (
    boolField(payload, 'cancelled') === true ||
    boolField(payload, 'canceled') === true ||
    boolField(payload, 'interrupted') === true
  ) {
    return 'cancelled';
  }
  return 'completed';
}

function measureHookContent(content: unknown): { length?: number; hash?: string } {
  if (typeof content === 'string') {
    return { length: content.length, hash: contentHash(content) };
  }
  if (content === undefined || content === null) return {};
  try {
    const serialized = JSON.stringify(content);
    if (typeof serialized !== 'string') return {};
    return { length: serialized.length, hash: contentHash(serialized) };
  } catch {
    return {};
  }
}

function contentHash(value: string): string {
  return createHash('sha256').update(value).digest('hex').slice(0, 16);
}

async function ingestClaudeTranscript(
  file: string,
  opts: { quiet: boolean },
): Promise<void> {
  let st: Awaited<ReturnType<typeof stat>>;
  try {
    st = await stat(file);
  } catch {
    if (!opts.quiet) process.stderr.write(`[burn] ingest: no transcript at ${file}\n`);
    return;
  }
  if (!st.isFile()) return;

  const cfg = await loadConfig();
  const cursors = await loadCursors();
  const before = structuredClone(cursors) as typeof cursors;
  const prior = cursors[file];
  const priorClaude = prior?.kind === 'claude' ? prior : undefined;
  const rotated =
    !priorClaude ||
    priorClaude.inode !== st.ino ||
    st.mtimeMs < priorClaude.mtimeMs ||
    st.size < priorClaude.offsetBytes;
  const startOffset = rotated ? 0 : priorClaude.offsetBytes;

  if (!rotated && startOffset >= st.size) {
    priorClaude.mtimeMs = st.mtimeMs;
    await saveCursorChanges(before, cursors);
    return;
  }

  const parseOpts: Parameters<typeof parseClaudeSessionIncremental>[1] = {
    startOffset,
    sessionPath: file,
    contentMode: cfg.content.store,
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

  if (turns.length > 0) await appendTurns(turns);
  if (content.length > 0) await appendContent(content);
  if (events.length > 0) await appendCompactions(events);
  if (relationships.length > 0) await appendRelationships(relationships);
  if (toolResultEvents.length > 0) await appendReconciledToolResultEvents(toolResultEvents);
  if (userTurns.length > 0) await appendUserTurns(userTurns);

  const next: ClaudeCursor = {
    kind: 'claude',
    inode: st.ino,
    offsetBytes: endOffset,
    mtimeMs: st.mtimeMs,
  };
  if (lastUserText) next.lastUserText = lastUserText;
  cursors[file] = next;
  await saveCursorChanges(before, cursors);

  if (!opts.quiet && turns.length > 0) {
    process.stderr.write(`[burn] ingested ${turns.length} turns from ${file}\n`);
  }
}

async function readStdin(): Promise<string> {
  if (process.stdin.isTTY) return '';
  const chunks: Buffer[] = [];
  for await (const chunk of process.stdin) {
    chunks.push(typeof chunk === 'string' ? Buffer.from(chunk) : chunk);
  }
  return Buffer.concat(chunks).toString('utf8');
}
