import { createReadStream } from 'node:fs';
import { open } from 'node:fs/promises';
import { createInterface } from 'node:readline';

import { resolveProject } from './git.js';
import { argsHash } from './hash.js';
import type {
  ContentRecord,
  ContentStoreMode,
  Subagent,
  ToolCall,
  TurnRecord,
  Usage,
} from './types.js';

interface AssistantLine {
  type: 'assistant';
  message: {
    id: string;
    model?: string;
    content?: ContentBlock[];
    stop_reason?: string | null;
    usage?: ClaudeUsage;
  };
  sessionId?: string;
  timestamp?: string;
  cwd?: string;
  isSidechain?: boolean;
  uuid?: string;
  parentUuid?: string | null;
}

type ContentBlock =
  | { type: 'text'; text?: string }
  | { type: 'thinking'; thinking?: string }
  | {
      type: 'tool_use';
      id?: string;
      name?: string;
      input?: Record<string, unknown>;
    }
  | { type: string; [k: string]: unknown };

interface ClaudeUsage {
  input_tokens?: number;
  output_tokens?: number;
  cache_read_input_tokens?: number;
  cache_creation_input_tokens?: number;
  cache_creation?: {
    ephemeral_5m_input_tokens?: number;
    ephemeral_1h_input_tokens?: number;
  };
}

interface UserLine {
  type: 'user';
  message?: {
    role?: string;
    content?:
      | string
      | Array<
          | { type: 'tool_result'; tool_use_id?: string; content?: unknown; is_error?: boolean }
          | { type: string; [k: string]: unknown }
        >;
  };
  isSidechain?: boolean;
  sessionId?: string;
  timestamp?: string;
  cwd?: string;
  uuid?: string;
}

interface WorkingRecord {
  messageId: string;
  firstTs: string;
  model: string;
  sessionId: string;
  cwd?: string;
  isSidechain: boolean;
  usage: Usage;
  blocks: ContentBlock[];
  stopReason?: string;
  parentAssistantUuid?: string;
}

export interface ParseOptions {
  sessionPath?: string;
  contentMode?: ContentStoreMode;
}

export interface ParseResult {
  turns: TurnRecord[];
  content: ContentRecord[];
}

export async function parseClaudeSession(
  filePath: string,
  options: ParseOptions = {},
): Promise<ParseResult> {
  const contentMode = options.contentMode ?? 'off';
  const captureContent = contentMode === 'full';
  const working = new Map<string, WorkingRecord>();
  const order: string[] = [];
  const subagentByToolUseId = new Map<
    string,
    { type?: string; taskDescription?: string }
  >();
  const userContent: ContentRecord[] = [];

  const rl = createInterface({
    input: createReadStream(filePath, { encoding: 'utf8' }),
    crlfDelay: Infinity,
  });

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
      const rec = parsed as Record<string, unknown>;

      if (rec.type === 'assistant') {
        ingestAssistant(rec as unknown as AssistantLine, working, order);
      } else if (rec.type === 'user') {
        const ul = rec as unknown as UserLine;
        captureSubagentFromToolResult(ul, subagentByToolUseId);
        if (captureContent) {
          for (const c of extractUserContent(ul)) userContent.push(c);
        }
      }
    }
  } finally {
    rl.close();
  }

  const turns: TurnRecord[] = [];
  const content: ContentRecord[] = captureContent ? [...userContent] : [];
  for (let i = 0; i < order.length; i++) {
    const id = order[i]!;
    const w = working.get(id);
    if (!w) continue;
    const toolCalls = extractToolCalls(w.blocks);
    const filesTouched = extractFilesTouched(toolCalls);
    const subagent = resolveSubagent(w, toolCalls, subagentByToolUseId);

    const record: TurnRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: w.sessionId,
      messageId: w.messageId,
      turnIndex: i,
      ts: w.firstTs,
      model: w.model,
      usage: w.usage,
      toolCalls,
    };
    if (options.sessionPath !== undefined) record.sessionPath = options.sessionPath;
    if (w.cwd !== undefined) {
      const resolved = resolveProject(w.cwd);
      record.project = resolved.project;
      if (resolved.projectKey !== undefined) record.projectKey = resolved.projectKey;
    }
    if (filesTouched.length > 0) record.filesTouched = filesTouched;
    if (subagent) record.subagent = subagent;
    if (w.stopReason !== undefined) record.stopReason = w.stopReason;
    turns.push(record);

    if (captureContent) {
      for (const c of extractAssistantContent(w)) content.push(c);
    }
  }
  return { turns, content };
}

