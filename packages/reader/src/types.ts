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
  usage: Usage;
  toolCalls: ToolCall[];
  filesTouched?: string[];
  subagent?: Subagent;
  stopReason?: string;
}
