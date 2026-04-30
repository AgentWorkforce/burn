import { open } from 'node:fs/promises';

import { classifyActivity } from './classifier.js';
import { EMPTY_COVERAGE, makeFidelity } from './fidelity.js';
import { resolveProject } from './git.js';
import { argsHash, contentHash } from './hash.js';
import type {
  CompactionEvent,
  ContentRecord,
  ContentStoreMode,
  Coverage,
  Fidelity,
  SessionRelationshipRecord,
  ToolCall,
  ToolResultEventRecord,
  ToolResultStatus,
  TurnRecord,
  Usage,
  UserTurnBlock,
  UserTurnRecord,
} from './types.js';
import {
  createUserTurnTokenCounter,
  makeTextBlock,
  makeToolResultBlock,
} from './userTurn.js';
import type { UserTurnTokenCounter, UserTurnTokenizer } from './userTurn.js';

export interface ParseCodexOptions {
  sessionPath?: string;
  contentMode?: ContentStoreMode;
  // Controls how UserTurnBlock.approxTokens is computed. The default uses
  // cl100k; callers can opt into the historical bytes/4 heuristic for a cheap
  // proportional signal.
  tokenizer?: UserTurnTokenizer;
}

export interface ParseCodexIncrementalOptions extends ParseCodexOptions {
  startOffset?: number;
  resume?: CodexResumeState;
}

export interface CodexResumeState {
  cumulative: { input: number; output: number; cacheRead: number; reasoning: number };
  sessionId: string;
  sessionCwd?: string;
  turnContexts: Record<string, { turn_id?: string; cwd?: string; model?: string }>;
  // The user-turn slot in flight as of the last task_complete commit. Codex
  // user turns span the gap between two assistant turns, so the slot must
  // survive across resumed parses — tool outputs from the most recently
  // committed turn live here until the next task_started stamps `following`
  // and the subsequent task_complete commits the record.
  userTurnSlot?: PersistedUserTurnSlot;
  // Execution-graph counters. All committed-state — only advanced
  // at task_complete boundaries. These survive across resumes so the next
  // pass produces session-monotonic `eventIndex` values without duplicating
  // any already-emitted (sessionId, toolUseId, eventIndex) tuple, and so the
  // root relationship row is emitted exactly once per session id.
  rootSessionEmitted?: boolean;
  // Session-meta fork / continuation rows already committed. Codex can repeat
  // session_meta records after restarts; this keeps resumed parses from
  // re-emitting the same metadata edge.
  sessionMetaRelationshipKeys?: string[];
  nextEventIndex?: number;
  // Per-tool-call event counter — eventually filled with the `callIndex`
  // for the next event seen for this call_id. Codex tool calls almost
  // always have exactly one output, but spawn_agent / wait fanouts can
  // produce multiple events for the same call_id.
  toolResultCounters?: Record<string, number>;
  // Most recent committed assistant turn. Kept in the resume/cursor so a
  // later Codex compaction marker can be anchored to the turn it compacted.
  lastCompletedTurn?: CodexLastCompletedTurn;
}

export interface PersistedUserTurnSlot {
  blocks: UserTurnBlock[];
  precedingMessageId?: string;
  ts: string;
}

export interface CodexLastCompletedTurn {
  messageId: string;
  cacheRead: number;
}

export interface ParseCodexIncrementalResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  events: CompactionEvent[];
  userTurns: UserTurnRecord[];
  // Execution graph. `relationships` carries one `root` row per
  // newly-seen session id and one `subagent` row per `spawn_agent` call.
  // `toolResultEvents` carries one row per `function_call_output` /
  // `custom_tool_call_output` (with status patched from
  // `exec_command_end.exit_code` / `patch_apply_end.success` when seen) plus
  // one row per `subagent_message_complete` notification. Both arrays
  // respect the same committed-end-offset deferral the rest of the
  // incremental result uses, so resumed ingest doesn't double-emit.
  relationships: SessionRelationshipRecord[];
  toolResultEvents: ToolResultEventRecord[];
  endOffset: number;
  resume: CodexResumeState;
}

interface SessionMetaPayload {
  id?: string;
  cwd?: string;
  timestamp?: string;
  cli_version?: string;
  version?: string;
  sourceSessionId?: string;
  source_session_id?: string;
  forkSessionId?: string;
  fork_session_id?: string;
  continuedFromSessionId?: string;
  continued_from_session_id?: string;
}

function sessionMetaPayloadId(payload: unknown): string | null {
  if (!payload || typeof payload !== 'object') return null;
  const id = (payload as SessionMetaPayload).id;
  return typeof id === 'string' && id.length > 0 ? id : null;
}

interface TurnContextPayload {
  turn_id?: string;
  cwd?: string;
  model?: string;
}

interface TaskStartedPayload {
  type: 'task_started';
  turn_id?: string;
}

interface TaskCompletePayload {
  type: 'task_complete';
  turn_id?: string;
}

interface TokenUsage {
  input_tokens?: number;
  cached_input_tokens?: number;
  output_tokens?: number;
  reasoning_output_tokens?: number;
  total_tokens?: number;
}

interface TokenCountPayload {
  type: 'token_count';
  info?: {
    total_token_usage?: TokenUsage;
    last_token_usage?: TokenUsage;
  } | null;
}

interface FunctionCallPayload {
  type: 'function_call';
  name?: string;
  arguments?: string;
  call_id?: string;
}

interface CustomToolCallPayload {
  type: 'custom_tool_call';
  name?: string;
  input?: string;
  call_id?: string;
}

interface PatchApplyEndPayload {
  type: 'patch_apply_end';
  turn_id?: string;
  call_id?: string;
  success?: boolean;
  changes?: Record<string, unknown>;
}

interface ExecCommandEndPayload {
  type: 'exec_command_end';
  turn_id?: string;
  call_id?: string;
  exit_code?: number;
}

interface MessagePayload {
  type: 'message';
  role?: string;
  content?: Array<{ type?: string; text?: string }>;
}

interface ReasoningPayload {
  type: 'reasoning';
  summary?: Array<{ type?: string; text?: string }>;
  content?: Array<{ type?: string; text?: string }> | null;
}

interface FunctionCallOutputPayload {
  type: 'function_call_output';
  call_id?: string;
  output?: unknown;
}

interface CustomToolCallOutputPayload {
  type: 'custom_tool_call_output';
  call_id?: string;
  output?: unknown;
}

// `event_msg` payload that confirms a spawned subagent has reached a
// terminal state. Codex doesn't have a single canonical name for this —
// implementations vary across rollout versions. We accept any event_msg
// type beginning with `subagent_` and ending with `_complete` / `_done` /
// `_finished` (e.g. `subagent_message_complete`, `subagent_done`,
// `subagent_finished`) so long as it carries a `call_id` joining it back
// to the spawning function call.
interface SubagentNotificationPayload {
  type: string;
  call_id?: string;
  agent_id?: string;
  subagent_id?: string;
  session_id?: string;
  success?: boolean;
  status?: string;
}