function ingestAssistant(
  line: AssistantLine,
  working: Map<string, WorkingRecord>,
  order: string[],
): void {
  const msg = line.message;
  if (!msg || typeof msg.id !== 'string') return;
  const messageId = msg.id;

  let w = working.get(messageId);
  if (!w) {
    w = {
      messageId,
      firstTs: line.timestamp ?? '',
      model: msg.model ?? '',
      sessionId: line.sessionId ?? '',
      isSidechain: line.isSidechain === true,
      usage: toUsage(msg.usage),
      blocks: [],
    };
    if (line.cwd !== undefined) w.cwd = line.cwd;
    if (line.parentUuid) w.parentAssistantUuid = line.parentUuid;
    working.set(messageId, w);
    order.push(messageId);
  } else {
    if (line.isSidechain === true) w.isSidechain = true;
    if (!w.model && msg.model) w.model = msg.model;
  }
  if (typeof msg.stop_reason === 'string') w.stopReason = msg.stop_reason;
  if (Array.isArray(msg.content)) {
    for (const block of msg.content) w.blocks.push(block);
  }
}

function captureSubagentFromToolResult(
  line: UserLine,
  into: Map<string, { type?: string; taskDescription?: string }>,
): void {
  const content = line.message?.content;
  if (!Array.isArray(content)) return;
  for (const block of content) {
    if (block && typeof block === 'object' && block.type === 'tool_result') {
      const tr = block as { tool_use_id?: string };
      if (typeof tr.tool_use_id === 'string' && !into.has(tr.tool_use_id)) {
        into.set(tr.tool_use_id, {});
      }
    }
  }
}

function extractAssistantContent(w: WorkingRecord): ContentRecord[] {
  const out: ContentRecord[] = [];
  if (!w.sessionId || !w.messageId) return out;
  const ts = w.firstTs;
  for (const block of w.blocks) {
    if (!block || typeof block !== 'object') continue;
    if (block.type === 'text') {
      const b = block as { text?: string };
      if (typeof b.text === 'string' && b.text.length > 0) {
        out.push({
          v: 1,
          source: 'claude-code',
          sessionId: w.sessionId,
          messageId: w.messageId,
          ts,
          role: 'assistant',
          kind: 'text',
          text: b.text,
        });
      }
    } else if (block.type === 'thinking') {
      const b = block as { thinking?: string };
      if (typeof b.thinking === 'string' && b.thinking.length > 0) {
        out.push({
          v: 1,
          source: 'claude-code',
          sessionId: w.sessionId,
          messageId: w.messageId,
          ts,
          role: 'assistant',
          kind: 'thinking',
          text: b.thinking,
        });
      }
    } else if (block.type === 'tool_use') {
      const b = block as { id?: string; name?: string; input?: Record<string, unknown> };
      if (typeof b.id === 'string' && typeof b.name === 'string') {
        out.push({
          v: 1,
          source: 'claude-code',
          sessionId: w.sessionId,
          messageId: w.messageId,
          ts,
          role: 'assistant',
          kind: 'tool_use',
          toolUse: { id: b.id, name: b.name, input: b.input ?? {} },
        });
      }
    }
  }
  return out;
}

function extractUserContent(line: UserLine): ContentRecord[] {
  const out: ContentRecord[] = [];
  const sessionId = line.sessionId;
  const messageId = line.uuid;
  const ts = line.timestamp ?? '';
  if (!sessionId || !messageId) return out;
  const body = line.message?.content;
  if (typeof body === 'string') {
    if (body.length > 0) {
      out.push({
        v: 1,
        source: 'claude-code',
        sessionId,
        messageId,
        ts,
        role: 'user',
        kind: 'text',
        text: body,
      });
    }
    return out;
  }
  if (!Array.isArray(body)) return out;
  for (const block of body) {
    if (!block || typeof block !== 'object') continue;
    if (block.type === 'tool_result') {
      const tr = block as { tool_use_id?: string; content?: unknown; is_error?: boolean };
      if (typeof tr.tool_use_id !== 'string') continue;
      const record: ContentRecord = {
        v: 1,
        source: 'claude-code',
        sessionId,
        messageId,
        ts,
        role: 'tool_result',
        kind: 'tool_result',
        toolResult: { toolUseId: tr.tool_use_id, content: tr.content ?? '' },
      };
      if (tr.is_error === true) record.toolResult!.isError = true;
      out.push(record);
    } else if (block.type === 'text') {
      const b = block as { text?: string };
      if (typeof b.text === 'string' && b.text.length > 0) {
        out.push({
          v: 1,
          source: 'claude-code',
          sessionId,
          messageId,
          ts,
          role: 'user',
          kind: 'text',
          text: b.text,
        });
      }
    }
  }
  return out;
}

