export type SourceKind =
  | 'claude-code'
  | 'codex'
  | 'opencode'
  | 'anthropic-api'
  | 'openai-api'
  | 'gemini-api';

export interface Usage {
  input: number;
  output: number;
  reasoning: number;
  cacheRead: number;
  cacheCreate5m: number;
  cacheCreate1h: number;
}

export interface ToolCall {
  id: string;
  name: string;
  target?: string;
  argsHash: string;
}

export interface Subagent {
  isSidechain: boolean;
  type?: string;
  taskDescription?: string;
  parentToolUseId?: string;
}

export type ActivityCategory =
  | 'planning'
  | 'delegation'
  | 'testing'
  | 'git'
  | 'build-deploy'
  | 'deps'
  | 'format'
  | 'coding'
  | 'docs'
  | 'debugging'
  | 'refactoring'
  | 'feature'
  | 'exploration'
  | 'reasoning'
  | 'brainstorming'
  | 'conversation';

export interface TurnRecord {
  v: 1;
  source: SourceKind;
  sessionId: string;
  sessionPath?: string;
  messageId: string;
  turnIndex: number;
  ts: string;
  model: string;
  project?: string;
  projectKey?: string;
  usage: Usage;
  toolCalls: ToolCall[];
  filesTouched?: string[];
  subagent?: Subagent;
  stopReason?: string;
  activity?: ActivityCategory;
  retries?: number;
  hasEdits?: boolean;
}

export type ContentRole = 'user' | 'assistant' | 'tool_result';
export type ContentKind = 'text' | 'thinking' | 'tool_use' | 'tool_result';

export interface ContentToolUse {
  id: string;
  name: string;
  input: Record<string, unknown>;
}

export interface ContentToolResult {
  toolUseId: string;
  // Tool results arrive in multiple shapes: plain string output, structured
  // objects (e.g. image blocks), arrays of blocks. We pass the value through
  // verbatim; downstream consumers narrow as needed.
  content: unknown;
  isError?: boolean;
}

export interface ContentRecord {
  v: 1;
  source: SourceKind;
  sessionId: string;
  messageId: string;
  ts: string;
  role: ContentRole;
  kind: ContentKind;
  text?: string;
  toolUse?: ContentToolUse;
  toolResult?: ContentToolResult;
}

export type ContentStoreMode = 'full' | 'hash-only' | 'off';