interface CumulativeUsage {
  input: number;
  output: number;
  cacheRead: number;
  reasoning: number;
}

interface OpenTurn {
  turnId: string;
  ts: string;
  model: string;
  project?: string;
  startCumulative: CumulativeUsage;
  toolCalls: ToolCall[];
  seenCallIds: Set<string>;
  filesTouched: Set<string>;
  userText: string;
  assistantText: string;
  erroredCallIds: Set<string>;
  // Captured only when contentMode === 'full'. Emitted alongside the turn
  // once task_complete commits it; dropped if the turn never commits.
  content: ContentRecord[];
  // Per-turn buffer of pending execution-graph rows. They're committed (folded
  // into the parser-level `pendingToolResultEvents` / `pendingRelationships`
  // arrays) when the enclosing turn's task_complete fires, and dropped if
  // the turn never commits. status on tool-result events is patched at
  // commit time using `erroredCallIds`, since `exec_command_end` /
  // `patch_apply_end` ordering relative to the output payload isn't
  // guaranteed.
  pendingToolResultEvents: ToolResultEventRecord[];
  pendingRelationships: SessionRelationshipRecord[];
  // Per-call_id metadata for `spawn_agent` calls seen in this turn — used to
  // build the `subagent` SessionRelationshipRecord once the spawning call's
  // output (or terminal notification) resolves the spawned agent's id.
  spawnCalls: Map<string, SpawnCallInfo>;
  // Set true once a `token_count` event with `total_token_usage` is observed
  // while this turn is open. Drives the per-turn usage `Coverage` flags so a
  // turn whose source omitted `total_token_usage` lands in `class: 'partial'`
  // rather than silently reporting zeros as full-fidelity.
  usageObserved: boolean;
}

interface SpawnCallInfo {
  callId: string;
  ts: string;
  subagentType?: string;
  description?: string;
  // Resolved from the function_call_output / notification when seen.
  spawnedAgentId?: string;
  // Whether we've already emitted a SessionRelationshipRecord for this spawn.
  emitted: boolean;
}

interface FinalizedTurn
  extends Omit<
    OpenTurn,
    | 'startCumulative'
    | 'seenCallIds'
    | 'filesTouched'
    | 'erroredCallIds'
    | 'pendingToolResultEvents'
    | 'pendingRelationships'
    | 'spawnCalls'
    | 'usageObserved'
  > {
  usage: Usage;
  filesTouched: string[];
  erroredCallIds: Set<string>;
  fidelity: Fidelity;
}

interface UserTurnSlot {
  blocks: UserTurnBlock[];
  precedingMessageId?: string;
  ts: string;
}

export interface ParseCodexResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  events: CompactionEvent[];
  userTurns: UserTurnRecord[];
  // Execution graph. See `ParseCodexIncrementalResult` for the
  // shape; full parses always reflect the committed state at end-of-file.
  relationships: SessionRelationshipRecord[];
  toolResultEvents: ToolResultEventRecord[];
}

export async function parseCodexSession(
  filePath: string,
  options: ParseCodexOptions = {},
): Promise<ParseCodexResult> {
  const { turns, content, events, userTurns, relationships, toolResultEvents } =
    await parseCodexSessionIncremental(filePath, {
      ...options,
      startOffset: 0,
    });
  return { turns, content, events, userTurns, relationships, toolResultEvents };
}

export async function readCodexSessionIdHint(filePath: string): Promise<string | null> {
  try {
    const handle = await open(filePath, 'r');
    try {
      const buf = Buffer.alloc(8192);
      const { bytesRead } = await handle.read(buf, 0, buf.length, 0);
      if (bytesRead === 0) return null;

      const raw = buf.subarray(0, bytesRead).toString('utf8');
      const newline = raw.indexOf('\n');
      const firstLine = (newline === -1 ? raw : raw.slice(0, newline)).replace(/\r$/, '').trim();
      if (!firstLine) return null;

      let parsed: unknown;
      try {
        parsed = JSON.parse(firstLine);
      } catch {
        return null;
      }
      if (!parsed || typeof parsed !== 'object') return null;
      const rec = parsed as { type?: unknown; payload?: unknown };
      if (rec.type !== 'session_meta') return null;
      return sessionMetaPayloadId(rec.payload);
    } finally {
      await handle.close();
    }
  } catch {
    return null;
  }
}