function toUsage(u: ClaudeUsage | undefined): Usage {
  const input = u?.input_tokens ?? 0;
  const output = u?.output_tokens ?? 0;
  const cacheRead = u?.cache_read_input_tokens ?? 0;
  const create5m = u?.cache_creation?.ephemeral_5m_input_tokens ?? 0;
  const create1h = u?.cache_creation?.ephemeral_1h_input_tokens ?? 0;
  const totalCreate = u?.cache_creation_input_tokens ?? 0;
  if (create5m === 0 && create1h === 0 && totalCreate > 0) {
    return { input, output, reasoning: 0, cacheRead, cacheCreate5m: totalCreate, cacheCreate1h: 0 };
  }
  return { input, output, reasoning: 0, cacheRead, cacheCreate5m: create5m, cacheCreate1h: create1h };
}

function extractToolCalls(blocks: ContentBlock[]): ToolCall[] {
  const out: ToolCall[] = [];
  const seen = new Set<string>();
  for (const b of blocks) {
    if (!b || b.type !== 'tool_use') continue;
    const tu = b as { id?: string; name?: string; input?: Record<string, unknown> };
    if (typeof tu.id !== 'string' || typeof tu.name !== 'string') continue;
    if (seen.has(tu.id)) continue;
    seen.add(tu.id);
    const input = tu.input ?? {};
    const call: ToolCall = {
      id: tu.id,
      name: tu.name,
      argsHash: argsHash(input),
    };
    const target = pickTarget(tu.name, input);
    if (target !== undefined) call.target = target;
    out.push(call);
  }
  return out;
}

function pickTarget(name: string, input: Record<string, unknown>): string | undefined {
  const s = (k: string): string | undefined => {
    const v = input[k];
    return typeof v === 'string' ? v : undefined;
  };
  switch (name) {
    case 'Read':
    case 'Edit':
    case 'Write':
    case 'NotebookEdit':
      return s('file_path');
    case 'Bash':
      return s('command');
    case 'Grep':
      return s('pattern');
    case 'Glob':
      return s('pattern');
    case 'WebFetch':
      return s('url');
    case 'Agent':
    case 'Task':
      return s('subagent_type') ?? s('description');
    default:
      return s('file_path') ?? s('path') ?? s('url') ?? s('command');
  }
}

function extractFilesTouched(toolCalls: ToolCall[]): string[] {
  const files = new Set<string>();
  for (const tc of toolCalls) {
    if (!tc.target) continue;
    if (tc.name === 'Read' || tc.name === 'Edit' || tc.name === 'Write' || tc.name === 'NotebookEdit') {
      files.add(tc.target);
    }
  }
  return [...files];
}

function resolveSubagent(
  w: WorkingRecord,
  _toolCalls: ToolCall[],
  _subagentIndex: Map<string, { type?: string; taskDescription?: string }>,
): Subagent | undefined {
  if (w.isSidechain) {
    return { isSidechain: true };
  }
  return undefined;
}

export interface ParseIncrementalOptions extends ParseOptions {
  startOffset?: number;
}

export interface ParseIncrementalResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  endOffset: number;
}

