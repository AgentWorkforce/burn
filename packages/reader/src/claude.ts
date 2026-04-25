import { createReadStream } from 'node:fs';
import { open } from 'node:fs/promises';
import { createInterface } from 'node:readline';

import { classifyActivity } from './classifier.js';
import { resolveProject } from './git.js';
import { argsHash, contentHash } from './hash.js';
import type {
  CompactionEvent,
  ContentRecord,
  ContentStoreMode,
  Subagent,
  ToolCall,
  TurnRecord,
  Usage,
  UserTurnBlock,
  UserTurnRecord,
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
  parentUuid?: string | null;
}

// Line-level registry used to walk `parentUuid` chains across the session when
// resolving subagent invocation roots. Only assistant lines that carry an
// Agent/Task tool_use block are tagged with `agentToolUse`; that's the signal
// that a child user line with sidechain=true is the root of a new invocation —
// but only when the child user line is *not* the tool_result coming back from
// that spawn (continuation within the same invocation).
interface LineNode {
  uuid: string;
  parentUuid?: string;
  kind: 'user' | 'assistant';
  isSidechain: boolean;
  agentToolUse?: {
    id: string;
    subagentType?: string;
    description?: string;
  };
  // tool_use ids for which this user line carries a tool_result block. Used
  // to distinguish a subagent spawn (child is the initial prompt) from a
  // parent-thread continuation after the subagent completed (child is the
  // tool_result for the Agent/Task call).
  toolResultIds?: Set<string>;
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
  // uuid of the first assistant line carrying this messageId; used as the
  // starting point when walking parentUuid chains to resolve subagent roots.
  firstAssistantUuid?: string;
  parentAssistantUuid?: string;
}

export interface ParseOptions {
  sessionPath?: string;
  contentMode?: ContentStoreMode;
}

export interface ParseResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  events: CompactionEvent[];
  // Per-user-turn block info between assistant turns (issue #2). Ordered by
  // appearance in the session log; entries reference adjacent assistant turns
  // by `precedingMessageId` / `followingMessageId`.
  userTurns: UserTurnRecord[];
}