export async function parseCodexSessionIncremental(
  filePath: string,
  options: ParseCodexIncrementalOptions = {},
): Promise<ParseCodexIncrementalResult> {
  const startOffset = options.startOffset ?? 0;
  const handle = await open(filePath, 'r');
  let buf: Buffer;
  let size: number;
  try {
    const st = await handle.stat();
    size = st.size;
    if (startOffset >= size) {
      return {
        turns: [],
        content: [],
        events: [],
        userTurns: [],
        relationships: [],
        toolResultEvents: [],
        endOffset: startOffset,
        resume: cloneResume(options.resume),
      };
    }
    const length = size - startOffset;
    buf = Buffer.allocUnsafe(length);
    await handle.read(buf, 0, length, startOffset);
  } finally {
    await handle.close();
  }

  const captureContent = options.contentMode === 'full';
  const tokenCounter = await createUserTurnTokenCounter(options.tokenizer);

  let sessionId = options.resume?.sessionId ?? '';
  let sessionCwd: string | undefined = options.resume?.sessionCwd;
  const turnContexts = new Map<string, TurnContextPayload>();
  if (options.resume) {
    for (const [k, v] of Object.entries(options.resume.turnContexts)) turnContexts.set(k, v);
  }
  const cumulative: CumulativeUsage = {
    input: options.resume?.cumulative.input ?? 0,
    output: options.resume?.cumulative.output ?? 0,
    cacheRead: options.resume?.cumulative.cacheRead ?? 0,
    reasoning: options.resume?.cumulative.reasoning ?? 0,
  };
  let openTurn: OpenTurn | null = null;
  let pendingUserText = '';
  // User content (and any stray records) that arrive before the next
  // task_started. Attached to the turn on open, so they only flush if the
  // turn itself eventually commits.
  let pendingContent: ContentRecord[] = [];
  const finalized: FinalizedTurn[] = [];

  // The user-turn slot accumulates user-side blocks (free text + tool outputs)
  // for the gap between two assistant turns. Lifecycle: blocks accrue during
  // an open turn or between turns; `precedingMessageId` is stamped at
  // task_complete; `followingMessageId` is stamped + the record is pushed to
  // `userTurns` at the next task_started; the record is committed (counted
  // toward `committedUserTurnsCount`) at the following turn's task_complete.
  let userTurnSlot: UserTurnSlot = options.resume?.userTurnSlot
    ? cloneSlot(options.resume.userTurnSlot)
    : { blocks: [], ts: '' };
  const userTurns: UserTurnRecord[] = [];

  // Execution-graph state. All four are committed-state — they
  // only count toward the final result once the enclosing turn's
  // task_complete fires. The counters and seen-set are seeded from the
  // resume blob so `eventIndex` stays session-monotonic across re-ingest
  // cycles and the root row is emitted exactly once per session id.
  let rootSessionEmitted = options.resume?.rootSessionEmitted === true;
  const seenSessionMetaRelationshipKeys = new Set(
    options.resume?.sessionMetaRelationshipKeys ?? [],
  );
  let nextEventIndex = options.resume?.nextEventIndex ?? 0;
  const toolResultCounters = new Map<string, number>();
  if (options.resume?.toolResultCounters) {
    for (const [k, v] of Object.entries(options.resume.toolResultCounters)) {
      toolResultCounters.set(k, v);
    }
  }
  // Pending records, tagged with the byte offset of their source line so
  // we can drop anything past `committedEndOffset` at output time. Loose
  // tool-result events (those whose function_call_output arrived without
  // an open turn) live here too; they're committed when the next
  // task_complete advances `committedEndOffset` past their offset.
  const pendingToolResultEvents: Array<{
    offset: number;
    record: ToolResultEventRecord;
  }> = [];
  const pendingRelationships: Array<{
    offset: number;
    record: SessionRelationshipRecord;
  }> = [];
  const pendingCompactions: Array<{
    offset: number;
    event: CompactionEvent;
  }> = [];

  // Commit snapshot — only advanced at task_complete boundaries.
  let committedEndOffset = startOffset;
  let committedCumulative: CumulativeUsage = { ...cumulative };
  let committedSessionId = sessionId;
  let committedSessionCwd = sessionCwd;
  let committedTurnContexts = new Map(turnContexts);
  let committedFinalizedCount = 0;
  let committedUserTurnsCount = 0;
  let committedUserTurnSlot: UserTurnSlot = cloneSlot(userTurnSlot);
  let committedRootSessionEmitted = rootSessionEmitted;
  let committedSessionMetaRelationshipKeys = new Set(seenSessionMetaRelationshipKeys);
  let committedNextEventIndex = nextEventIndex;
  let committedToolResultCounters = new Map(toolResultCounters);
  let lastCompletedTurn = cloneLastCompletedTurn(options.resume?.lastCompletedTurn);
  let committedLastCompletedTurn = cloneLastCompletedTurn(lastCompletedTurn);

  let p = 0;
  while (p < buf.length) {
    const nlIdx = buf.indexOf(0x0a, p);
    if (nlIdx === -1) break;
    const lineEndOffset = startOffset + nlIdx + 1;
    const text = buf.subarray(p, nlIdx).toString('utf8').trim();
    p = nlIdx + 1;
    if (!text) continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      continue;
    }
    if (!parsed || typeof parsed !== 'object') continue;
    const rec = parsed as {
      type?: string;
      timestamp?: string;
      payload?: unknown;
    };
    const payload = rec.payload;
    if (!payload || typeof payload !== 'object') continue;

    if (rec.type === 'session_meta') {
      const sp = payload as SessionMetaPayload;
      const metaSessionId = sessionMetaPayloadId(payload);
      if (metaSessionId) sessionId = metaSessionId;
      if (typeof sp.cwd === 'string') {
        sessionCwd = sp.cwd;
        if (openTurn && openTurn.project === undefined) openTurn.project = sp.cwd;
      }
      // Emit the root SessionRelationshipRecord exactly once per session id.
      // Codex always carries the session id on session_meta; the row is
      // pending until the next task_complete commits it (mirrors the
      // committed-end-offset deferral the rest of the parser uses), so a
      // session_meta without any subsequent task_complete won't surface a
      // root row in the result.
      if (typeof sessionId === 'string' && sessionId.length > 0 && !rootSessionEmitted) {
        rootSessionEmitted = true;
        const ts = typeof sp.timestamp === 'string' ? sp.timestamp : rec.timestamp;
        pendingRelationships.push({
          offset: lineEndOffset,
          record: buildRootRelationship(sessionId, ts, sp),
        });
      }
      if (typeof sessionId === 'string' && sessionId.length > 0) {
        for (const row of buildSessionMetaRelationships(sessionId, sp, rec.timestamp)) {
          const key = codexRelationshipKey(row);
          if (seenSessionMetaRelationshipKeys.has(key)) continue;
          seenSessionMetaRelationshipKeys.add(key);
          pendingRelationships.push({ offset: lineEndOffset, record: row });
        }
      }
      continue;
    }

    if (rec.type === 'turn_context') {
      const ctx = payload as TurnContextPayload;
      if (typeof ctx.turn_id === 'string') turnContexts.set(ctx.turn_id, ctx);
      if (openTurn && ctx.turn_id === openTurn.turnId) {
        if (!openTurn.model && typeof ctx.model === 'string') openTurn.model = ctx.model;
        if (openTurn.project === undefined && typeof ctx.cwd === 'string') {
          openTurn.project = ctx.cwd;
        }
      }
      continue;
    }

    const pl = payload as { type?: string };

    if (rec.type === 'compacted') {
      if (sessionId) {
        pendingCompactions.push({
          offset: lineEndOffset,
          event: buildCodexCompactionEvent(sessionId, rec.timestamp ?? '', lastCompletedTurn),
        });
      }
      continue;
    }

    if (rec.type === 'event_msg') {
      if (pl.type === 'token_count') {
        const tc = payload as TokenCountPayload;
        const total = tc.info?.total_token_usage;
        if (total) {
          const inputTotal = total.input_tokens ?? 0;
          const cached = total.cached_input_tokens ?? 0;
          cumulative.input = inputTotal - cached;
          cumulative.cacheRead = cached;
          cumulative.output = total.output_tokens ?? 0;
          cumulative.reasoning = total.reasoning_output_tokens ?? 0;
          // Mark coverage on the open turn — usage flags are honest only when
          // a `total_token_usage` actually arrived between task_started and
          // task_complete. Without this, finalize would emit zero-deltas as
          // `full` fidelity.
          if (openTurn) openTurn.usageObserved = true;
        }
        continue;
      }

      if (pl.type === 'task_started') {
        const ts = rec.timestamp ?? '';
        const ev = payload as TaskStartedPayload;
        const turnId = ev.turn_id;
        if (typeof turnId !== 'string') continue;
        if (openTurn) {
          finalized.push(finalizeTurn(openTurn, cumulative));
        }
        // Close the user-turn slot that bridges the previous assistant turn
        // and this one. `precedingMessageId` was stamped at the previous
        // task_complete (or left undef at session start); now we know
        // `followingMessageId`. The record is committed for emission at this
        // turn's task_complete.
        if (userTurnSlot.blocks.length > 0) {
          userTurns.push(buildCodexUserTurnRecord(userTurnSlot, sessionId, turnId, ts));
        }
        userTurnSlot = { blocks: [], ts: '' };
        const ctx = turnContexts.get(turnId);
        const project = ctx?.cwd ?? sessionCwd;
        openTurn = {
          turnId,
          ts,
          model: ctx?.model ?? '',
          startCumulative: { ...cumulative },
          toolCalls: [],
          seenCallIds: new Set(),
          filesTouched: new Set(),
          userText: pendingUserText,
          assistantText: '',
          erroredCallIds: new Set(),
          content: [],
          pendingToolResultEvents: [],
          pendingRelationships: [],
          spawnCalls: new Map(),
          usageObserved: false,
        };
        pendingUserText = '';
        if (captureContent && pendingContent.length > 0) {
          // Re-stamp pre-turn content with this turn's id so sidecar records
          // group under the turn that absorbed them, matching how
          // `pendingUserText` folds into `openTurn.userText`.
          for (const c of pendingContent) c.messageId = turnId;
          openTurn.content.push(...pendingContent);
          pendingContent = [];
        }
        if (project !== undefined) openTurn.project = project;
        continue;
      }

      if (pl.type === 'task_complete') {
        const ev = payload as TaskCompletePayload;
        if (openTurn && ev.turn_id === openTurn.turnId) {
          // Apply isError to any tool-result blocks accumulated during this
          // turn — exec_command_end / patch_apply_end fired before now and
          // populated `erroredCallIds`, but the function_call_output /
          // custom_tool_call_output payloads themselves don't carry status.
          for (const b of userTurnSlot.blocks) {
            if (
              b.kind === 'tool_result' &&
              b.toolUseId !== undefined &&
              openTurn.erroredCallIds.has(b.toolUseId)
            ) {
              b.isError = true;
            }
          }
          // Patch status / isError on pending tool_result events whose
          // call_id ended up in `erroredCallIds`. Then drain the turn's
          // pending execution-graph rows into the parser-level pending
          // arrays so they're committed for emission below.
          for (const ev of openTurn.pendingToolResultEvents) {
            if (openTurn.erroredCallIds.has(ev.toolUseId)) {
              ev.status = 'errored';
              ev.isError = true;
            } else if (ev.status === 'unknown') {
              ev.status = 'completed';
            }
            pendingToolResultEvents.push({ offset: lineEndOffset, record: ev });
          }
          for (const r of openTurn.pendingRelationships) {
            pendingRelationships.push({ offset: lineEndOffset, record: r });
          }
          // Stamp preceding so the next task_started knows this turn closed
          // off the slot and the record can be linked.
          userTurnSlot.precedingMessageId = openTurn.turnId;
          const closed = finalizeTurn(openTurn, cumulative);
          finalized.push(closed);
          lastCompletedTurn = {
            messageId: closed.turnId,
            cacheRead: closed.usage.cacheRead,
          };
          openTurn = null;
          committedEndOffset = lineEndOffset;
          committedCumulative = { ...cumulative };
          committedSessionId = sessionId;
          committedSessionCwd = sessionCwd;
          committedTurnContexts = new Map(turnContexts);
          committedFinalizedCount = finalized.length;
          committedUserTurnsCount = userTurns.length;
          committedUserTurnSlot = cloneSlot(userTurnSlot);
          committedRootSessionEmitted = rootSessionEmitted;
          committedSessionMetaRelationshipKeys = new Set(seenSessionMetaRelationshipKeys);
          committedNextEventIndex = nextEventIndex;
          committedToolResultCounters = new Map(toolResultCounters);
          committedLastCompletedTurn = cloneLastCompletedTurn(lastCompletedTurn);
        }
        continue;
      }

      if (pl.type === 'patch_apply_end') {
        const ev = payload as PatchApplyEndPayload;
        if (!openTurn || ev.turn_id !== openTurn.turnId) continue;
        if (ev.success === false) {
          if (typeof ev.call_id === 'string') openTurn.erroredCallIds.add(ev.call_id);
          continue;
        }
        const changes = ev.changes;
        if (changes && typeof changes === 'object') {
          for (const file of Object.keys(changes)) openTurn.filesTouched.add(file);
        }
        continue;
      }

      if (pl.type === 'exec_command_end') {
        const ev = payload as ExecCommandEndPayload;
        if (!openTurn || ev.turn_id !== openTurn.turnId) continue;
        if (typeof ev.exit_code === 'number' && ev.exit_code !== 0 && typeof ev.call_id === 'string') {
          openTurn.erroredCallIds.add(ev.call_id);
        }
        continue;
      }

      // Terminal subagent notification — joined back to the spawning call by
      // call_id. Codex emits these as `event_msg` payloads with
      // implementation-specific names; we accept any type starting with
      // `subagent_` and ending in a terminal-status suffix so the parser
      // doesn't have to track every rollout-version naming scheme.
      if (typeof pl.type === 'string' && isSubagentTerminalNotification(pl.type)) {
        const note = payload as SubagentNotificationPayload;
        if (typeof note.call_id !== 'string' || note.call_id.length === 0) continue;
        const callIndex = toolResultCounters.get(note.call_id) ?? 0;
        toolResultCounters.set(note.call_id, callIndex + 1);
        const status = subagentNotificationStatus(note);
        const evRec: ToolResultEventRecord = {
          v: 1,
          source: 'codex',
          sessionId,
          toolUseId: note.call_id,
          callIndex,
          eventIndex: nextEventIndex++,
          status,
          eventSource: 'subagent_notification',
        };
        if (openTurn) evRec.messageId = openTurn.turnId;
        if (rec.timestamp) evRec.ts = rec.timestamp;
        if (status === 'errored') evRec.isError = true;
        const spawnedId =
          (typeof note.agent_id === 'string' && note.agent_id) ||
          (typeof note.subagent_id === 'string' && note.subagent_id) ||
          (typeof note.session_id === 'string' && note.session_id) ||
          undefined;
        if (spawnedId) {
          evRec.agentId = spawnedId;
          evRec.subagentSessionId = spawnedId;
          // Backfill the spawn relationship if the spawning function_call
          // didn't carry the id and the function_call_output never did either.
          if (openTurn) {
            const spawn = openTurn.spawnCalls.get(note.call_id);
            if (spawn && !spawn.spawnedAgentId) {
              spawn.spawnedAgentId = spawnedId;
              maybeEmitSpawnRelationship(openTurn, sessionId, spawn, rec.timestamp ?? '');
            }
          }
        }
        if (openTurn) {
          openTurn.pendingToolResultEvents.push(evRec);
        } else {
          pendingToolResultEvents.push({ offset: lineEndOffset, record: evRec });
        }
        continue;
      }
      continue;
    }

    if (rec.type === 'response_item') {
      const itemTs = rec.timestamp ?? '';
      if (pl.type === 'message') {
        const msg = payload as MessagePayload;
        const text = collectMessageText(msg);
        if (text.length === 0) continue;
        if (msg.role === 'user') {
          // User messages can arrive before task_started; buffer them so the
          // next task_started picks them up as that turn's prompt text.
          if (openTurn) openTurn.userText = appendText(openTurn.userText, text);
          else pendingUserText = appendText(pendingUserText, text);
          // Capture the user prose as a UserTurnBlock for the slot bridging
          // the previous and next assistant turn.
          userTurnSlot.blocks.push(makeTextBlock(text, tokenCounter));
          if (!userTurnSlot.ts && itemTs) userTurnSlot.ts = itemTs;
          if (captureContent) {
            pushContent(openTurn, pendingContent, {
              v: 1,
              source: 'codex',
              sessionId,
              messageId: openTurn?.turnId ?? '',
              ts: itemTs,
              role: 'user',
              kind: 'text',
              text,
            });
          }
        } else if (msg.role === 'assistant' && openTurn) {
          openTurn.assistantText = appendText(openTurn.assistantText, text);
          if (captureContent) {
            openTurn.content.push({
              v: 1,
              source: 'codex',
              sessionId,
              messageId: openTurn.turnId,
              ts: itemTs,
              role: 'assistant',
              kind: 'text',
              text,
            });
          }
        }
        continue;
      }
      if (pl.type === 'reasoning' && openTurn && captureContent) {
        const rp = payload as ReasoningPayload;
        const text = collectReasoningText(rp);
        if (text.length > 0) {
          openTurn.content.push({
            v: 1,
            source: 'codex',
            sessionId,
            messageId: openTurn.turnId,
            ts: itemTs,
            role: 'assistant',
            kind: 'thinking',
            text,
          });
        }
        continue;
      }
      if (pl.type === 'function_call_output' || pl.type === 'custom_tool_call_output') {
        // Tool outputs can appear outside an open turn if codex streams them
        // after task_complete. Attribution only requires the call_id linkage,
        // which is preserved either way; we attach to the open turn when we
        // have one, or buffer as pre-turn content otherwise.
        const out = payload as FunctionCallOutputPayload | CustomToolCallOutputPayload;
        if (typeof out.call_id !== 'string') continue;
        // Always capture the output as a UserTurnBlock for the slot bridging
        // the open turn (or its predecessor) and the next assistant turn —
        // attribution doesn't require contentMode and shouldn't pay its cost.
        // isError is filled in at task_complete using `erroredCallIds`, since
        // exec_command_end / patch_apply_end ordering relative to the output
        // payload isn't guaranteed.
        userTurnSlot.blocks.push(
          makeToolResultBlock(out.call_id, out.output, undefined, tokenCounter),
        );
        if (!userTurnSlot.ts && itemTs) userTurnSlot.ts = itemTs;
        // Build the ToolResultEventRecord. Status is derived from
        // `erroredCallIds` if exec_command_end / patch_apply_end already fired
        // for this call_id; otherwise we fall back to `unknown` and patch at
        // task_complete (status arrival is not ordered relative to output).
        const callIndex = toolResultCounters.get(out.call_id) ?? 0;
        toolResultCounters.set(out.call_id, callIndex + 1);
        const initialStatus: ToolResultStatus = openTurn?.erroredCallIds.has(out.call_id)
          ? 'errored'
          : 'unknown';
        const evRec: ToolResultEventRecord = {
          v: 1,
          source: 'codex',
          sessionId,
          toolUseId: out.call_id,
          callIndex,
          eventIndex: nextEventIndex++,
          status: initialStatus,
          eventSource: 'function_call_output',
        };
        if (openTurn) evRec.messageId = openTurn.turnId;
        if (itemTs) evRec.ts = itemTs;
        if (initialStatus === 'errored') evRec.isError = true;
        const measured = measureToolOutput(out.output);
        if (measured.length !== undefined) evRec.contentLength = measured.length;
        if (measured.hash !== undefined) evRec.contentHash = measured.hash;
        // If the call was a `spawn_agent`, resolve the spawned agent's id from
        // the output payload and record it both on the event and the pending
        // SessionRelationshipRecord built when the function_call was seen.
        if (openTurn) {
          const spawn = openTurn.spawnCalls.get(out.call_id);
          if (spawn) {
            const spawnedId = extractSpawnedAgentId(out.output);
            if (spawnedId) {
              spawn.spawnedAgentId = spawnedId;
              evRec.agentId = spawnedId;
              evRec.subagentSessionId = spawnedId;
            }
            maybeEmitSpawnRelationship(openTurn, sessionId, spawn, itemTs);
          }
          openTurn.pendingToolResultEvents.push(evRec);
        } else {
          // Output landed outside any open turn (e.g. trailing after the
          // last task_complete). Commit it immediately as 'unknown' — we
          // can't retroactively patch status without an open-turn context,
          // and the next task_complete (if any) will simply pass it through.
          pendingToolResultEvents.push({ offset: lineEndOffset, record: evRec });
        }
        if (!captureContent) continue;
        pushContent(openTurn, pendingContent, {
          v: 1,
          source: 'codex',
          sessionId,
          messageId: openTurn?.turnId ?? '',
          ts: itemTs,
          role: 'tool_result',
          kind: 'tool_result',
          toolResult: { toolUseId: out.call_id, content: out.output ?? '' },
        });
        continue;
      }
      if (!openTurn) continue;
      if (pl.type === 'function_call') {
        const fc = payload as FunctionCallPayload;
        if (typeof fc.name !== 'string' || typeof fc.call_id !== 'string') continue;
        if (openTurn.seenCallIds.has(fc.call_id)) continue;
        openTurn.seenCallIds.add(fc.call_id);
        const parsedArgs = safeParseJson(fc.arguments);
        const call: ToolCall = {
          id: fc.call_id,
          name: fc.name,
          argsHash: argsHash(parsedArgs ?? {}),
        };
        const target = pickFunctionCallTarget(fc.name, parsedArgs);
        if (target !== undefined) call.target = target;
        openTurn.toolCalls.push(call);
        // Capture spawn_agent metadata so the eventual function_call_output /
        // subagent notification can complete a SessionRelationshipRecord.
        // The relationship row is emitted as soon as we know the spawned
        // agent's id (or, failing that, never — without an id the row would
        // carry no useful join key).
        if (fc.name === 'spawn_agent') {
          const info: SpawnCallInfo = {
            callId: fc.call_id,
            ts: itemTs,
            emitted: false,
          };
          if (parsedArgs) {
            const t = pickStringField(parsedArgs, ['subagent_type', 'agent_type', 'type']);
            const d = pickStringField(parsedArgs, ['description', 'task', 'prompt']);
            if (t !== undefined) info.subagentType = t;
            if (d !== undefined) info.description = d;
            const id = pickStringField(parsedArgs, ['agent_id', 'subagent_id', 'session_id']);
            if (id !== undefined) info.spawnedAgentId = id;
          }
          openTurn.spawnCalls.set(fc.call_id, info);
          maybeEmitSpawnRelationship(openTurn, sessionId, info, itemTs);
        }
        if (captureContent) {
          openTurn.content.push({
            v: 1,
            source: 'codex',
            sessionId,
            messageId: openTurn.turnId,
            ts: itemTs,
            role: 'assistant',
            kind: 'tool_use',
            toolUse: { id: fc.call_id, name: fc.name, input: parsedArgs ?? {} },
          });
        }
      } else if (pl.type === 'custom_tool_call') {
        const ct = payload as CustomToolCallPayload;
        if (typeof ct.name !== 'string' || typeof ct.call_id !== 'string') continue;
        if (openTurn.seenCallIds.has(ct.call_id)) continue;
        openTurn.seenCallIds.add(ct.call_id);
        const input = ct.input ?? '';
        const call: ToolCall = {
          id: ct.call_id,
          name: ct.name,
          argsHash: argsHash({ input }),
        };
        const target = pickCustomToolTarget(ct.name, input);
        if (target !== undefined) call.target = target;
        openTurn.toolCalls.push(call);
        if (captureContent) {
          openTurn.content.push({
            v: 1,
            source: 'codex',
            sessionId,
            messageId: openTurn.turnId,
            ts: itemTs,
            role: 'assistant',
            kind: 'tool_use',
            toolUse: { id: ct.call_id, name: ct.name, input: { input } },
          });
        }
      }
    }
  }

  // Only emit turns committed up to the last task_complete boundary.
  const committed = finalized.slice(0, committedFinalizedCount);
  const turns: TurnRecord[] = [];
  const content: ContentRecord[] = [];
  for (let i = 0; i < committed.length; i++) {
    const f = committed[i]!;
    const record: TurnRecord = {
      v: 1,
      source: 'codex',
      sessionId: committedSessionId,
      messageId: f.turnId,
      turnIndex: i,
      ts: f.ts,
      model: f.model,
      usage: f.usage,
      toolCalls: f.toolCalls,
      fidelity: f.fidelity,
    };
    if (options.sessionPath !== undefined) record.sessionPath = options.sessionPath;
    if (f.project !== undefined) {
      const resolved = resolveProject(f.project);
      record.project = resolved.project;
      if (resolved.projectKey !== undefined) record.projectKey = resolved.projectKey;
    }
    if (f.filesTouched.length > 0) record.filesTouched = f.filesTouched;
    const cText = [f.userText, f.assistantText].filter((s) => s.length > 0).join('\n');
    const hasFailedTool = f.toolCalls.some((tc) => f.erroredCallIds.has(tc.id));
    const classified = classifyActivity({
      toolCalls: f.toolCalls,
      text: cText,
      hasFailedTool,
      reasoningTokens: f.usage.reasoning,
    });
    record.activity = classified.activity;
    record.retries = classified.retries;
    record.hasEdits = classified.hasEdits;
    turns.push(record);
    if (captureContent) content.push(...f.content);
  }

  const resume: CodexResumeState = {
    cumulative: { ...committedCumulative },
    sessionId: committedSessionId,
    turnContexts: Object.fromEntries(committedTurnContexts),
    userTurnSlot: cloneSlot(committedUserTurnSlot),
    rootSessionEmitted: committedRootSessionEmitted,
    sessionMetaRelationshipKeys: [...committedSessionMetaRelationshipKeys],
    nextEventIndex: committedNextEventIndex,
    toolResultCounters: Object.fromEntries(committedToolResultCounters),
  };
  if (committedSessionCwd !== undefined) resume.sessionCwd = committedSessionCwd;
  const lastCompleted = cloneLastCompletedTurn(committedLastCompletedTurn);
  if (lastCompleted) resume.lastCompletedTurn = lastCompleted;

  const emittedUserTurns = userTurns.slice(0, committedUserTurnsCount);
  const emittedEvents: CompactionEvent[] = [];
  for (const e of pendingCompactions) {
    if (e.offset <= committedEndOffset) emittedEvents.push(e.event);
  }

  // Execution graph. Emit only records whose source line ended
  // at-or-before the committed end offset — anything past it belongs to an
  // open / partial turn and will be re-emitted by the next incremental pass.
  const emittedRelationships: SessionRelationshipRecord[] = [];
  for (const r of pendingRelationships) {
    if (r.offset <= committedEndOffset) emittedRelationships.push(r.record);
  }
  const emittedToolResultEvents: ToolResultEventRecord[] = [];
  for (const ev of pendingToolResultEvents) {
    if (ev.offset <= committedEndOffset) emittedToolResultEvents.push(ev.record);
  }

  return {
    turns,
    content,
    events: emittedEvents,
    userTurns: emittedUserTurns,
    relationships: emittedRelationships,
    toolResultEvents: emittedToolResultEvents,
    endOffset: committedEndOffset,
    resume,
  };
}

