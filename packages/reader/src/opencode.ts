import { readFile, readdir } from 'node:fs/promises';
import * as path from 'node:path';

import { classifyActivity } from './classifier.js';
import { resolveProject } from './git.js';
import { argsHash, contentHash } from './hash.js';
import type {
  ContentRecord,
  ContentStoreMode,
  SessionRelationshipRecord,
  Subagent,
  ToolCall,
  ToolResultEventRecord,
  TurnRecord,
  Usage,
  UserTurnBlock,
  UserTurnRecord,
} from './types.js';
import { makeTextBlock, makeToolResultBlock } from './userTurn.js';

export interface ParseOpencodeOptions {
  sessionPath?: string;
  contentMode?: ContentStoreMode;
}

interface SessionInfo {
  id: string;
  parentID?: string;
  directory?: string;
}

interface MessageTokens {
  input?: number;
  output?: number;
  reasoning?: number;
  cache?: {
    read?: number;
    write?: number;
  };
}

interface AssistantMessage {
  id: string;
  sessionID: string;
  role: 'assistant';
  time: { created: number };
  providerID?: string;
  modelID?: string;
  path?: { cwd?: string };
  tokens?: MessageTokens;
}

interface ToolPart {
  type: 'tool';
  callID?: string;
  tool?: string;
  state?: {
    input?: Record<string, unknown>;
    status?: string;
    metadata?: { exit?: number; [k: string]: unknown };
    // Tool output, written by opencode once the tool completes. Typically a
    // string but can be structured for some tools; we pass it through to the
    // sidecar unchanged and let downstream stringify as needed.
    output?: unknown;
    [k: string]: unknown;
  };
}

interface StepFinishPart {
  type: 'step-finish';
  reason?: string;
  tokens?: MessageTokens;
}

interface TextPart {
  type: 'text';
  text?: string;
  synthetic?: boolean;
}

type Part =
  | ToolPart
  | StepFinishPart
  | TextPart
  | { type: string; [k: string]: unknown };

interface UserMessage {
  id: string;
  sessionID: string;
  role: 'user';
  time: { created: number };
}

export interface ParseOpencodeResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  userTurns: UserTurnRecord[];
  // Execution-graph substrate (#42 / #93). One `root` row per session, plus a
  // `subagent` row when the session payload carries a `parentID`. Always
  // present (possibly empty) so callers can pass directly to `appendRelationships`.
  relationships: SessionRelationshipRecord[];
  // Terminal-status tool-result events, one per tool part with a resolved
  // `state.output`. Status is derived from `state.status === 'error'` and
  // `metadata.exit !== 0`. Metadata-only — `contentLength` / `contentHash`
  // are computed from the stringified output; raw bytes are never stored.
  toolResultEvents: ToolResultEventRecord[];
}

export async function parseOpencodeSession(
  sessionFilePath: string,
  options: ParseOpencodeOptions = {},
): Promise<ParseOpencodeResult> {
  const { turns, content, userTurns, relationships, toolResultEvents } =
    await parseOpencodeSessionIncremental(sessionFilePath, options);
  return { turns, content, userTurns, relationships, toolResultEvents };
}

export interface ParseOpencodeIncrementalOptions extends ParseOpencodeOptions {
  seenMessageIds?: ReadonlySet<string>;
}

export interface ParseOpencodeIncrementalResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  userTurns: UserTurnRecord[];
  relationships: SessionRelationshipRecord[];
  toolResultEvents: ToolResultEventRecord[];
  seenMessageIds: Set<string>;
}

