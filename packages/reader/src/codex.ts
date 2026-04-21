import { createReadStream } from 'node:fs';
import { createInterface } from 'node:readline';

import { argsHash } from './hash.js';
import type { ToolCall, TurnRecord, Usage } from './types.js';

export interface ParseCodexOptions {
  sessionPath?: string;
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
  const rl = createInterface({
    input: createReadStream(filePath, { encoding: 'utf8' }),
    crlfDelay: Infinity,
  });

  let sessionId = '';
  let sessionCwd: string | undefined;
  const turnContexts = new Map<string, TurnContextPayload>();
  const cumulative: CumulativeUsage = { input: 0, output: 0, cacheRead: 0, reasoning: 0 };
  let openTurn: OpenTurn | null = null;
  const finalized: FinalizedTurn[] = [];

  try {
    for await (const line of rl) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      let parsed: unknown;
      try {
        parsed = JSON.parse(trimmed);
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
        const p = payload as SessionMetaPayload;
        if (typeof p.id === 'string') sessionId = p.id;
        if (typeof p.cwd === 'string') {
          sessionCwd = p.cwd;
          if (openTurn && openTurn.project === undefined) openTurn.project = p.cwd;
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

      const p = payload as { type?: string };

      if (rec.type === 'event_msg') {
        if (p.type === 'token_count') {
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

        if (p.type === 'task_started') {
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

        if (p.type === 'task_complete') {
          const ev = payload as TaskCompletePayload;
          if (openTurn && ev.turn_id === openTurn.turnId) {
            finalized.push(finalizeTurn(openTurn, cumulative));
            openTurn = null;
          }
          continue;
        }

        if (p.type === 'patch_apply_end') {
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
        if (p.type === 'function_call') {
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
        } else if (p.type === 'custom_tool_call') {
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
  } finally {
    rl.close();
  }

  if (openTurn) {
    finalized.push(finalizeTurn(openTurn, cumulative));
  }

  const turns: TurnRecord[] = [];
  for (let i = 0; i < finalized.length; i++) {
    const f = finalized[i]!;
    const record: TurnRecord = {
      v: 1,
      source: 'codex',
      sessionId,
      messageId: f.turnId,
      turnIndex: i,
      ts: f.ts,
      model: f.model,
      usage: f.usage,
      toolCalls: f.toolCalls,
    };
    if (options.sessionPath !== undefined) record.sessionPath = options.sessionPath;
    if (f.project !== undefined) record.project = f.project;
    if (f.filesTouched.length > 0) record.filesTouched = f.filesTouched;
    turns.push(record);
  }
  return turns;
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