export async function parseClaudeSessionIncremental(
  filePath: string,
  options: ParseIncrementalOptions = {},
): Promise<ParseIncrementalResult> {
  const startOffset = options.startOffset ?? 0;
  const contentMode = options.contentMode ?? 'off';
  const captureContent = contentMode === 'full';
  const handle = await open(filePath, 'r');
  let buf: Buffer;
  let size: number;
  try {
    const st = await handle.stat();
    size = st.size;
    if (startOffset >= size) {
      return { turns: [], content: [], endOffset: startOffset };
    }
    const length = size - startOffset;
    buf = Buffer.allocUnsafe(length);
    await handle.read(buf, 0, length, startOffset);
  } finally {
    await handle.close();
  }

  const working = new Map<string, WorkingRecord>();
  const order: string[] = [];
  const subagentByToolUseId = new Map<
    string,
    { type?: string; taskDescription?: string }
  >();
  const messageIdFirstOffset = new Map<string, number>();
  // Track user-line content records along with their line start offsets so we
  // can include only those fully within the committed range.
  const pendingUserContent: Array<{ offset: number; record: ContentRecord }> = [];

  let p = 0;
  let cursorOffset = startOffset; // position just past the last complete \n
  while (p < buf.length) {
    const nlIdx = buf.indexOf(0x0a, p);
    if (nlIdx === -1) break;
    const lineStartOffset = startOffset + p;
    const lineEndOffset = startOffset + nlIdx + 1;
    const text = buf.subarray(p, nlIdx).toString('utf8').trim();
    p = nlIdx + 1;
    cursorOffset = lineEndOffset;
    if (!text) continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      continue;
    }
    if (!parsed || typeof parsed !== 'object') continue;
    const rec = parsed as Record<string, unknown>;
    if (rec.type === 'assistant') {
      const line = rec as unknown as AssistantLine;
      const msgId =
        line.message && typeof line.message.id === 'string' ? line.message.id : undefined;
      if (msgId && !messageIdFirstOffset.has(msgId)) {
        messageIdFirstOffset.set(msgId, lineStartOffset);
      }
      ingestAssistant(line, working, order);
    } else if (rec.type === 'user') {
      const ul = rec as unknown as UserLine;
      captureSubagentFromToolResult(ul, subagentByToolUseId);
      if (captureContent) {
        for (const c of extractUserContent(ul)) {
          pendingUserContent.push({ offset: lineStartOffset, record: c });
        }
      }
    }
  }

  // Determine end offset: the byte position of the earliest in-progress messageId,
  // or `cursorOffset` (= pos after last complete newline) if all messages are complete.
  let earliestIncompleteOffset: number | undefined;
  for (const id of order) {
    const w = working.get(id);
    if (!w) continue;
    if (w.stopReason === undefined) {
      const firstOff = messageIdFirstOffset.get(id);
      if (firstOff !== undefined) {
        if (earliestIncompleteOffset === undefined || firstOff < earliestIncompleteOffset) {
          earliestIncompleteOffset = firstOff;
        }
      }
    }
  }
  const endOffset = earliestIncompleteOffset ?? cursorOffset;

  const turns: TurnRecord[] = [];
  const content: ContentRecord[] = [];
  if (captureContent) {
    for (const { offset, record } of pendingUserContent) {
      if (offset < endOffset) content.push(record);
    }
  }
  for (let i = 0; i < order.length; i++) {
    const id = order[i]!;
    const w = working.get(id);
    if (!w) continue;
    if (w.stopReason === undefined) continue; // defer in-progress messages
    const toolCalls = extractToolCalls(w.blocks);
    const filesTouched = extractFilesTouched(toolCalls);
    const subagent = resolveSubagent(w, toolCalls, subagentByToolUseId);
    const record: TurnRecord = {
      v: 1,
      source: 'claude-code',
      sessionId: w.sessionId,
      messageId: w.messageId,
      turnIndex: i,
      ts: w.firstTs,
      model: w.model,
      usage: w.usage,
      toolCalls,
    };
    if (options.sessionPath !== undefined) record.sessionPath = options.sessionPath;
    if (w.cwd !== undefined) {
      const resolved = resolveProject(w.cwd);
      record.project = resolved.project;
      if (resolved.projectKey !== undefined) record.projectKey = resolved.projectKey;
    }
    if (filesTouched.length > 0) record.filesTouched = filesTouched;
    if (subagent) record.subagent = subagent;
    record.stopReason = w.stopReason;
    turns.push(record);
    if (captureContent) {
      for (const c of extractAssistantContent(w)) content.push(c);
    }
  }

  return { turns, content, endOffset };
}
