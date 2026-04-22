import { readFile, readdir } from 'node:fs/promises';
import * as path from 'node:path';

import { classifyActivity } from './classifier.js';
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
}

export async function parseOpencodeSession(
  sessionFilePath: string,
  options: ParseOpencodeOptions = {},
): Promise<ParseOpencodeResult> {
  const { turns, content } = await parseOpencodeSessionIncremental(sessionFilePath, options);
  return { turns, content };
}

export interface ParseOpencodeIncrementalOptions extends ParseOpencodeOptions {
  seenMessageIds?: ReadonlySet<string>;
}

export interface ParseOpencodeIncrementalResult {
  turns: TurnRecord[];
  content: ContentRecord[];
  seenMessageIds: Set<string>;
}

export async function parseOpencodeSessionIncremental(
  sessionFilePath: string,
  options: ParseOpencodeIncrementalOptions = {},
): Promise<ParseOpencodeIncrementalResult> {
  const session = await readSession(sessionFilePath);
  if (!session) {
    return { turns: [], content: [], seenMessageIds: new Set(options.seenMessageIds ?? []) };
  }

  const storageRoot = path.resolve(sessionFilePath, '..', '..', '..');
  const messages = await readMessages(storageRoot, session.id);
  const assistants = messages.filter(isCompleteAssistant);
  const users = messages.filter(isCompleteUser);
  assistants.sort((a, b) => a.time.created - b.time.created);
  users.sort((a, b) => a.time.created - b.time.created);

  const isSidechain = typeof session.parentID === 'string' && session.parentID.length > 0;
  const seen = new Set<string>(options.seenMessageIds ?? []);
  const turns: TurnRecord[] = [];

  for (let i = 0; i < assistants.length; i++) {
    const m = assistants[i]!;
    if (seen.has(m.id)) continue;
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
    });
    record.activity = classified.activity;
    record.retries = classified.retries;
    record.hasEdits = classified.hasEdits;

    turns.push(record);
    seen.add(m.id);
  }

  // TODO(#33-followup): content capture for OpenCode sessions. The result
  // shape carries a `content` array so appendContent wiring in the CLI stays
  // a no-op once capture lands.
  return { turns, content: [], seenMessageIds: seen };
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