function pushContent(
  openTurn: OpenTurn | null,
  pending: ContentRecord[],
  record: ContentRecord,
): void {
  if (openTurn) openTurn.content.push(record);
  else pending.push(record);
}

function collectReasoningText(rp: ReasoningPayload): string {
  const parts: string[] = [];
  if (Array.isArray(rp.summary)) {
    for (const s of rp.summary) {
      if (s && typeof s.text === 'string' && s.text.length > 0) parts.push(s.text);
    }
  }
  if (Array.isArray(rp.content)) {
    for (const c of rp.content) {
      if (c && typeof c.text === 'string' && c.text.length > 0) parts.push(c.text);
    }
  }
  return parts.join('\n');
}

function buildCodexCompactionEvent(
  sessionId: string,
  ts: string,
  preceding: CodexLastCompletedTurn | undefined,
): CompactionEvent {
  const event: CompactionEvent = {
    v: 1,
    source: 'codex',
    sessionId,
    ts,
  };
  if (preceding) {
    event.precedingMessageId = preceding.messageId;
    event.tokensBeforeCompact = preceding.cacheRead;
  }
  return event;
}

function cloneResume(r: CodexResumeState | undefined): CodexResumeState {
  if (!r) {
    return {
      cumulative: { input: 0, output: 0, cacheRead: 0, reasoning: 0 },
      sessionId: '',
      turnContexts: {},
      userTurnSlot: { blocks: [], ts: '' },
      rootSessionEmitted: false,
      sessionMetaRelationshipKeys: [],
      nextEventIndex: 0,
      toolResultCounters: {},
    };
  }
  const out: CodexResumeState = {
    cumulative: { ...r.cumulative },
    sessionId: r.sessionId,
    turnContexts: { ...r.turnContexts },
    rootSessionEmitted: r.rootSessionEmitted === true,
    sessionMetaRelationshipKeys: [...(r.sessionMetaRelationshipKeys ?? [])],
    nextEventIndex: r.nextEventIndex ?? 0,
    toolResultCounters: { ...(r.toolResultCounters ?? {}) },
  };
  if (r.sessionCwd !== undefined) out.sessionCwd = r.sessionCwd;
  if (r.userTurnSlot) out.userTurnSlot = cloneSlot(r.userTurnSlot);
  else out.userTurnSlot = { blocks: [], ts: '' };
  const lastCompleted = cloneLastCompletedTurn(r.lastCompletedTurn);
  if (lastCompleted) out.lastCompletedTurn = lastCompleted;
  return out;
}