export async function parseOpencodeSessionIncremental(
  sessionFilePath: string,
  options: ParseOpencodeIncrementalOptions = {},
): Promise<ParseOpencodeIncrementalResult> {
  const session = await readSession(sessionFilePath);
  if (!session) {
    return {
      turns: [],
      content: [],
      userTurns: [],
      relationships: [],
      toolResultEvents: [],
      seenMessageIds: new Set(options.seenMessageIds ?? []),
    };
  }

  const storageRoot = path.resolve(sessionFilePath, '..', '..', '..');
  const messages = await readMessages(storageRoot, session.id);
  const assistants = messages.filter(isCompleteAssistant);
  const users = messages.filter(isCompleteUser);
  assistants.sort((a, b) => a.time.created - b.time.created);
  users.sort((a, b) => a.time.created - b.time.created);

  const captureContent = options.contentMode === 'full';
  const isSidechain = typeof session.parentID === 'string' && session.parentID.length > 0;
  const seen = new Set<string>(options.seenMessageIds ?? []);
  const turns: TurnRecord[] = [];
  const content: ContentRecord[] = [];
  const userTurns: UserTurnRecord[] = [];
  const toolResultEvents: ToolResultEventRecord[] = [];
  // Per-toolUseId callIndex counter — 0 for the first event for that id, 1
  // for the next, etc. Local to this parse pass; on resumed ingest the
  // writer's hash-based dedup is the source of truth.
  const callIndexCounters = new Map<string, number>();
  let nextEventIndex = 0;

  for (let i = 0; i < assistants.length; i++) {
    const m = assistants[i]!;
    if (seen.has(m.id)) continue;
    // Build the user turn that bridges the previous assistant message and
    // this one — tool outputs from the predecessor's parts plus any user
    // text from the user message that precedes `m`. Issue #86.
    // Emitted once per gap, on the pass when `m` is first processed; the
    // previous assistant may have been seen on an earlier pass, but its
    // parts are still on disk for re-reading.
    const prev = i > 0 ? assistants[i - 1]! : undefined;
    const userMsg = findPrecedingUser(users, m.time.created);
    // Don't re-attribute the same user message to two assistants — for
    // gaps after the first, only count the user message if its timestamp
    // is *after* the previous assistant's.
    const userMsgForGap =
      userMsg && (!prev || userMsg.time.created > prev.time.created) ? userMsg : undefined;
    const userTurn = await buildOpencodeUserTurnRecord(
      storageRoot,
      session.id,
      prev,
      m,
      userMsgForGap,
    );
    if (userTurn) userTurns.push(userTurn);

    const parts = await readParts(storageRoot, m.id);
    const { toolCalls, filesTouched, erroredCallIds } = extractToolsAndFiles(parts);
    const stopReason = lastStepFinishReason(parts);

    const model = buildModel(m.providerID, m.modelID);
    const project = m.path?.cwd ?? session.directory;
    const usage = toUsage(m.tokens);

    const record: TurnRecord = {
      v: 1,
      source: 'opencode',
      sessionId: m.sessionID,
      messageId: m.id,
      turnIndex: i,
      ts: new Date(m.time.created).toISOString(),
      model,
      usage,
      toolCalls,
    };
    if (options.sessionPath !== undefined) record.sessionPath = options.sessionPath;
    if (project !== undefined) {
      const resolved = resolveProject(project);
      record.project = resolved.project;
      if (resolved.projectKey !== undefined) record.projectKey = resolved.projectKey;
    }
    if (filesTouched.length > 0) record.filesTouched = filesTouched;
    if (isSidechain) {
      const sub: Subagent = { isSidechain: true };
      record.subagent = sub;
    }
    if (stopReason !== undefined) record.stopReason = stopReason;

    const userMessage = findPrecedingUser(users, m.time.created);
    const userText = userMessage ? await readUserText(storageRoot, userMessage.id) : '';
    const assistantText = extractAssistantText(parts);
    const cText = [userText, assistantText].filter((s) => s.length > 0).join('\n');
    const hasFailedTool = toolCalls.some((tc) => erroredCallIds.has(tc.id));
    const classified = classifyActivity({
      toolCalls,
      text: cText,
      hasFailedTool,
      reasoningTokens: usage.reasoning,
    });
    record.activity = classified.activity;
    record.retries = classified.retries;
    record.hasEdits = classified.hasEdits;

    turns.push(record);
    seen.add(m.id);

    // Execution graph (#42 / #93). One ToolResultEventRecord per tool part
    // with a resolved output, in part-id order. Status follows the same
    // failure rules as the existing erroredCallIds set: state.status='error'
    // OR a non-zero `metadata.exit` (bash-family tools).
    nextEventIndex = collectOpencodeToolResultEvents(
      parts,
      m.sessionID,
      m.id,
      new Date(m.time.created).toISOString(),
      toolResultEvents,
      callIndexCounters,
      nextEventIndex,
    );

    if (captureContent) {
      const assistantTs = new Date(m.time.created).toISOString();
      if (userMessage) {
        const userTs = new Date(userMessage.time.created).toISOString();
        for (const t of await readUserTextParts(storageRoot, userMessage.id)) {
          content.push({
            v: 1,
            source: 'opencode',
            sessionId: m.sessionID,
            messageId: userMessage.id,
            ts: userTs,
            role: 'user',
            kind: 'text',
            text: t,
          });
        }
      }
      for (const rec of extractAssistantContent(parts, m.sessionID, m.id, assistantTs)) {
        content.push(rec);
      }
    }
  }

  // Relationship rows are session-level state and always reflect the current
  // session payload (root for every session, plus a `subagent` row when
  // `parentID` is set). Emitted on every pass; the writer dedups by hash so
  // resumed ingest doesn't double-write. OpenCode session payloads don't
  // expose a stable spawning `callID` / subagent_type / description on the
  // child session itself today, so the subagent row carries `relatedSessionId`
  // (the parent session id) and nothing more — matches the Claude shape's
  // mandatory fields without inventing fields the source doesn't surface.
  const relationships = buildOpencodeRelationships(session, assistants);

  return { turns, content, userTurns, relationships, toolResultEvents, seenMessageIds: seen };
}