export async function parseClaudeSession(
  filePath: string,
  options: ParseOptions = {},
): Promise<ParseResult> {
  const contentMode = options.contentMode ?? 'off';
  const captureContent = contentMode === 'full';
  const working = new Map<string, WorkingRecord>();
  const order: string[] = [];
  const nodesByUuid = new Map<string, LineNode>();
  const invocationCache = new Map<string, InvocationInfo | null>();
  // Track content with a monotonic sequence tied to line-read order so user
  // and assistant records can be merged back into chronological order at the
  // end (one TurnRecord may span multiple lines; we key its assistant content
  // to the seq of its first appearance).
  const userPending: Array<{ seq: number; record: ContentRecord }> = [];
  const firstSeq = new Map<string, number>();
  const userTextByMessageId = new Map<string, string>();
  const erroredToolUseIds = new Set<string>();
  const events: CompactionEvent[] = [];
  // Per-user-turn block info between assistant turns. Captured in source order
  // so consumers can recover per-tool-call cost as a delta against the next
  // assistant turn's input/cacheRead numbers (issue #2).
  const userTurns: UserTurnRecord[] = [];
  // The user-turn record that hasn't yet been linked to its following
  // assistant turn — set when we read a user line, cleared when the next
  // assistant turn with a new messageId arrives.
  let pendingUserTurn: UserTurnRecord | undefined;
  // Track the most recent completed assistant messageId so a compact_boundary
  // system record can be anchored to the turn right before it.
  let lastAssistantMessageId: string | undefined;
  let currentUserText = '';
  let seq = 0;

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
        const al = rec as unknown as AssistantLine;
        const mid = al.message?.id;
        // Link any pending user-turn record to this assistant turn — but only
        // the first time we see a new messageId, so multi-line assistant
        // messages don't shadow the link. Once linked, the user turn is
        // closed off.
        if (typeof mid === 'string' && pendingUserTurn && !working.has(mid)) {
          pendingUserTurn.followingMessageId = mid;
          pendingUserTurn = undefined;
        }
        if (captureContent && typeof mid === 'string' && !firstSeq.has(mid)) {
          firstSeq.set(mid, seq);
        }
        if (typeof mid === 'string' && !userTextByMessageId.has(mid)) {
          userTextByMessageId.set(mid, currentUserText);
        }
        if (typeof mid === 'string') lastAssistantMessageId = mid;
        ingestAssistant(al, working, order, nodesByUuid);
      } else if (rec.type === 'user') {
        const ul = rec as unknown as UserLine;
        registerUserNode(ul, nodesByUuid);
        const prompt = extractPlainUserText(ul);
        if (prompt) currentUserText = prompt;
        collectErroredToolUseIds(ul, erroredToolUseIds);
        const userTurn = buildUserTurnRecord(ul, lastAssistantMessageId);
        if (userTurn) {
          userTurns.push(userTurn);
          pendingUserTurn = userTurn;
        }
        if (captureContent) {
          for (const c of extractUserContent(ul)) userPending.push({ seq, record: c });
        }
      } else if (rec.type === 'system' && rec['subtype'] === 'compact_boundary') {
        const sl = rec as { sessionId?: string; timestamp?: string };
        const sessionId = sl.sessionId ?? '';
        const ts = sl.timestamp ?? '';
        if (sessionId) {
          const ev: CompactionEvent = {
            v: 1,
            source: 'claude-code',
            sessionId,
            ts,
          };
          if (lastAssistantMessageId) ev.precedingMessageId = lastAssistantMessageId;
          events.push(ev);
        }
      }
      seq++;
    }
  } finally {
    rl.close();
  }

  const turns: TurnRecord[] = [];
  const assistantPending: Array<{ seq: number; sub: number; record: ContentRecord }> = [];
  for (let i = 0; i < order.length; i++) {
    const id = order[i]!;
    const w = working.get(id);
    if (!w) continue;
    const toolCalls = extractToolCalls(w.blocks, erroredToolUseIds);
    const filesTouched = extractFilesTouched(toolCalls);
    const subagent = resolveSubagent(w, nodesByUuid, invocationCache);

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
    applyClassification(record, w, userTextByMessageId, erroredToolUseIds);
    turns.push(record);

    if (captureContent) {
      const seqForMsg = firstSeq.get(w.messageId) ?? 0;
      extractAssistantContent(w).forEach((r, sub) => {
        // sub starts at 1 so user content at the same seq sorts before assistant
        assistantPending.push({ seq: seqForMsg, sub: sub + 1, record: r });
      });
    }
  }

  annotateCompactionEvents(events, turns);

  const content: ContentRecord[] = captureContent
    ? mergeContentByOrder(userPending, assistantPending)
    : [];
  return { turns, content, events, userTurns };
}

function mergeContentByOrder(
  userPending: Array<{ seq: number; record: ContentRecord }>,
  assistantPending: Array<{ seq: number; sub: number; record: ContentRecord }>,
): ContentRecord[] {
  const merged: Array<{ seq: number; sub: number; record: ContentRecord }> = [];
  for (const u of userPending) merged.push({ seq: u.seq, sub: 0, record: u.record });
  for (const a of assistantPending) merged.push(a);
  merged.sort((a, b) => a.seq - b.seq || a.sub - b.sub);
  return merged.map((m) => m.record);
}

function ingestAssistant(
  line: AssistantLine,
  working: Map<string, WorkingRecord>,
  order: string[],
  nodesByUuid?: Map<string, LineNode>,
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
    if (typeof line.uuid === 'string') w.firstAssistantUuid = line.uuid;
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
  registerAssistantNode(line, nodesByUuid);
}

function makeLineNode(
  line: { uuid?: string; parentUuid?: string | null; isSidechain?: boolean },
  kind: 'user' | 'assistant',
): LineNode | undefined {
  if (typeof line.uuid !== 'string') return undefined;
  const node: LineNode = {
    uuid: line.uuid,
    kind,
    isSidechain: line.isSidechain === true,
  };
  if (typeof line.parentUuid === 'string' && line.parentUuid.length > 0) {
    node.parentUuid = line.parentUuid;
  }
  return node;
}