function cloneLastCompletedTurn(
  turn: CodexLastCompletedTurn | undefined,
): CodexLastCompletedTurn | undefined {
  if (!turn) return undefined;
  return { ...turn };
}

function cloneSlot(s: UserTurnSlot | PersistedUserTurnSlot): UserTurnSlot {
  const out: UserTurnSlot = {
    blocks: s.blocks.map((b) => ({ ...b })),
    ts: s.ts,
  };
  if (s.precedingMessageId !== undefined) out.precedingMessageId = s.precedingMessageId;
  return out;
}

// Build a UserTurnRecord for a slot whose `following` is now known.
// `userUuid` is synthesized from the surrounding assistant turn ids — Codex
// doesn't carry a stable per-line uuid for tool outputs, but the
// (preceding, following) pair is unique within a session and stable across
// resumes. When preceding is unset (session-start slot), we substitute
// "start".
function buildCodexUserTurnRecord(
  slot: UserTurnSlot,
  sessionId: string,
  followingMessageId: string,
  fallbackTs: string,
): UserTurnRecord {
  const precedingTag = slot.precedingMessageId ?? 'start';
  const userUuid = `${sessionId}:${precedingTag}->${followingMessageId}`;
  const record: UserTurnRecord = {
    v: 1,
    source: 'codex',
    sessionId,
    userUuid,
    ts: slot.ts || fallbackTs,
    blocks: slot.blocks,
    followingMessageId,
  };
  if (slot.precedingMessageId !== undefined) {
    record.precedingMessageId = slot.precedingMessageId;
  }
  return record;
}