function collectOpencodeToolResultEvents(
  parts: Part[],
  sessionId: string,
  messageId: string,
  ts: string,
  out: ToolResultEventRecord[],
  callIndexCounters: Map<string, number>,
  startEventIndex: number,
): number {
  let nextIndex = startEventIndex;
  for (const p of parts) {
    if (p.type !== 'tool') continue;
    const tp = p as ToolPart;
    if (typeof tp.callID !== 'string' || tp.callID.length === 0) continue;
    const state = tp.state;
    if (!state || !Object.prototype.hasOwnProperty.call(state, 'output')) continue;
    const isError = isFailedTool(tp);
    const callIndex = callIndexCounters.get(tp.callID) ?? 0;
    callIndexCounters.set(tp.callID, callIndex + 1);
    const record: ToolResultEventRecord = {
      v: 1,
      source: 'opencode',
      sessionId,
      messageId,
      toolUseId: tp.callID,
      callIndex,
      eventIndex: nextIndex++,
      ts,
      status: isError ? 'errored' : 'completed',
      eventSource: 'tool_result',
    };
    if (isError) record.isError = true;
    const measured = measureOpencodeToolOutput(state.output);
    if (measured.length !== undefined) record.contentLength = measured.length;
    if (measured.hash !== undefined) record.contentHash = measured.hash;
    out.push(record);
  }
  return nextIndex;
}

function measureOpencodeToolOutput(output: unknown): { length?: number; hash?: string } {
  if (typeof output === 'string') {
    return { length: output.length, hash: contentHash(output) };
  }
  if (output === undefined || output === null) {
    return {};
  }
  try {
    const serialized = JSON.stringify(output);
    if (typeof serialized !== 'string') return {};
    return { length: serialized.length, hash: contentHash(serialized) };
  } catch {
    return {};
  }
}

function buildOpencodeRelationships(
  session: SessionInfo,
  assistants: AssistantMessage[],
): SessionRelationshipRecord[] {
  const out: SessionRelationshipRecord[] = [];
  // Earliest assistant ts is a reasonable witness for "when this session
  // first did anything"; absent assistants we leave ts unset.
  const firstTs =
    assistants.length > 0 ? new Date(assistants[0]!.time.created).toISOString() : undefined;
  const root: SessionRelationshipRecord = {
    v: 1,
    source: 'opencode',
    sessionId: session.id,
    relationshipType: 'root',
  };
  if (firstTs !== undefined) root.ts = firstTs;
  out.push(root);
  if (typeof session.parentID === 'string' && session.parentID.length > 0) {
    const sub: SessionRelationshipRecord = {
      v: 1,
      source: 'opencode',
      sessionId: session.id,
      relatedSessionId: session.parentID,
      relationshipType: 'subagent',
    };
    if (firstTs !== undefined) sub.ts = firstTs;
    out.push(sub);
  }
  return out;
}