function registerAssistantNode(
  line: AssistantLine,
  nodesByUuid?: Map<string, LineNode>,
): void {
  if (!nodesByUuid) return;
  const node = makeLineNode(line, 'assistant');
  if (!node) return;
  // Only the *first* Agent/Task tool_use in a line is captured — a single
  // assistant line typically carries a single content block, so this is
  // unambiguous in practice.
  if (Array.isArray(line.message?.content)) {
    for (const block of line.message!.content) {
      if (!block || typeof block !== 'object' || block.type !== 'tool_use') continue;
      const tu = block as { id?: string; name?: string; input?: Record<string, unknown> };
      if (tu.name !== 'Agent' && tu.name !== 'Task') continue;
      if (typeof tu.id !== 'string') continue;
      const input = tu.input ?? {};
      const subagentType = typeof input['subagent_type'] === 'string' ? (input['subagent_type'] as string) : undefined;
      const description = typeof input['description'] === 'string' ? (input['description'] as string) : undefined;
      const at: LineNode['agentToolUse'] = { id: tu.id };
      if (subagentType !== undefined) at.subagentType = subagentType;
      if (description !== undefined) at.description = description;
      node.agentToolUse = at;
      break;
    }
  }
  nodesByUuid.set(node.uuid, node);
}

function registerUserNode(
  line: UserLine,
  nodesByUuid?: Map<string, LineNode>,
): void {
  if (!nodesByUuid) return;
  const node = makeLineNode(line, 'user');
  if (!node) return;
  const body = line.message?.content;
  if (Array.isArray(body)) {
    for (const block of body) {
      if (!block || typeof block !== 'object' || block.type !== 'tool_result') continue;
      const tr = block as { tool_use_id?: string };
      if (typeof tr.tool_use_id !== 'string') continue;
      if (!node.toolResultIds) node.toolResultIds = new Set();
      node.toolResultIds.add(tr.tool_use_id);
    }
  }
  nodesByUuid.set(node.uuid, node);
}

