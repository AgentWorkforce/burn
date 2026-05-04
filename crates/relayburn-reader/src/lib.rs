//! Rust port of `@relayburn/reader`. See AgentWorkforce/burn#242.
//!
//! This crate is a work-in-progress port of the TS reader package. Foundational
//! modules (`types`, `hash`, `fidelity`, `git`, `classifier`, `user_turn`) are
//! ported with native conformance tests; the `codex` parser (#256) and the
//! `opencode_stream` ingestor (#258) are ported; the Claude Code parser
//! (`claude`) is in progress under #255 — synchronous `parse_claude_session`
//! and the cross-file reconciler are landed, the incremental entry point is
//! not yet ported. The remaining per-harness parser (`opencode`) is
//! scaffolded but not yet implemented — see #257.

pub mod classifier;
pub mod fidelity;
pub mod git;
pub mod hash;
pub mod types;
pub mod user_turn;

pub mod claude;
pub mod codex;
pub mod opencode;
pub mod opencode_stream;

pub use codex::{
    parse_codex_session, parse_codex_session_incremental, read_codex_session_id_hint,
    CodexLastCompletedTurn, CodexResumeState, CodexTurnContext, CumulativeUsage,
    ParseCodexIncrementalOptions, ParseCodexIncrementalResult, ParseCodexOptions, ParseCodexResult,
    PersistedUserTurnSlot,
};

pub use opencode_stream::{
    create_opencode_stream_ingestor, OpencodeStreamCursorState, OpencodeStreamIngestOptions,
    OpencodeStreamIngestResult, OpencodeStreamIngestor,
};

pub use classifier::{
    classify_activity, count_retries, normalize_tool_name, parse_bash_command, BashParse,
    ClassificationInput, ClassificationResult,
};
pub use fidelity::classify_fidelity;
pub use git::{
    canonicalize_remote_url, parse_git_config, resolve_project, ProjectResolver, ResolvedProject,
};
pub use hash::{args_hash, content_hash, stable_stringify};
pub use types::{
    ActivityCategory, CompactionEvent, ContentKind, ContentRecord, ContentRole, ContentStoreMode,
    ContentToolResult, ContentToolUse, Coverage, Fidelity, FidelityClass, Harness,
    RelationshipSourceKind, RelationshipType, SessionRelationshipRecord, SourceKind, Subagent,
    ToolCall, ToolResultEventRecord, ToolResultEventSource, ToolResultStatus, TurnRecord, Usage,
    UsageGranularity, UserTurnBlock, UserTurnRecord,
};
pub use user_turn::{
    bytes_to_approx_tokens, measure_content_bytes, stringify_measured_content, HeuristicCounter,
    TokenCounter, UserTurnTokenizer,
};
pub use claude::{
    parse_claude_session, parse_claude_session_with_counter, reconcile_claude_session_relationships,
    ClaudeRelationshipEvidence, ParseOptions as ClaudeParseOptions,
    ParseResult as ClaudeParseResult, ReconcileClaudeRelationshipsInput,
};