// Build a UserTurnRecord for the gap between two consecutive assistant
// messages. Tool outputs come from the predecessor's parts (the harness's
// response to the previous assistant's tool calls); free-text comes from the
// user message preceding the following assistant. preceding is undef on the
// first assistant of the session. Returns undefined if the gap has no
// measurable blocks. Issue #86.
async function buildOpencodeUserTurnRecord(
  storageRoot: string,
  sessionId: string,
  prev: AssistantMessage | undefined,
  next: AssistantMessage,
  userMsg: UserMessage | undefined,
): Promise<UserTurnRecord | undefined> {
  const blocks: UserTurnBlock[] = [];

  // Prior assistant's tool outputs feed back into `next`'s input — the
  // harness wrote them between the two assistant turns.
  if (prev) {
    const prevParts = await readParts(storageRoot, prev.id);
    for (const p of prevParts) {
      if (p.type !== 'tool') continue;
      const tp = p as ToolPart;
      if (typeof tp.callID !== 'string') continue;
      const state = tp.state;
      if (!state || !Object.prototype.hasOwnProperty.call(state, 'output')) continue;
      const isError = isFailedTool(tp);
      blocks.push(makeToolResultBlock(tp.callID, state.output ?? '', isError));
    }
  }

  // User-typed text from the user message preceding `next`. Synthetic parts
  // are harness-injected (env context, agent-mode nudges) — they still flow
  // into the model's input, so they count toward attribution byte length
  // even though the activity classifier filters them out.
  let ts = userMsg ? new Date(userMsg.time.created).toISOString() : '';
  if (userMsg) {
    const userParts = await readParts(storageRoot, userMsg.id);
    for (const p of userParts) {
      if (p.type !== 'text') continue;
      const tp = p as TextPart;
      if (typeof tp.text === 'string' && tp.text.length > 0) blocks.push(makeTextBlock(tp.text));
    }
  }

  if (blocks.length === 0) return undefined;

  if (!ts) ts = new Date(next.time.created).toISOString();

  // userUuid: prefer the user message id when present (stable, OpenCode-native);
  // fall back to a synthesized id from the surrounding assistants when the gap
  // contains only tool outputs.
  const userUuid = userMsg
    ? userMsg.id
    : `${sessionId}:${prev?.id ?? 'start'}->${next.id}`;

  const record: UserTurnRecord = {
    v: 1,
    source: 'opencode',
    sessionId,
    userUuid,
    ts,
    blocks,
    followingMessageId: next.id,
  };
  if (prev) record.precedingMessageId = prev.id;
  return record;
}

function extractAssistantContent(
  parts: Part[],
  sessionId: string,
  messageId: string,
  ts: string,
): ContentRecord[] {
  const out: ContentRecord[] = [];
  for (const p of parts) {
    if (p.type === 'text') {
      const tp = p as TextPart;
      if (tp.synthetic === true) continue;
      if (typeof tp.text === 'string' && tp.text.length > 0) {
        out.push({
          v: 1,
          source: 'opencode',
          sessionId,
          messageId,
          ts,
          role: 'assistant',
          kind: 'text',
          text: tp.text,
        });
      }
      continue;
    }
    if (p.type === 'tool') {
      const tp = p as ToolPart;
      if (typeof tp.callID !== 'string' || typeof tp.tool !== 'string') continue;
      const input = tp.state?.input ?? {};
      out.push({
        v: 1,
        source: 'opencode',
        sessionId,
        messageId,
        ts,
        role: 'assistant',
        kind: 'tool_use',
        toolUse: { id: tp.callID, name: tp.tool, input },
      });
      const state = tp.state;
      if (state && Object.prototype.hasOwnProperty.call(state, 'output')) {
        const result: ContentRecord = {
          v: 1,
          source: 'opencode',
          sessionId,
          messageId,
          ts,
          role: 'tool_result',
          kind: 'tool_result',
          toolResult: { toolUseId: tp.callID, content: state.output ?? '' },
        };
        if (state.status === 'error') result.toolResult!.isError = true;
        else {
          const exit = state.metadata?.exit;
          if (typeof exit === 'number' && exit !== 0) result.toolResult!.isError = true;
        }
        out.push(result);
      }
    }
  }
  return out;
}

