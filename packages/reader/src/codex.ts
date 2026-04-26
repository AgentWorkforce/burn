import { open } from 'node:fs/promises';

import { classifyActivity } from './classifier.js';
import { resolveProject } from './git.js';
import { argsHash, contentHash } from './hash.js';
import type {
  ContentRecord,
  ContentStoreMode,
  SessionRelationshipRecord,
  ToolCall,
  ToolResultEventSource,
  ToolResultEventRecord,
  ToolResultStatus,
  TurnRecord,
  Usage,
  UserTurnBlock,
  UserTurnRecord,
} from './types.js';
import { makeTextBlock, makeToolResultBlock } from './userTurn.js';

export interface ParseCodexOptions {
  sessionPath?: string;
  contentMode?: ContentStoreMode;
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
  // and the subsequent task_complete commits the record. Issue #81.
  userTurnSlot?: PersistedUserTurnSlot;
  graph?: PersistedCodexGraphState;
}

export interface PersistedUserTurnSlot {
  blocks: UserTurnBlock[];
  precedingMessageId?: string;
  ts: string;
}

export interface PersistedCodexSpawnCall {
  parentToolUseId: string;
  agentId?: string;
  subagentSessionId?: string;
  subagentType?: string;
  description?: string;
}

export interface PersistedCodexGraphState {
  nextEventIndex: number;
  toolResultCounters: Record<string, number>;
  seenRootSessionIds: string[];
  spawnCalls?: Record<string, PersistedCodexSpawnCall>;
}

export interface ParseCodexIncrementalResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  userTurns: UserTurnRecord[];
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
  sourceSession?: string;
  source_session?: string;
  forkSessionId?: string;
  fork_session_id?: string;
  forkedFromSessionId?: string;
  forked_from_session_id?: string;
  continuedFromSessionId?: string;
  continued_from_session_id?: string;
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
}

interface FinalizedTurn
  extends Omit<
    OpenTurn,
    'startCumulative' | 'seenCallIds' | 'filesTouched' | 'erroredCallIds'
  > {
  usage: Usage;
  filesTouched: string[];
  erroredCallIds: Set<string>;
}

interface UserTurnSlot {
  blocks: UserTurnBlock[];
  precedingMessageId?: string;
  ts: string;
}

export interface ParseCodexResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  userTurns: UserTurnRecord[];
  relationships: SessionRelationshipRecord[];
  toolResultEvents: ToolResultEventRecord[];
}