// Light pre-scan of [0, endOffset) that registers LineNodes only — no turn
// emission, no classification, no content capture. Used by the incremental
// parser so resuming ingest can still resolve parentUuid chains that reach
// back before the resume point. Returns the messageId of the last assistant
// line seen so a resumed pass can anchor `precedingMessageId` on user turns
// whose preceding assistant was already ingested in a prior pass.
async function prescanNodes(
  filePath: string,
  endOffset: number,
  nodesByUuid: Map<string, LineNode>,
): Promise<{ lastAssistantMessageId?: string }> {
  if (endOffset <= 0) return {};
  const handle = await open(filePath, 'r');
  let buf: Buffer;
  try {
    const st = await handle.stat();
    const length = Math.min(endOffset, st.size);
    if (length <= 0) return {};
    buf = Buffer.allocUnsafe(length);
    await handle.read(buf, 0, length, 0);
  } finally {
    await handle.close();
  }
  let p = 0;
  let lastAssistantMessageId: string | undefined;
  while (p < buf.length) {
    const nlIdx = buf.indexOf(0x0a, p);
    if (nlIdx === -1) break;
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
    const rec = parsed as Record<string, unknown>;
    if (rec.type === 'assistant') {
      const al = rec as unknown as AssistantLine;
      registerAssistantNode(al, nodesByUuid);
      const mid = al.message?.id;
      if (typeof mid === 'string') lastAssistantMessageId = mid;
    } else if (rec.type === 'user') {
      registerUserNode(rec as unknown as UserLine, nodesByUuid);
    }
  }
  const out: { lastAssistantMessageId?: string } = {};
  if (lastAssistantMessageId !== undefined) out.lastAssistantMessageId = lastAssistantMessageId;
  return out;
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

// Convert a user line into a `UserTurnRecord` carrying one block per
// `tool_result` and one `text` block per free-text chunk. Returns undefined
// when the line lacks a session id or uuid (we'd have nothing to anchor it
// to), or carries no measurable blocks.
//
// Token estimate uses the `bytes/4` heuristic. The issue notes a tokenizer
// could be wired in later (cl100k via @dqbd/tiktoken) — measure first; for
// proportional allocation across blocks within the same user turn the
// heuristic is fine, the constant cancels.
function buildUserTurnRecord(
  line: UserLine,
  precedingMessageId: string | undefined,
): UserTurnRecord | undefined {
  const sessionId = line.sessionId;
  const userUuid = line.uuid;
  if (!sessionId || !userUuid) return undefined;
  const blocks = extractUserTurnBlocks(line);
  if (blocks.length === 0) return undefined;
  const record: UserTurnRecord = {
    v: 1,
    source: 'claude-code',
    sessionId,
    userUuid,
    ts: line.timestamp ?? '',
    blocks,
  };
  if (precedingMessageId !== undefined) record.precedingMessageId = precedingMessageId;
  return record;
}

function extractUserTurnBlocks(line: UserLine): UserTurnBlock[] {
  const out: UserTurnBlock[] = [];
  const body = line.message?.content;
  if (typeof body === 'string') {
    if (body.length > 0) out.push(makeTextBlock(body));
    return out;
  }
  if (!Array.isArray(body)) return out;
  for (const block of body) {
    if (!block || typeof block !== 'object') continue;
    if (block.type === 'tool_result') {
      const tr = block as { tool_use_id?: string; content?: unknown; is_error?: boolean };
      if (typeof tr.tool_use_id !== 'string') continue;
      const byteLen = measureContentBytes(tr.content);
      const b: UserTurnBlock = {
        kind: 'tool_result',
        toolUseId: tr.tool_use_id,
        byteLen,
        approxTokens: bytesToApproxTokens(byteLen),
      };
      if (tr.is_error === true) b.isError = true;
      out.push(b);
    } else if (block.type === 'text') {
      const tb = block as { text?: string };
      if (typeof tb.text === 'string' && tb.text.length > 0) out.push(makeTextBlock(tb.text));
    }
  }
  return out;
}

function makeTextBlock(text: string): UserTurnBlock {
  const byteLen = Buffer.byteLength(text, 'utf8');
  return { kind: 'text', byteLen, approxTokens: bytesToApproxTokens(byteLen) };
}

// Measures the wire-shape byte length of a `tool_result.content` value:
// a plain string is measured as UTF-8; structured content is JSON-stringified
// first (matching how it'd be serialized into the request body).
function measureContentBytes(content: unknown): number {
  if (content === undefined || content === null) return 0;
  if (typeof content === 'string') return Buffer.byteLength(content, 'utf8');
  try {
    return Buffer.byteLength(JSON.stringify(content), 'utf8');
  } catch {
    // Circular references, BigInts, etc. — fall back to a coerced string so
    // we still return a usable signal rather than zero.
    return Buffer.byteLength(String(content), 'utf8');
  }
}

function bytesToApproxTokens(byteLen: number): number {
  if (byteLen <= 0) return 0;
  return Math.ceil(byteLen / 4);
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

function extractToolCalls(
  blocks: ContentBlock[],
  erroredToolUseIds: Set<string>,
): ToolCall[] {
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
    if (erroredToolUseIds.has(tu.id)) call.isError = true;
    applyEditHashes(call, input);
    out.push(call);
  }
  return out;
}

function applyEditHashes(call: ToolCall, input: Record<string, unknown>): void {
  if (call.name === 'Edit' || call.name === 'NotebookEdit') {
    const oldStr = input['old_string'];
    const newStr = input['new_string'];
    if (typeof oldStr === 'string') call.editPreHash = contentHash(oldStr);
    if (typeof newStr === 'string') call.editPostHash = contentHash(newStr);
  } else if (call.name === 'Write') {
    const content = input['content'];
    if (typeof content === 'string') call.editPostHash = contentHash(content);
  }
}

function annotateCompactionEvents(
  events: CompactionEvent[],
  turns: TurnRecord[],
): void {
  if (events.length === 0) return;
  const byMessageId = new Map<string, TurnRecord>();
  for (const t of turns) byMessageId.set(t.messageId, t);
  for (const ev of events) {
    if (ev.precedingMessageId) {
      const t = byMessageId.get(ev.precedingMessageId);
      if (t) {
        ev.tokensBeforeCompact = t.usage.cacheRead;
      }
    }
  }
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
  nodesByUuid?: Map<string, LineNode>,
  invocationCache?: Map<string, InvocationInfo | null>,
): Subagent | undefined {
  if (!w.isSidechain) return undefined;
  const sub: Subagent = { isSidechain: true };
  if (!nodesByUuid || !w.firstAssistantUuid) return sub;
  const info = resolveInvocation(w.firstAssistantUuid, nodesByUuid, invocationCache);
  if (!info) return sub;
  sub.agentId = info.rootUuid;
  if (info.parentToolUseId !== undefined) {
    sub.parentToolUseId = info.parentToolUseId;
  }
  if (info.subagentType !== undefined) sub.subagentType = info.subagentType;
  if (info.description !== undefined) sub.description = info.description;
  if (info.parentAgentId !== undefined) {
    sub.parentAgentId = info.parentAgentId;
  } else {
    // First-level subagent: parent is the main thread. Use the session id as a
    // stable anchor so callers can build parent→child trees without a null
    // sentinel.
    sub.parentAgentId = w.sessionId;
  }
  return sub;
}

interface InvocationInfo {
  rootUuid: string;
  parentToolUseId?: string;
  subagentType?: string;
  description?: string;
  parentAgentId?: string;
}

// Walk the parentUuid chain starting from `startUuid` looking for the user
// line that is the root of a subagent invocation: a user message whose
// immediate parent is an assistant line carrying an Agent/Task tool_use block.
// Returns undefined if no such boundary is found before the chain runs out
// (e.g. partial/incremental data, or `startUuid` belongs to the main thread).
// `cache` memoizes results per startUuid so the recursive parent-invocation
// resolution doesn't re-walk the outer chain once per inner turn.
function resolveInvocation(
  startUuid: string,
  nodes: Map<string, LineNode>,
  cache?: Map<string, InvocationInfo | null>,
  depth = 0,
): InvocationInfo | undefined {
  // Cycle / pathological-data guard; real chains are shallow.
  if (depth > 64) return undefined;
  if (cache) {
    const cached = cache.get(startUuid);
    if (cached !== undefined) return cached ?? undefined;
  }
  let node = nodes.get(startUuid);
  // Guard against parentUuid cycles (A→B→A) in malformed JSONL — the depth
  // guard only covers recursion, not the while-loop walk.
  const visited = new Set<string>();
  while (node) {
    if (visited.has(node.uuid)) break;
    visited.add(node.uuid);
    const parent = node.parentUuid ? nodes.get(node.parentUuid) : undefined;
    if (!parent) break;
    if (
      node.kind === 'user' &&
      parent.kind === 'assistant' &&
      parent.agentToolUse &&
      !(node.toolResultIds && node.toolResultIds.has(parent.agentToolUse.id))
    ) {
      const out: InvocationInfo = { rootUuid: node.uuid };
      if (parent.agentToolUse.id) out.parentToolUseId = parent.agentToolUse.id;
      if (parent.agentToolUse.subagentType !== undefined) out.subagentType = parent.agentToolUse.subagentType;
      if (parent.agentToolUse.description !== undefined) out.description = parent.agentToolUse.description;
      if (parent.isSidechain) {
        const parentInvocation = resolveInvocation(parent.uuid, nodes, cache, depth + 1);
        if (parentInvocation) out.parentAgentId = parentInvocation.rootUuid;
      }
      if (cache) cache.set(startUuid, out);
      return out;
    }
    node = parent;
  }
  if (cache) cache.set(startUuid, null);
  return undefined;
}

function extractPlainUserText(line: UserLine): string | undefined {
  const body = line.message?.content;
  if (typeof body === 'string') {
    return body.length > 0 ? body : undefined;
  }
  if (!Array.isArray(body)) return undefined;
  const parts: string[] = [];
  for (const block of body) {
    if (!block || typeof block !== 'object') continue;
    if (block.type === 'text') {
      const b = block as { text?: string };
      if (typeof b.text === 'string' && b.text.length > 0) parts.push(b.text);
    }
  }
  return parts.length > 0 ? parts.join('\n') : undefined;
}

function collectErroredToolUseIds(line: UserLine, into: Set<string>): void {
  const body = line.message?.content;
  if (!Array.isArray(body)) return;
  for (const block of body) {
    if (!block || typeof block !== 'object' || block.type !== 'tool_result') continue;
    const tr = block as { tool_use_id?: string; is_error?: boolean };
    if (tr.is_error === true && typeof tr.tool_use_id === 'string') {
      into.add(tr.tool_use_id);
    }
  }
}

function applyClassification(
  record: TurnRecord,
  w: WorkingRecord,
  userTextByMessageId: Map<string, string>,
  erroredToolUseIds: Set<string>,
): void {
  const userText = userTextByMessageId.get(w.messageId) ?? '';
  const assistantText = extractAssistantTextForClassification(w.blocks);
  const text = [userText, assistantText].filter((s) => s.length > 0).join('\n');
  const hasFailedTool = record.toolCalls.some((tc) => erroredToolUseIds.has(tc.id));
  const result = classifyActivity({
    toolCalls: record.toolCalls,
    text,
    hasFailedTool,
    reasoningTokens: record.usage.reasoning,
  });
  record.activity = result.activity;
  record.retries = result.retries;
  record.hasEdits = result.hasEdits;
}

function extractAssistantTextForClassification(blocks: ContentBlock[]): string {
  const parts: string[] = [];
  for (const b of blocks) {
    if (!b || typeof b !== 'object') continue;
    if (b.type === 'text') {
      const tb = b as { text?: string };
      if (typeof tb.text === 'string' && tb.text.length > 0) parts.push(tb.text);
    }
  }
  return parts.join('\n');
}

export interface ParseIncrementalOptions extends ParseOptions {
  startOffset?: number;
  // The most recent user prompt text seen before `startOffset`. Classification
  // uses the user prompt for keyword refinement; when `endOffset` backs up to
  // an incomplete assistant line, the prompt that preceded it is before the
  // resume point and won't be re-read — so callers must carry it forward.
  lastUserText?: string;
}

export interface ParseIncrementalResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  events: CompactionEvent[];
  endOffset: number;
  // Carry forward to the next incremental call; see `lastUserText` option.
  lastUserText: string;
  // Per-user-turn block info between assistant turns (issue #2). Filtered by
  // `endOffset` like content/events so the next incremental pass re-reads any
  // bytes past the cursor without double-emitting.
  userTurns: UserTurnRecord[];
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
      return {
        turns: [],
        content: [],
        events: [],
        endOffset: startOffset,
        lastUserText: options.lastUserText ?? '',
        userTurns: [],
      };
    }
    const length = size - startOffset;
    buf = Buffer.allocUnsafe(length);
    await handle.read(buf, 0, length, startOffset);
  } finally {
    await handle.close();
  }

  const working = new Map<string, WorkingRecord>();
  const order: string[] = [];
  const nodesByUuid = new Map<string, LineNode>();
  const invocationCache = new Map<string, InvocationInfo | null>();
  // When resuming mid-file, populate nodesByUuid from the already-ingested
  // prefix so new sidechain turns can still resolve their invocation root via
  // parentUuid chains that point back before startOffset. Without this,
  // subagent tree fields come up empty on the primary incremental ingest path.
  // Also captures the last assistant messageId before the resume point so
  // user turns landing in this pass can record `precedingMessageId` even
  // when their preceding assistant was already ingested previously.
  let prescanLastAssistantMid: string | undefined;
  if (startOffset > 0) {
    const prescan = await prescanNodes(filePath, startOffset, nodesByUuid);
    prescanLastAssistantMid = prescan.lastAssistantMessageId;
  }
  const messageIdFirstOffset = new Map<string, number>();
  const userTextByMessageId = new Map<string, string>();
  const erroredToolUseIds = new Set<string>();
  const events: Array<{ offset: number; event: CompactionEvent }> = [];
  // Seeded from the prescan so user turns whose preceding assistant turn
  // lives before `startOffset` still get a `precedingMessageId`.
  let lastAssistantMessageId: string | undefined = prescanLastAssistantMid;
  // Seed from the prior call so an in-progress turn whose user prompt lives
  // before `startOffset` still classifies against that prompt on resume.
  let currentUserText = options.lastUserText ?? '';
  // User content tagged with the byte offset of its line so we can (a) drop
  // records past endOffset and (b) interleave them with assistant content by
  // source-order at emit time.
  const pendingUserContent: Array<{ offset: number; record: ContentRecord }> = [];
  // Per-user-turn records tagged with their line offset so we can drop any
  // that fall past endOffset (avoiding double-emission on resume), mirroring
  // the content/event handling.
  const pendingUserTurns: Array<{ offset: number; record: UserTurnRecord }> = [];
  let pendingUserTurnInc: UserTurnRecord | undefined;

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
      // Link any pending user-turn record to this assistant turn — first time
      // we see a new messageId only, so multi-line assistant messages don't
      // shadow the link.
      if (msgId && pendingUserTurnInc && !messageIdFirstOffset.has(msgId)) {
        pendingUserTurnInc.followingMessageId = msgId;
        pendingUserTurnInc = undefined;
      }
      if (msgId && !messageIdFirstOffset.has(msgId)) {
        messageIdFirstOffset.set(msgId, lineStartOffset);
      }
      if (msgId && !userTextByMessageId.has(msgId)) {
        userTextByMessageId.set(msgId, currentUserText);
      }
      if (msgId) lastAssistantMessageId = msgId;
      ingestAssistant(line, working, order, nodesByUuid);
    } else if (rec.type === 'user') {
      const ul = rec as unknown as UserLine;
      registerUserNode(ul, nodesByUuid);
      const prompt = extractPlainUserText(ul);
      if (prompt) currentUserText = prompt;
      collectErroredToolUseIds(ul, erroredToolUseIds);
      const userTurn = buildUserTurnRecord(ul, lastAssistantMessageId);
      if (userTurn) {
        pendingUserTurns.push({ offset: lineStartOffset, record: userTurn });
        pendingUserTurnInc = userTurn;
      }
      if (captureContent) {
        for (const c of extractUserContent(ul)) {
          pendingUserContent.push({ offset: lineStartOffset, record: c });
        }
      }
    } else if (rec.type === 'system' && rec['subtype'] === 'compact_boundary') {
      const sl = rec as { sessionId?: string; timestamp?: string };
      const sessionId = sl.sessionId ?? '';
      const ts = sl.timestamp ?? '';
      if (sessionId) {
        const ev: CompactionEvent = {
          v: 1,
          source: 'claude-code',
          sessionId,
          ts,
        };
        if (lastAssistantMessageId) ev.precedingMessageId = lastAssistantMessageId;
        events.push({ offset: lineStartOffset, event: ev });
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
  const assistantPending: Array<{ offset: number; sub: number; record: ContentRecord }> = [];
  for (let i = 0; i < order.length; i++) {
    const id = order[i]!;
    const w = working.get(id);
    if (!w) continue;
    if (w.stopReason === undefined) continue; // defer in-progress messages
    const toolCalls = extractToolCalls(w.blocks, erroredToolUseIds);
    const filesTouched = extractFilesTouched(toolCalls);
    const subagent = resolveSubagent(w, nodesByUuid, invocationCache);
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
    applyClassification(record, w, userTextByMessageId, erroredToolUseIds);
    turns.push(record);
    if (captureContent) {
      const msgOffset = messageIdFirstOffset.get(w.messageId) ?? 0;
      extractAssistantContent(w).forEach((r, sub) => {
        assistantPending.push({ offset: msgOffset, sub: sub + 1, record: r });
      });
    }
  }

  let content: ContentRecord[] = [];
  if (captureContent) {
    const merged: Array<{ offset: number; sub: number; record: ContentRecord }> = [];
    for (const u of pendingUserContent) {
      if (u.offset < endOffset) merged.push({ offset: u.offset, sub: 0, record: u.record });
    }
    // Filter assistant content by the same endOffset boundary. TurnRecords
    // past endOffset are still emitted (appendTurns dedups by messageId), but
    // appendContent has no dedup, so content emitted past endOffset would be
    // re-emitted and duplicated when the next incremental call resumes from
    // endOffset and re-processes the same bytes.
    for (const a of assistantPending) {
      if (a.offset < endOffset) merged.push(a);
    }
    merged.sort((a, b) => a.offset - b.offset || a.sub - b.sub);
    content = merged.map((m) => m.record);
  }

  // Only emit compaction events whose bytes fall before endOffset — mirrors
  // the content-dedup rule. appendCompactions does its own dedup by id hash,
  // but we still don't want to emit an event past endOffset and then re-emit
  // it on the next incremental pass.
  const emittedEvents: CompactionEvent[] = [];
  for (const e of events) {
    if (e.offset < endOffset) emittedEvents.push(e.event);
  }
  annotateCompactionEvents(emittedEvents, turns);

  // Emit user turns whose bytes fall before endOffset, same dedup discipline
  // as content/events. Trailing user turns past endOffset will be re-read on
  // the next incremental call.
  const emittedUserTurns: UserTurnRecord[] = [];
  for (const u of pendingUserTurns) {
    if (u.offset < endOffset) emittedUserTurns.push(u.record);
  }

  return {
    turns,
    content,
    events: emittedEvents,
    endOffset,
    lastUserText: currentUserText,
    userTurns: emittedUserTurns,
  };
}