async function readUserTextParts(storageRoot: string, userMessageId: string): Promise<string[]> {
  const parts = await readParts(storageRoot, userMessageId);
  const out: string[] = [];
  for (const p of parts) {
    if (p.type !== 'text') continue;
    const tp = p as TextPart;
    if (tp.synthetic === true) continue;
    if (typeof tp.text === 'string' && tp.text.length > 0) out.push(tp.text);
  }
  return out;
}

async function readSession(sessionFilePath: string): Promise<SessionInfo | null> {
  try {
    const raw = await readFile(sessionFilePath, 'utf8');
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    if (typeof parsed.id !== 'string') return null;
    const out: SessionInfo = { id: parsed.id };
    if (typeof parsed.parentID === 'string') out.parentID = parsed.parentID;
    if (typeof parsed.directory === 'string') out.directory = parsed.directory;
    return out;
  } catch {
    return null;
  }
}

async function readMessages(
  storageRoot: string,
  sessionId: string,
): Promise<Array<AssistantMessage | UserMessage | { role: string; id: string }>> {
  const dir = path.join(storageRoot, 'message', sessionId);
  let entries: string[];
  try {
    entries = await readdir(dir);
  } catch {
    return [];
  }
  const out: Array<AssistantMessage | UserMessage | { role: string; id: string }> = [];
  for (const name of entries) {
    if (!name.endsWith('.json')) continue;
    const full = path.join(dir, name);
    try {
      const raw = await readFile(full, 'utf8');
      const parsed = JSON.parse(raw) as Record<string, unknown>;
      const role = parsed.role;
      const id = parsed.id;
      if (typeof role !== 'string' || typeof id !== 'string') continue;
      out.push(parsed as unknown as AssistantMessage | UserMessage | { role: string; id: string });
    } catch {
      continue;
    }
  }
  return out;
}

async function readParts(storageRoot: string, messageId: string): Promise<Part[]> {
  const dir = path.join(storageRoot, 'part', messageId);
  let entries: string[];
  try {
    entries = await readdir(dir);
  } catch {
    return [];
  }
  const parts: Array<Part & { id?: string }> = [];
  for (const name of entries) {
    if (!name.endsWith('.json')) continue;
    try {
      const raw = await readFile(path.join(dir, name), 'utf8');
      const parsed = JSON.parse(raw) as Part & { id?: string };
      parts.push(parsed);
    } catch {
      continue;
    }
  }
  // prt_* ids have time-ordered base36 suffixes, so sorting by part.id keeps chronological order
  parts.sort((a, b) => ((a.id ?? '') < (b.id ?? '') ? -1 : (a.id ?? '') > (b.id ?? '') ? 1 : 0));
  return parts;
}

function extractToolsAndFiles(parts: Part[]): {
  toolCalls: ToolCall[];
  filesTouched: string[];
  erroredCallIds: Set<string>;
} {
  const toolCalls: ToolCall[] = [];
  const seen = new Set<string>();
  const files = new Set<string>();
  const erroredCallIds = new Set<string>();
  for (const p of parts) {
    if (p.type !== 'tool') continue;
    const tp = p as ToolPart;
    if (typeof tp.callID !== 'string' || typeof tp.tool !== 'string') continue;
    if (seen.has(tp.callID)) continue;
    seen.add(tp.callID);
    const input = tp.state?.input ?? {};
    const call: ToolCall = {
      id: tp.callID,
      name: tp.tool,
      argsHash: argsHash(input),
    };
    const target = pickTarget(tp.tool, input);
    if (target !== undefined) call.target = target;
    toolCalls.push(call);
    if (target !== undefined && isFileTool(tp.tool)) files.add(target);
    if (isFailedTool(tp)) erroredCallIds.add(tp.callID);
  }
  return { toolCalls, filesTouched: [...files], erroredCallIds };
}