export async function parseCodexSession(
  filePath: string,
  options: ParseCodexOptions = {},
): Promise<ParseCodexResult> {
  const { turns, content, userTurns, relationships, toolResultEvents } =
    await parseCodexSessionIncremental(filePath, {
      ...options,
      startOffset: 0,
    });
  return { turns, content, userTurns, relationships, toolResultEvents };
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
  // See issue #81.
  let userTurnSlot: UserTurnSlot = options.resume?.userTurnSlot
    ? cloneSlot(options.resume.userTurnSlot)
    : { blocks: [], ts: '' };
  const userTurns: UserTurnRecord[] = [];

  const graphState = cloneGraphState(options.resume?.graph);
  let nextEventIndex = graphState.nextEventIndex;
  const toolResultCounters = new Map<string, number>();
  for (const [k, v] of Object.entries(graphState.toolResultCounters)) {
    if (Number.isFinite(v)) toolResultCounters.set(k, v);
  }
  const seenRootSessionIds = new Set(graphState.seenRootSessionIds);
  const spawnCalls = new Map<string, PersistedCodexSpawnCall>();
  if (graphState.spawnCalls) {
    for (const [k, v] of Object.entries(graphState.spawnCalls)) spawnCalls.set(k, { ...v });
  }
  const relationships: Array<{ offset: number; record: SessionRelationshipRecord }> = [];
  const seenRelationshipIds = new Set<string>();
  const toolResultEvents: Array<{ offset: number; record: ToolResultEventRecord }> = [];
  const eventsByCallId = new Map<string, ToolResultEventRecord[]>();

  // Commit snapshot — only advanced at task_complete boundaries.
  let committedEndOffset = startOffset;
  let committedCumulative: CumulativeUsage = { ...cumulative };
  let committedSessionId = sessionId;
  let committedSessionCwd = sessionCwd;
  let committedTurnContexts = new Map(turnContexts);
  let committedFinalizedCount = 0;
  let committedUserTurnsCount = 0;
  let committedUserTurnSlot: UserTurnSlot = cloneSlot(userTurnSlot);
  let committedGraphState: PersistedCodexGraphState = snapshotGraphState(
    nextEventIndex,
    toolResultCounters,
    seenRootSessionIds,
    spawnCalls,
  );

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
      if (typeof sp.id === 'string') {
        sessionId = sp.id;
        collectSessionRelationships(
          sp,
          rec.timestamp,
          lineEndOffset,
          relationships,
          seenRelationshipIds,
          seenRootSessionIds,
        );
      }
      if (typeof sp.cwd === 'string') {
        sessionCwd = sp.cwd;
        if (openTurn && openTurn.project === undefined) openTurn.project = sp.cwd;
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
          for (const callId of openTurn.erroredCallIds) {
            markCodexToolResultStatus(eventsByCallId, callId, 'errored');
          }
          // Stamp preceding so the next task_started knows this turn closed
          // off the slot and the record can be linked.
          userTurnSlot.precedingMessageId = openTurn.turnId;
          finalized.push(finalizeTurn(openTurn, cumulative));
          openTurn = null;
          committedEndOffset = lineEndOffset;
          committedCumulative = { ...cumulative };
          committedSessionId = sessionId;
          committedSessionCwd = sessionCwd;
          committedTurnContexts = new Map(turnContexts);
          committedFinalizedCount = finalized.length;
          committedUserTurnsCount = userTurns.length;
          committedUserTurnSlot = cloneSlot(userTurnSlot);
          committedGraphState = snapshotGraphState(
            nextEventIndex,
            toolResultCounters,
            seenRootSessionIds,
            spawnCalls,
          );
        }
        continue;
      }

      if (pl.type === 'patch_apply_end') {
        const ev = payload as PatchApplyEndPayload;
        if (!openTurn || ev.turn_id !== openTurn.turnId) continue;
        if (ev.success === false) {
          if (typeof ev.call_id === 'string') {
            openTurn.erroredCallIds.add(ev.call_id);
            markCodexToolResultStatus(eventsByCallId, ev.call_id, 'errored');
          }
          continue;
        }
        if (typeof ev.call_id === 'string') {
          markCodexToolResultStatus(eventsByCallId, ev.call_id, 'completed');
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
        if (typeof ev.call_id === 'string') {
          if (typeof ev.exit_code === 'number' && ev.exit_code !== 0) {
            openTurn.erroredCallIds.add(ev.call_id);
            markCodexToolResultStatus(eventsByCallId, ev.call_id, 'errored');
          } else if (typeof ev.exit_code === 'number') {
            markCodexToolResultStatus(eventsByCallId, ev.call_id, 'completed');
          }
        }
        continue;
      }

      const notification = collectCodexNotificationEvent(
        payload as Record<string, unknown>,
        sessionId,
        rec.timestamp,
        toolResultCounters,
        nextEventIndex,
      );
      if (notification) {
        nextEventIndex++;
        toolResultEvents.push({ offset: lineEndOffset, record: notification });
        rememberEvent(eventsByCallId, notification);
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
          // the previous and next assistant turn (issue #81).
          userTurnSlot.blocks.push(makeTextBlock(text));
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
        const callIndex = toolResultCounters.get(out.call_id) ?? 0;
        toolResultCounters.set(out.call_id, callIndex + 1);
        const eventInput: Parameters<typeof buildCodexToolResultEvent>[0] = {
          sessionId,
          toolUseId: out.call_id,
          callIndex,
          eventIndex: nextEventIndex++,
          ts: itemTs,
          output: out.output,
          status: 'completed',
        };
        if (openTurn?.turnId !== undefined) eventInput.messageId = openTurn.turnId;
        const event = buildCodexToolResultEvent(eventInput);
        const outcome = extractCodexSubagentOutcome(out.output);
        const spawn = spawnCalls.get(out.call_id);
        annotateCodexSubagentEvent(event, outcome, spawn);
        if (event.status === 'errored' && openTurn) openTurn.erroredCallIds.add(out.call_id);
        toolResultEvents.push({ offset: lineEndOffset, record: event });
        rememberEvent(eventsByCallId, event);
        collectCodexSubagentRelationship(
          relationships,
          seenRelationshipIds,
          sessionId,
          out.call_id,
          itemTs,
          lineEndOffset,
          outcome,
          spawn,
        );
        // Always capture the output as a UserTurnBlock for the slot bridging
        // the open turn (or its predecessor) and the next assistant turn —
        // attribution doesn't require contentMode and shouldn't pay its cost.
        // isError is filled in at task_complete using `erroredCallIds`, since
        // exec_command_end / patch_apply_end ordering relative to the output
        // payload isn't guaranteed.
        userTurnSlot.blocks.push(makeToolResultBlock(out.call_id, out.output));
        if (!userTurnSlot.ts && itemTs) userTurnSlot.ts = itemTs;
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
        const spawn = extractCodexSpawnCall(fc.name, fc.call_id, parsedArgs);
        if (spawn) {
          spawnCalls.set(fc.call_id, spawn);
          collectCodexSubagentRelationship(
            relationships,
            seenRelationshipIds,
            sessionId,
            fc.call_id,
            itemTs,
            lineEndOffset,
            spawn,
            spawn,
          );
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
        const parsedInput = safeParseJson(input);
        const spawn = extractCodexSpawnCall(ct.name, ct.call_id, parsedInput ?? { input });
        if (spawn) {
          spawnCalls.set(ct.call_id, spawn);
          collectCodexSubagentRelationship(
            relationships,
            seenRelationshipIds,
            sessionId,
            ct.call_id,
            itemTs,
            lineEndOffset,
            spawn,
            spawn,
          );
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
    graph: cloneGraphState(committedGraphState),
  };
  if (committedSessionCwd !== undefined) resume.sessionCwd = committedSessionCwd;

  const emittedUserTurns = userTurns.slice(0, committedUserTurnsCount);
  const emittedRelationships = relationships
    .filter((r) => r.offset < committedEndOffset)
    .map((r) => r.record);
  const emittedToolResultEvents = toolResultEvents
    .filter((e) => e.offset < committedEndOffset)
    .map((e) => e.record);

  return {
    turns,
    content,
    userTurns: emittedUserTurns,
    relationships: emittedRelationships,
    toolResultEvents: emittedToolResultEvents,
    endOffset: committedEndOffset,
    resume,
  };
}

function cloneGraphState(g: PersistedCodexGraphState | undefined): PersistedCodexGraphState {
  if (!g) {
    return {
      nextEventIndex: 0,
      toolResultCounters: {},
      seenRootSessionIds: [],
      spawnCalls: {},
    };
  }
  const out: PersistedCodexGraphState = {
    nextEventIndex: Number.isFinite(g.nextEventIndex) ? g.nextEventIndex : 0,
    toolResultCounters: { ...g.toolResultCounters },
    seenRootSessionIds: [...g.seenRootSessionIds],
    spawnCalls: {},
  };
  if (g.spawnCalls) {
    out.spawnCalls = Object.fromEntries(
      Object.entries(g.spawnCalls).map(([k, v]) => [k, { ...v }]),
    );
  }
  return out;
}

function snapshotGraphState(
  nextEventIndex: number,
  counters: Map<string, number>,
  seenRootSessionIds: Set<string>,
  spawnCalls: Map<string, PersistedCodexSpawnCall>,
): PersistedCodexGraphState {
  return {
    nextEventIndex,
    toolResultCounters: Object.fromEntries(counters),
    seenRootSessionIds: [...seenRootSessionIds],
    spawnCalls: Object.fromEntries(
      [...spawnCalls].map(([k, v]) => [k, { ...v }]),
    ),
  };
}

function collectSessionRelationships(
  meta: SessionMetaPayload,
  fallbackTs: string | undefined,
  offset: number,
  out: Array<{ offset: number; record: SessionRelationshipRecord }>,
  seenRelationships: Set<string>,
  seenRootSessionIds: Set<string>,
): void {
  if (typeof meta.id !== 'string' || meta.id.length === 0) return;
  const ts = nonEmpty(meta.timestamp) ?? nonEmpty(fallbackTs);
  const sourceVersion = nonEmpty(meta.cli_version) ?? nonEmpty(meta.version);
  if (!seenRootSessionIds.has(meta.id)) {
    seenRootSessionIds.add(meta.id);
    const root: SessionRelationshipRecord = {
      v: 1,
      source: 'codex',
      sessionId: meta.id,
      relationshipType: 'root',
    };
    if (ts !== undefined) root.ts = ts;
    if (sourceVersion !== undefined) root.sourceVersion = sourceVersion;
    pushCodexRelationship(out, seenRelationships, root, offset);
  }

  const forkSource =
    nonEmpty(meta.forkSessionId) ??
    nonEmpty(meta.fork_session_id) ??
    nonEmpty(meta.forkedFromSessionId) ??
    nonEmpty(meta.forked_from_session_id);
  const continuationSource =
    nonEmpty(meta.continuedFromSessionId) ??
    nonEmpty(meta.continued_from_session_id) ??
    nonEmpty(meta.sourceSessionId) ??
    nonEmpty(meta.source_session_id) ??
    nonEmpty(meta.sourceSession) ??
    nonEmpty(meta.source_session);
  const relatedSessionId = forkSource ?? continuationSource;
  if (relatedSessionId === undefined || relatedSessionId === meta.id) return;
  const rel: SessionRelationshipRecord = {
    v: 1,
    source: 'codex',
    sessionId: meta.id,
    relatedSessionId,
    relationshipType: forkSource !== undefined ? 'fork' : 'continuation',
    sourceSessionId: relatedSessionId,
  };
  if (ts !== undefined) rel.ts = ts;
  if (sourceVersion !== undefined) rel.sourceVersion = sourceVersion;
  pushCodexRelationship(out, seenRelationships, rel, offset);
}

function pushCodexRelationship(
  out: Array<{ offset: number; record: SessionRelationshipRecord }>,
  seen: Set<string>,
  record: SessionRelationshipRecord,
  offset: number,
): void {
  const key = [
    record.source,
    record.sessionId,
    record.relationshipType,
    record.relatedSessionId ?? '',
    record.agentId ?? '',
    record.parentToolUseId ?? '',
  ].join('|');
  if (seen.has(key)) return;
  seen.add(key);
  out.push({ offset, record });
}

function buildCodexToolResultEvent(input: {
  sessionId: string;
  messageId?: string;
  toolUseId: string;
  callIndex: number;
  eventIndex: number;
  ts: string;
  output: unknown;
  status: ToolResultStatus;
}): ToolResultEventRecord {
  const record: ToolResultEventRecord = {
    v: 1,
    source: 'codex',
    sessionId: input.sessionId,
    toolUseId: input.toolUseId,
    callIndex: input.callIndex,
    eventIndex: input.eventIndex,
    status: input.status,
    eventSource: 'function_call_output',
  };
  if (input.messageId !== undefined) record.messageId = input.messageId;
  if (input.ts) record.ts = input.ts;
  const measured = measureGraphContent(input.output);
  if (measured.length !== undefined) record.contentLength = measured.length;
  if (measured.hash !== undefined) record.contentHash = measured.hash;
  return record;
}

function rememberEvent(
  byCallId: Map<string, ToolResultEventRecord[]>,
  event: ToolResultEventRecord,
): void {
  const existing = byCallId.get(event.toolUseId);
  if (existing) existing.push(event);
  else byCallId.set(event.toolUseId, [event]);
}

function markCodexToolResultStatus(
  byCallId: Map<string, ToolResultEventRecord[]>,
  callId: string,
  status: ToolResultStatus,
): void {
  const events = byCallId.get(callId);
  if (!events) return;
  for (const ev of events) {
    if (ev.status === 'errored' && status !== 'errored') continue;
    ev.status = status;
    if (status === 'errored') ev.isError = true;
  }
}

interface CodexSubagentEvidence {
  parentToolUseId?: string;
  agentId?: string;
  subagentSessionId?: string;
  subagentType?: string;
  description?: string;
  status?: ToolResultStatus;
}

function extractCodexSpawnCall(
  toolName: string,
  callId: string,
  args: Record<string, unknown> | undefined,
): PersistedCodexSpawnCall | undefined {
  if (!isCodexSpawnTool(toolName)) return undefined;
  const input = args ?? {};
  const out: PersistedCodexSpawnCall = { parentToolUseId: callId };
  const agentId = pickDeepString(input, [
    'agentId',
    'agent_id',
    'subagentId',
    'subagent_id',
    'taskId',
    'task_id',
  ]);
  const sessionId = pickDeepString(input, [
    'sessionId',
    'session_id',
    'subagentSessionId',
    'subagent_session_id',
    'childSessionId',
    'child_session_id',
  ]);
  const subagentType = pickDeepString(input, ['subagent_type', 'subagentType', 'agentType']);
  const description = pickDeepString(input, ['description', 'title', 'name']);
  if (agentId !== undefined) out.agentId = agentId;
  if (sessionId !== undefined) out.subagentSessionId = sessionId;
  if (subagentType !== undefined) out.subagentType = subagentType;
  if (description !== undefined) out.description = description;
  return out;
}

function isCodexSpawnTool(name: string): boolean {
  const normalized = name.toLowerCase().replace(/[-\s]/g, '_');
  return (
    normalized === 'spawn_agent' ||
    normalized === 'spawn_subagent' ||
    normalized === 'subagent' ||
    normalized === 'agent' ||
    normalized === 'task'
  );
}

function extractCodexSubagentOutcome(output: unknown): CodexSubagentEvidence {
  const obj = objectFromUnknown(output);
  if (!obj) {
    const status = statusFromUnknown(output);
    return status ? { status } : {};
  }
  const parentToolUseId = pickDeepString(obj, [
    'parentToolUseID',
    'parentToolUseId',
    'parent_tool_use_id',
    'toolUseId',
    'tool_use_id',
    'spawnCallId',
    'spawn_call_id',
  ]);
  const agentId = pickDeepString(obj, [
    'agentId',
    'agent_id',
    'subagentId',
    'subagent_id',
    'taskId',
    'task_id',
  ]);
  const subagentSessionId = pickDeepString(obj, [
    'sessionId',
    'session_id',
    'subagentSessionId',
    'subagent_session_id',
    'childSessionId',
    'child_session_id',
  ]);
  const subagentType = pickDeepString(obj, ['subagent_type', 'subagentType', 'agentType']);
  const description = pickDeepString(obj, ['description', 'title', 'name']);
  const status = statusFromUnknown(obj);
  const out: CodexSubagentEvidence = {};
  if (parentToolUseId !== undefined) out.parentToolUseId = parentToolUseId;
  if (agentId !== undefined) out.agentId = agentId;
  if (subagentSessionId !== undefined) out.subagentSessionId = subagentSessionId;
  if (subagentType !== undefined) out.subagentType = subagentType;
  if (description !== undefined) out.description = description;
  if (status !== undefined) out.status = status;
  return out;
}

function annotateCodexSubagentEvent(
  event: ToolResultEventRecord,
  outcome: CodexSubagentEvidence,
  spawn: PersistedCodexSpawnCall | undefined,
): void {
  const status = outcome.status;
  if (status !== undefined) {
    event.status = status;
    if (status === 'errored') event.isError = true;
  }
  const subagentSessionId = outcome.subagentSessionId ?? spawn?.subagentSessionId;
  const agentId = outcome.agentId ?? spawn?.agentId ?? subagentSessionId;
  if (subagentSessionId !== undefined) event.subagentSessionId = subagentSessionId;
  if (agentId !== undefined) event.agentId = agentId;
}

function collectCodexSubagentRelationship(
  out: Array<{ offset: number; record: SessionRelationshipRecord }>,
  seen: Set<string>,
  parentSessionId: string,
  callId: string,
  ts: string,
  offset: number,
  outcome: CodexSubagentEvidence,
  spawn: PersistedCodexSpawnCall | undefined,
): void {
  if (!parentSessionId) return;
  const childSessionId =
    outcome.subagentSessionId ?? spawn?.subagentSessionId ?? outcome.agentId ?? spawn?.agentId;
  if (childSessionId === undefined || childSessionId.length === 0) return;
  const agentId = outcome.agentId ?? spawn?.agentId ?? childSessionId;
  const parentToolUseId = outcome.parentToolUseId ?? spawn?.parentToolUseId ?? callId;
  const row: SessionRelationshipRecord = {
    v: 1,
    source: 'codex',
    sessionId: childSessionId,
    relatedSessionId: parentSessionId,
    relationshipType: 'subagent',
    parentToolUseId,
    agentId,
  };
  if (ts) row.ts = ts;
  const subagentType = outcome.subagentType ?? spawn?.subagentType;
  const description = outcome.description ?? spawn?.description;
  if (subagentType !== undefined) row.subagentType = subagentType;
  if (description !== undefined) row.description = description;
  pushCodexRelationship(out, seen, row, offset);
}

function collectCodexNotificationEvent(
  payload: Record<string, unknown>,
  sessionId: string,
  ts: string | undefined,
  counters: Map<string, number>,
  eventIndex: number,
): ToolResultEventRecord | undefined {
  if (!sessionId) return undefined;
  const type = typeof payload['type'] === 'string' ? payload['type'] : '';
  const eventSource = classifyCodexNotificationSource(type);
  if (eventSource === undefined) return undefined;
  const toolUseId = pickDeepString(payload, [
    'toolUseId',
    'tool_use_id',
    'call_id',
    'callId',
    'parentToolUseID',
    'parentToolUseId',
    'parent_tool_use_id',
  ]);
  if (toolUseId === undefined) return undefined;
  const callIndex = counters.get(toolUseId) ?? 0;
  counters.set(toolUseId, callIndex + 1);
  const record: ToolResultEventRecord = {
    v: 1,
    source: 'codex',
    sessionId,
    toolUseId,
    callIndex,
    eventIndex,
    status: statusFromUnknown(payload) ?? 'unknown',
    eventSource,
  };
  if (ts !== undefined) record.ts = ts;
  const agentId = pickDeepString(payload, ['agentId', 'agent_id', 'taskId', 'task_id']);
  const subagentSessionId = pickDeepString(payload, [
    'sessionId',
    'session_id',
    'subagentSessionId',
    'subagent_session_id',
    'childSessionId',
    'child_session_id',
  ]);
  if (agentId !== undefined) record.agentId = agentId;
  if (subagentSessionId !== undefined && subagentSessionId !== sessionId) {
    record.subagentSessionId = subagentSessionId;
  }
  if (record.status === 'errored') record.isError = true;
  const content = pickNotificationContent(payload);
  const measured = measureGraphContent(content);
  if (measured.length !== undefined) record.contentLength = measured.length;
  if (measured.hash !== undefined) record.contentHash = measured.hash;
  return record;
}

function classifyCodexNotificationSource(type: string): ToolResultEventSource | undefined {
  const normalized = type.toLowerCase();
  if (normalized.includes('queue') || normalized.includes('enqueue')) return 'queue_event';
  if (normalized.includes('progress')) return 'progress_event';
  if (normalized.includes('subagent') || normalized.includes('agent')) {
    return 'subagent_notification';
  }
  return undefined;
}

function pickNotificationContent(payload: Record<string, unknown>): unknown {
  for (const key of ['content', 'message', 'output', 'result']) {
    if (Object.prototype.hasOwnProperty.call(payload, key)) return payload[key];
  }
  return undefined;
}

function objectFromUnknown(value: unknown): Record<string, unknown> | undefined {
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    return value as Record<string, unknown>;
  }
  if (typeof value !== 'string' || value.length === 0) return undefined;
  try {
    const parsed = JSON.parse(value);
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      return parsed as Record<string, unknown>;
    }
  } catch {
    return undefined;
  }
  return undefined;
}

