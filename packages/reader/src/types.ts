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
  // True iff the subsequent tool_result carried is_error=true. Absent when
  // the tool_result hasn't been seen (in-progress turn) or the source doesn't
  // surface an error flag.
  isError?: boolean;
  // For Edit tool calls: hashes of the pre-state (old_string) and post-state
  // (new_string). For Write tool calls: editPostHash only (hash of content).
  // Used by the edit-revert detector to spot cycles without storing raw
  // strings in the ledger.
  editPreHash?: string;
  editPostHash?: string;
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
  | 'review'
  | 'git'
  | 'build-deploy'
  | 'deps'
  | 'format'
  | 'verification'
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

// Emitted by session parsers when the agent harness performs context
// compaction (e.g. Claude Code's `compact_boundary` system marker). Anchored
// to the turn immediately preceding the compaction so detectors can price
// what was lost.
export interface CompactionEvent {
  v: 1;
  source: SourceKind;
  sessionId: string;
  ts: string;
  // Message id of the turn just before the compaction boundary, if known.
  precedingMessageId?: string;
  // Cache-read tokens on the preceding turn. That cached span is effectively
  // dead after compaction and will have to be rebuilt if the session
  // continues.
  tokensBeforeCompact?: number;
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