function isFailedTool(tp: ToolPart): boolean {
  const state = tp.state;
  if (!state) return false;
  if (state.status === 'error') return true;
  // For bash-family tools, a non-zero exit code in metadata is a failure
  // even though state.status is reported as 'completed'.
  const exit = state.metadata?.exit;
  if (typeof exit === 'number' && exit !== 0) return true;
  return false;
}

function extractAssistantText(parts: Part[]): string {
  const chunks: string[] = [];
  for (const p of parts) {
    if (p.type !== 'text') continue;
    const tp = p as TextPart;
    if (tp.synthetic === true) continue;
    if (typeof tp.text === 'string' && tp.text.length > 0) chunks.push(tp.text);
  }
  return chunks.join('\n');
}

function findPrecedingUser(users: UserMessage[], tsCreated: number): UserMessage | undefined {
  let best: UserMessage | undefined;
  for (const u of users) {
    if (u.time.created <= tsCreated) best = u;
    else break;
  }
  return best;
}

async function readUserText(storageRoot: string, userMessageId: string): Promise<string> {
  const parts = await readParts(storageRoot, userMessageId);
  const chunks: string[] = [];
  for (const p of parts) {
    if (p.type !== 'text') continue;
    const tp = p as TextPart;
    // Skip harness-injected prompts (agent-mode nudges, etc.) — they'd bias
    // classification toward whatever the injection talks about rather than
    // the user's real intent.
    if (tp.synthetic === true) continue;
    if (typeof tp.text === 'string' && tp.text.length > 0) chunks.push(tp.text);
  }
  return chunks.join('\n');
}

function isCompleteUser(m: { role: string; id: string }): m is UserMessage {
  if (m.role !== 'user') return false;
  const u = m as Partial<UserMessage>;
  return (
    typeof u.sessionID === 'string' &&
    typeof u.time?.created === 'number' &&
    Number.isFinite(u.time.created)
  );
}

function pickTarget(name: string, input: Record<string, unknown>): string | undefined {
  const s = (k: string): string | undefined => {
    const v = input[k];
    return typeof v === 'string' ? v : undefined;
  };
  switch (name) {
    case 'read':
    case 'write':
    case 'edit':
      return s('filePath') ?? s('file_path') ?? s('path');
    case 'bash':
      return s('command');
    case 'grep':
      return s('pattern');
    case 'glob':
      return s('pattern');
    case 'webfetch':
      return s('url');
    case 'task':
      return s('subagent_type') ?? s('description') ?? s('prompt');
    default:
      return s('filePath') ?? s('file_path') ?? s('path') ?? s('url') ?? s('command');
  }
}

function isFileTool(name: string): boolean {
  return name === 'read' || name === 'write' || name === 'edit';
}

function lastStepFinishReason(parts: Part[]): string | undefined {
  for (let i = parts.length - 1; i >= 0; i--) {
    const p = parts[i]!;
    if (p.type === 'step-finish') {
      const sf = p as StepFinishPart;
      if (typeof sf.reason === 'string') return sf.reason;
    }
  }
  return undefined;
}

function toUsage(t: MessageTokens | undefined): Usage {
  const input = t?.input ?? 0;
  const output = t?.output ?? 0;
  const reasoning = t?.reasoning ?? 0;
  const cacheRead = t?.cache?.read ?? 0;
  const cacheWrite = t?.cache?.write ?? 0;
  return {
    input,
    output,
    reasoning,
    cacheRead,
    cacheCreate5m: cacheWrite,
    cacheCreate1h: 0,
  };
}

function buildModel(providerID: string | undefined, modelID: string | undefined): string {
  if (providerID && modelID) return `${providerID}/${modelID}`;
  return modelID ?? providerID ?? '';
}

function isCompleteAssistant(m: { role: string; id: string }): m is AssistantMessage {
  if (m.role !== 'assistant') return false;
  const a = m as Partial<AssistantMessage>;
  return (
    typeof a.sessionID === 'string' &&
    typeof a.time?.created === 'number' &&
    Number.isFinite(a.time.created)
  );
}