function finalizeTurn(open: OpenTurn, cumulative: CumulativeUsage): FinalizedTurn {
  const usage: Usage = {
    input: Math.max(0, cumulative.input - open.startCumulative.input),
    output: Math.max(0, cumulative.output - open.startCumulative.output),
    reasoning: Math.max(0, cumulative.reasoning - open.startCumulative.reasoning),
    cacheRead: Math.max(0, cumulative.cacheRead - open.startCumulative.cacheRead),
    cacheCreate5m: 0,
    cacheCreate1h: 0,
  };
  const out: FinalizedTurn = {
    turnId: open.turnId,
    ts: open.ts,
    model: open.model,
    toolCalls: open.toolCalls,
    usage,
    filesTouched: [...open.filesTouched],
    userText: open.userText,
    assistantText: open.assistantText,
    erroredCallIds: open.erroredCallIds,
    content: open.content,
    fidelity: buildCodexFidelity(open.usageObserved),
  };
  if (open.project !== undefined) out.project = open.project;
  return out;
}

function buildCodexFidelity(usageObserved: boolean): Fidelity {
  // Mirror Claude's pattern (`buildClaudeFidelity`): coverage is *capability*,
  // not *presence* — `hasToolCalls` / `hasToolResultEvents` / `hasRawContent`
  // are flipped on for every Codex turn because the source can surface them,
  // not because this particular turn happened to carry one.
  //
  // Per-turn token coverage hinges on whether a `token_count` event with
  // `total_token_usage` arrived between `task_started` and `task_complete`.
  // When it didn't, the cumulative-delta arithmetic in `finalizeTurn` returns
  // 0 for each field — set the matching coverage flags to `false` so the
  // resulting `class` is `partial` rather than a silent zero.
  //
  // `hasCacheCreateTokens` is `false`: Codex rollouts have no ephemeral
  // cache-create concept (the `FULL_REQUIRED` matrix in fidelity.ts excludes
  // this field, so absence does not demote a turn from `full`).
  //
  // `hasSessionRelationships` is `true` — the Codex parser emits `root` /
  // `subagent` SessionRelationshipRecords as a capability, not a per-turn
  // presence, so tool-less turns still report `true`.
  const coverage: Coverage = {
    ...EMPTY_COVERAGE,
    hasInputTokens: usageObserved,
    hasOutputTokens: usageObserved,
    hasReasoningTokens: usageObserved,
    hasCacheReadTokens: usageObserved,
    hasCacheCreateTokens: false,
    hasToolCalls: true,
    hasToolResultEvents: true,
    hasSessionRelationships: true,
    hasRawContent: true,
  };
  return makeFidelity('per-turn', coverage);
}

