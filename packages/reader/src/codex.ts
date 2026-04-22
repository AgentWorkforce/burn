import { open } from 'node:fs/promises';

import { resolveProject } from './git.js';
import { argsHash } from './hash.js';
import type { ToolCall, TurnRecord, Usage } from './types.js';

export interface ParseCodexOptions {
  sessionPath?: string;
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
}

export interface ParseCodexIncrementalResult {
  turns: TurnRecord[];
  endOffset: number;
  resume: CodexResumeState;
}

interface SessionMetaPayload {
  id?: string;
  cwd?: string;
  timestamp?: string;
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
  success?: boolean;
  changes?: Record<string, unknown>;
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
}

interface FinalizedTurn extends Omit<OpenTurn, 'startCumulative' | 'seenCallIds' | 'filesTouched'> {
  usage: Usage;
  filesTouched: string[];
}

export async function parseCodexSession(
  filePath: string,
  options: ParseCodexOptions = {},
): Promise<TurnRecord[]> {
  const { turns } = await parseCodexSessionIncremental(filePath, { ...options, startOffset: 0 });
  return turns;
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
  const finalized: FinalizedTurn[] = [];

  // Commit snapshot — only advanced at task_complete boundaries.
  let committedEndOffset = startOffset;
  let committedCumulative: CumulativeUsage = { ...cumulative };
  let committedSessionId = sessionId;
  let committedSessionCwd = sessionCwd;
  let committedTurnContexts = new Map(turnContexts);
  let committedFinalizedCount = 0;

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
      if (typeof sp.id === 'string') sessionId = sp.id;
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
        };
        if (project !== undefined) openTurn.project = project;
        continue;
      }

      if (pl.type === 'task_complete') {
        const ev = payload as TaskCompletePayload;
        if (openTurn && ev.turn_id === openTurn.turnId) {
          finalized.push(finalizeTurn(openTurn, cumulative));
          openTurn = null;
          committedEndOffset = lineEndOffset;
          committedCumulative = { ...cumulative };
          committedSessionId = sessionId;
          committedSessionCwd = sessionCwd;
          committedTurnContexts = new Map(turnContexts);
          committedFinalizedCount = finalized.length;
        }
        continue;
      }

      if (pl.type === 'patch_apply_end') {
        const ev = payload as PatchApplyEndPayload;
        if (!openTurn || ev.turn_id !== openTurn.turnId) continue;
        if (ev.success === false) continue;
        const changes = ev.changes;
        if (changes && typeof changes === 'object') {
          for (const file of Object.keys(changes)) openTurn.filesTouched.add(file);
        }
        continue;
      }
      continue;
    }

    if (rec.type === 'response_item') {
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
      }
    }
  }

  // Only emit turns committed up to the last task_complete boundary.
  const committed = finalized.slice(0, committedFinalizedCount);
  const turns: TurnRecord[] = [];
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
    turns.push(record);
  }

  const resume: CodexResumeState = {
    cumulative: { ...committedCumulative },
    sessionId: committedSessionId,
    turnContexts: Object.fromEntries(committedTurnContexts),
  };
  if (committedSessionCwd !== undefined) resume.sessionCwd = committedSessionCwd;

  return { turns, endOffset: committedEndOffset, resume };
}

function cloneResume(r: CodexResumeState | undefined): CodexResumeState {
  if (!r) {
    return {
      cumulative: { input: 0, output: 0, cacheRead: 0, reasoning: 0 },
      sessionId: '',
      turnContexts: {},
    };
  }
  const out: CodexResumeState = {
    cumulative: { ...r.cumulative },
    sessionId: r.sessionId,
    turnContexts: { ...r.turnContexts },
  };
  if (r.sessionCwd !== undefined) out.sessionCwd = r.sessionCwd;
  return out;
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
  };
  if (open.project !== undefined) out.project = open.project;
  return out;
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