function pickDeepString(obj: Record<string, unknown>, keys: string[]): string | undefined {
  for (const key of keys) {
    const direct = obj[key];
    if (typeof direct === 'string' && direct.length > 0) return direct;
  }
  for (const value of Object.values(obj)) {
    if (!value || typeof value !== 'object' || Array.isArray(value)) continue;
    const nested = pickDeepString(value as Record<string, unknown>, keys);
    if (nested !== undefined) return nested;
  }
  return undefined;
}

function statusFromUnknown(value: unknown): ToolResultStatus | undefined {
  if (typeof value === 'string') return normalizeToolResultStatus(value);
  if (!value || typeof value !== 'object' || Array.isArray(value)) return undefined;
  const obj = value as Record<string, unknown>;
  for (const key of ['status', 'state', 'result', 'terminalStatus', 'terminal_status']) {
    const status = normalizeToolResultStatus(obj[key]);
    if (status !== undefined) return status;
  }
  for (const nested of Object.values(obj)) {
    const status = statusFromUnknown(nested);
    if (status !== undefined) return status;
  }
  return undefined;
}

function normalizeToolResultStatus(value: unknown): ToolResultStatus | undefined {
  if (typeof value !== 'string') return undefined;
  const normalized = value.toLowerCase().replace(/[-\s]/g, '_');
  if (
    normalized === 'completed' ||
    normalized === 'complete' ||
    normalized === 'success' ||
    normalized === 'succeeded' ||
    normalized === 'done'
  ) {
    return 'completed';
  }
  if (
    normalized === 'error' ||
    normalized === 'errored' ||
    normalized === 'failed' ||
    normalized === 'failure'
  ) {
    return 'errored';
  }
  if (
    normalized === 'running' ||
    normalized === 'in_progress' ||
    normalized === 'queued' ||
    normalized === 'pending' ||
    normalized === 'started'
  ) {
    return 'running';
  }
  if (
    normalized === 'cancelled' ||
    normalized === 'canceled' ||
    normalized === 'aborted'
  ) {
    return 'cancelled';
  }
  return undefined;
}

function measureGraphContent(content: unknown): { length?: number; hash?: string } {
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

function nonEmpty(value: string | undefined): string | undefined {
  return typeof value === 'string' && value.length > 0 ? value : undefined;
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

function cloneResume(r: CodexResumeState | undefined): CodexResumeState {
  if (!r) {
    return {
      cumulative: { input: 0, output: 0, cacheRead: 0, reasoning: 0 },
      sessionId: '',
      turnContexts: {},
      userTurnSlot: { blocks: [], ts: '' },
      graph: cloneGraphState(undefined),
    };
  }
  const out: CodexResumeState = {
    cumulative: { ...r.cumulative },
    sessionId: r.sessionId,
    turnContexts: { ...r.turnContexts },
  };
  if (r.sessionCwd !== undefined) out.sessionCwd = r.sessionCwd;
  if (r.userTurnSlot) out.userTurnSlot = cloneSlot(r.userTurnSlot);
  else out.userTurnSlot = { blocks: [], ts: '' };
  out.graph = cloneGraphState(r.graph);
  return out;
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
// "start". Issue #81.
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
  };
  if (open.project !== undefined) out.project = open.project;
  return out;
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