// Codex user messages mix real prompts with harness boilerplate
// (environment_context, AGENTS.md injections, permissions instructions,
// collaboration_mode banners). Strip those so the classifier sees the text
// the user actually typed — keyword refinement depends on it.
const CODEX_BOILERPLATE_PATTERNS: RegExp[] = [
  /^\s*<environment_context/i,
  /^\s*<permissions/i,
  /^\s*<collaboration_mode/i,
  /^\s*<INSTRUCTIONS>/,
  /^\s*#\s*AGENTS\.md/i,
];

function collectMessageText(msg: MessagePayload): string {
  const content = msg.content;
  if (!Array.isArray(content)) return '';
  const parts: string[] = [];
  for (const block of content) {
    if (!block || typeof block !== 'object') continue;
    const text = block.text;
    if (typeof text !== 'string' || text.length === 0) continue;
    if (msg.role === 'user' && isCodexBoilerplate(text)) continue;
    parts.push(text);
  }
  return parts.join('\n');
}

function isCodexBoilerplate(text: string): boolean {
  return CODEX_BOILERPLATE_PATTERNS.some((re) => re.test(text));
}

function appendText(existing: string, next: string): string {
  if (!existing) return next;
  return existing + '\n' + next;
}

function safeParseJson(s: string | undefined): Record<string, unknown> | undefined {
  if (typeof s !== 'string' || s.length === 0) return undefined;
  try {
    const v = JSON.parse(s);
    if (v && typeof v === 'object' && !Array.isArray(v)) return v as Record<string, unknown>;
    return undefined;
  } catch {
    return undefined;
  }
}

function pickFunctionCallTarget(
  name: string,
  args: Record<string, unknown> | undefined,
): string | undefined {
  if (!args) return undefined;
  const s = (k: string): string | undefined => {
    const v = args[k];
    return typeof v === 'string' ? v : undefined;
  };
  switch (name) {
    case 'exec_command':
    case 'shell':
      return s('cmd') ?? s('command');
    case 'read_file':
      return s('path') ?? s('file_path');
    case 'write_file':
      return s('path') ?? s('file_path');
    default:
      return s('path') ?? s('file_path') ?? s('cmd') ?? s('command') ?? s('url');
  }
}

function pickCustomToolTarget(name: string, input: string): string | undefined {
  if (name === 'apply_patch') {
    const m = input.match(/\*\*\*\s+(?:Update|Add|Delete)\s+File:\s+(\S.*?)\s*$/m);
    if (m) return m[1];
  }
  return undefined;
}

