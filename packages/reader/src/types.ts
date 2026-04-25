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
  // The tool_use id of the Agent/Task call in the parent thread that spawned
  // this subagent. Set on every turn of the spawned subagent so all of its
  // turns share a single parent link.
  parentToolUseId?: string;
  // Stable id for this subagent invocation. For Claude sessions reconstructed
  // from JSONL alone, this is the uuid of the subagent's first user message
  // (the one with no parent inside the sidechain) — all turns of the same
  // invocation share it.
  agentId?: string;
  // agentId of the parent subagent, or the sessionId when the parent is the
  // main thread (for first-level subagents). Together with agentId this forms
  // a parent→child tree.
  parentAgentId?: string;
  // The `subagent_type` field from the spawning Agent/Task tool input.
  subagentType?: string;
  // The `description` field from the spawning Agent/Task tool input.
  description?: string;
}

// Granularity of the upstream usage data backing this record. Distinguishes
// per-turn token reporting from coarser shapes that some collectors are
// limited to (e.g. session-only aggregates, cost-only bills).
export type UsageGranularity =
  | 'per-turn'
  | 'per-message'
  | 'per-session-aggregate'
  | 'cost-only';

// Availability flags — strictly about whether the upstream source supplies a
// field, not about its numeric value. `hasOutputTokens: false` means "we do
// not know output tokens for this record"; it never means "0 output tokens".
// Numeric fields on `Usage` carry a `0` when the source actually reports zero
// or when it doesn't report the field at all; the matching coverage flag is
// the only honest way to tell those apart.
export interface Coverage {
  hasInputTokens: boolean;
  hasOutputTokens: boolean;
  hasReasoningTokens: boolean;
  hasCacheReadTokens: boolean;
  hasCacheCreateTokens: boolean;
  hasToolCalls: boolean;
  hasToolResultEvents: boolean;
  hasSessionRelationships: boolean;
  hasRawContent: boolean;
}

// Higher-level summary derived from `granularity` + `coverage`. Provided for
// command convenience; downstream code that needs to gate behavior on a
// specific field should still read `coverage` directly.
export type FidelityClass =
  | 'full'
  | 'usage-only'
  | 'aggregate-only'
  | 'cost-only'
  | 'partial';

export interface Fidelity {
  granularity: UsageGranularity;
  coverage: Coverage;
  // Convenience classification — derivable from granularity + coverage via
  // `classifyFidelity`. Stored on the record so JSON consumers don't need to
  // import the analyze helpers.
  class: FidelityClass;
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
  // Optional coverage / fidelity metadata. Absent on records emitted by older
  // ledger writers (pre-issue #41); commands should treat absence as
  // "unknown — best-effort full fidelity" rather than rejecting the record.
  fidelity?: Fidelity;
}

// Per-user-turn block info, recorded between assistant turns. Lets attribution
// recover per-tool-call cost as a delta against the next assistant turn's
// `usage.input` / `cacheRead` numbers — the API never reports usage at the
// `tool_use` granularity, but the size each `tool_result` contributed to
// context is enough to allocate the input-side delta across the calls that
// caused it. See issue #2.
//
// One `UserTurnRecord` per user line; `blocks` lists the individual content
// blocks that line carried (one entry per `tool_result` block plus any
// free-text the user typed). `precedingMessageId` and `followingMessageId`
// place the user turn between two assistant turns in the parser's emit order
// so consumers don't have to re-derive ordering from `parentUuid` chains.
export interface UserTurnBlock {
  // 'tool_result' for blocks returning to the model after a tool call;
  // 'text' for plain user input or harness-injected text blocks.
  kind: 'tool_result' | 'text';
  // The `tool_use.id` this result is for (only set when kind === 'tool_result').
  toolUseId?: string;
  // Byte length of the block's content as it would be serialized into the
  // request — `JSON.stringify`'d when content is structured, raw UTF-8 length
  // when it's a plain string.
  byteLen: number;
  // Cheap heuristic (`Math.ceil(byteLen / 4)`) suitable for proportional
  // allocation across tool calls within a user turn. Not a tokenizer; callers
  // that need accuracy can re-tokenize from the content sidecar.
  approxTokens: number;
  // True iff the source carried `is_error: true` for this tool_result.
  isError?: boolean;
}

export interface UserTurnRecord {
  v: 1;
  source: SourceKind;
  sessionId: string;
  // Stable per-line id (the JSONL `uuid` for Claude). Lets consumers dedupe
  // and reference a specific user turn without juggling `parentUuid` chains.
  userUuid: string;
  ts: string;
  // Message id of the assistant turn immediately before this user turn in
  // the session log. Absent for the first user turn of a session.
  precedingMessageId?: string;
  // Message id of the assistant turn this user turn fed into. Absent when
  // the user turn is trailing (no completed assistant turn after it yet).
  followingMessageId?: string;
  blocks: UserTurnBlock[];
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
