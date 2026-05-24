//! Rust port of `@relayburn/reader`. See AgentWorkforce/burn#242.
//!
//! This crate is a work-in-progress port of the TS reader package. Foundational
//! modules (`types`, `hash`, `fidelity`, `git`, `classifier`, `user_turn`) are
//! ported with native conformance tests; the `codex` (#256) and `opencode`
//! (#257) parsers are ported; the Claude Code parser (`claude`) covers the
//! synchronous, incremental, and cross-file reconciliation surface (#255).

pub mod classifier;
pub mod fidelity;
pub mod git;
pub mod hash;
pub mod types;
pub mod user_turn;

pub mod claude;
pub mod codex;
pub mod opencode;

pub use codex::{
    parse_codex_session_incremental, read_codex_session_id_hint, CodexLastCompletedTurn,
    CodexResumeState, CodexTurnContext, CumulativeUsage, ParseCodexIncrementalOptions,
    ParseCodexIncrementalResult, PersistedUserTurnSlot,
};
pub use opencode::{
    parse_opencode_session_incremental, ParseOpencodeIncrementalOptions,
    ParseOpencodeIncrementalResult,
};

pub use classifier::{
    count_retries, normalize_tool_name, parse_bash_command, BashParse, ClassificationInput,
    ClassificationResult,
};
pub use fidelity::classify_fidelity;
pub use git::{resolve_project, ProjectResolver, ResolvedProject};
pub use types::{
    ActivityCategory, CompactionEvent, ContentKind, ContentRecord, ContentRole, ContentStoreMode,
    ContentToolResult, ContentToolUse, Coverage, Fidelity, FidelityClass, Harness,
    RelationshipSourceKind, RelationshipType, SessionRelationshipRecord, SourceKind, Subagent,
    ToolCall, ToolResultEventRecord, ToolResultEventSource, ToolResultStatus, TurnRecord, Usage,
    UsageAttribution, UsageGranularity, UserTurnBlock, UserTurnBlockKind, UserTurnRecord,
};
pub use claude::{
    parse_claude_session, parse_claude_session_incremental,
    reconcile_claude_session_relationships, ParseIncrementalOptions as ClaudeParseIncrementalOptions,
    ParseIncrementalResult as ClaudeParseIncrementalResult, ParseOptions as ClaudeParseOptions,
    ParseResult as ClaudeParseResult, ReconcileClaudeRelationshipsInput,
};