// ---------------------------------------------------------------------------
// Execution graph helpers.
// ---------------------------------------------------------------------------

function buildRootRelationship(
  sessionId: string,
  ts?: string,
  meta?: SessionMetaPayload,
): SessionRelationshipRecord {
  const row: SessionRelationshipRecord = {
    v: 1,
    source: 'codex',
    sessionId,
    relationshipType: 'root',
  };
  if (typeof ts === 'string' && ts.length > 0) row.ts = ts;
  applyCodexSessionMetaProvenance(row, meta);
  return row;
}

function buildSessionMetaRelationships(
  sessionId: string,
  meta: SessionMetaPayload,
  fallbackTs?: string,
): SessionRelationshipRecord[] {
  const rows: SessionRelationshipRecord[] = [];
  const ts = stringField(meta, ['timestamp']) ?? fallbackTs;
  const forkSessionId = stringField(meta, ['forkSessionId', 'fork_session_id']);
  if (forkSessionId !== undefined && forkSessionId !== sessionId) {
    const row: SessionRelationshipRecord = {
      v: 1,
      source: 'codex',
      sessionId,
      relatedSessionId: forkSessionId,
      relationshipType: 'fork',
    };
    if (typeof ts === 'string' && ts.length > 0) row.ts = ts;
    applyCodexSessionMetaProvenance(row, meta);
    rows.push(row);
  }
  const continuedFromSessionId = stringField(meta, [
    'continuedFromSessionId',
    'continued_from_session_id',
  ]);
  if (continuedFromSessionId !== undefined && continuedFromSessionId !== sessionId) {
    const row: SessionRelationshipRecord = {
      v: 1,
      source: 'codex',
      sessionId,
      relatedSessionId: continuedFromSessionId,
      relationshipType: 'continuation',
    };
    if (typeof ts === 'string' && ts.length > 0) row.ts = ts;
    applyCodexSessionMetaProvenance(row, meta);
    rows.push(row);
  }
  return rows;
}

function applyCodexSessionMetaProvenance(
  row: SessionRelationshipRecord,
  meta: SessionMetaPayload | undefined,
): void {
  if (!meta) return;
  const sourceSessionId = stringField(meta, ['sourceSessionId', 'source_session_id']);
  if (sourceSessionId !== undefined) row.sourceSessionId = sourceSessionId;
  const sourceVersion = stringField(meta, ['cli_version', 'version']);
  if (sourceVersion !== undefined) row.sourceVersion = sourceVersion;
}

function stringField(obj: object, keys: ReadonlyArray<string>): string | undefined {
  const record = obj as Record<string, unknown>;
  for (const key of keys) {
    const value = record[key];
    if (typeof value === 'string' && value.length > 0) return value;
  }
  return undefined;
}

function codexRelationshipKey(row: SessionRelationshipRecord): string {
  return [
    row.source,
    row.sessionId,
    row.relationshipType,
    row.relatedSessionId ?? '',
    row.agentId ?? '',
    row.parentToolUseId ?? '',
  ].join('|');
}

// Build / refresh the SessionRelationshipRecord for a `spawn_agent` call once
// we know enough to make it useful. The row only carries weight when the
// spawned agent's id is known (so consumers can join child→parent), so we
// deliberately don't emit a placeholder when the id is still missing.
// Idempotent: `info.emitted` flips on the first commit so the second
// resolution path (output then notification, or vice versa) doesn't add a
// duplicate row.
function maybeEmitSpawnRelationship(
  openTurn: OpenTurn,
  sessionId: string,
  info: SpawnCallInfo,
  ts: string,
): void {
  if (info.emitted) return;
  if (!info.spawnedAgentId) return;
  const row: SessionRelationshipRecord = {
    v: 1,
    source: 'codex',
    sessionId: info.spawnedAgentId,
    relationshipType: 'subagent',
    relatedSessionId: sessionId,
    parentToolUseId: info.callId,
    agentId: info.spawnedAgentId,
  };
  if (info.subagentType !== undefined) row.subagentType = info.subagentType;
  if (info.description !== undefined) row.description = info.description;
  const stamp = ts || info.ts;
  if (stamp) row.ts = stamp;
  openTurn.pendingRelationships.push(row);
  info.emitted = true;
}

function pickStringField(
  obj: Record<string, unknown>,
  keys: ReadonlyArray<string>,
): string | undefined {
  for (const k of keys) {
    const v = obj[k];
    if (typeof v === 'string' && v.length > 0) return v;
  }
  return undefined;
}

// Best-effort extraction of a spawned agent / session id from a
// `function_call_output.output`. Codex output payloads vary by tool
// implementation — we look for common id-bearing fields whether the output
// is a JSON object, a JSON string, or a plain string we can parse.
function extractSpawnedAgentId(output: unknown): string | undefined {
  if (!output) return undefined;
  let obj: Record<string, unknown> | undefined;
  if (typeof output === 'object' && !Array.isArray(output)) {
    obj = output as Record<string, unknown>;
  } else if (typeof output === 'string') {
    try {
      const parsed = JSON.parse(output);
      if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
        obj = parsed as Record<string, unknown>;
      }
    } catch {
      return undefined;
    }
  }
  if (!obj) return undefined;
  return pickStringField(obj, ['agent_id', 'subagent_id', 'session_id']);
}

function measureToolOutput(output: unknown): { length?: number; hash?: string } {
  if (output === undefined || output === null) return {};
  if (typeof output === 'string') {
    return { length: output.length, hash: contentHash(output) };
  }
  try {
    const serialized = JSON.stringify(output);
    if (typeof serialized !== 'string') return {};
    return { length: serialized.length, hash: contentHash(serialized) };
  } catch {
    return {};
  }
}

// Codex `event_msg` notifications confirming a subagent has reached a
// terminal state. Names vary by rollout version (e.g. `subagent_message_complete`,
// `subagent_done`, `subagent_finished`); we accept any `subagent_*` event_msg
// type ending in `_complete` / `_done` / `_finished` / `_terminated` so the
// parser stays forward-compatible.
function isSubagentTerminalNotification(type: string): boolean {
  if (!type.startsWith('subagent_')) return false;
  return (
    type.endsWith('_complete') ||
    type.endsWith('_done') ||
    type.endsWith('_finished') ||
    type.endsWith('_terminated')
  );
}

function subagentNotificationStatus(note: SubagentNotificationPayload): ToolResultStatus {
  if (note.success === false) return 'errored';
  if (note.success === true) return 'completed';
  if (typeof note.status === 'string') {
    const s = note.status.toLowerCase();
    if (s === 'errored' || s === 'failed' || s === 'error') return 'errored';
    if (s === 'cancelled' || s === 'canceled') return 'cancelled';
    if (s === 'completed' || s === 'success' || s === 'succeeded') return 'completed';
  }
  // Notification fired but no explicit status field — treat as completed
  // since the notification kind itself implies a terminal transition.
  return 'completed';
}
