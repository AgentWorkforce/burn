//! Rust port of `@relayburn/reader`. See AgentWorkforce/burn#242.
//!
//! This crate is a work-in-progress port of the TS reader package. Foundational
//! modules (`types`, `hash`, `fidelity`, `git`, `classifier`, `user_turn`) are
//! ported with native conformance tests; the `codex` (#256), `opencode`
//! (#257), and `opencode_stream` (#258) parsers are ported; the Claude Code
//! parser (`claude`) covers the synchronous, incremental, and cross-file
//! reconciliation surface (#255).

// The four absorbed module roots carry the lower crates whole, including
// items the SDK does not re-export (dead from the SDK perspective). Silence
// the never-used warnings rather than handpicking re-exports — the next
// agent absorbing more verbs will need them.
#![allow(dead_code, unused_imports)]

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
pub use opencode::{
    parse_opencode_session, parse_opencode_session_incremental, ParseOpencodeIncrementalOptions,
    ParseOpencodeIncrementalResult, ParseOpencodeOptions, ParseOpencodeResult,
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
    UsageAttribution, UsageGranularity, UserTurnBlock, UserTurnBlockKind, UserTurnRecord,
};
pub use user_turn::{
    bytes_to_approx_tokens, measure_content_bytes, stringify_measured_content, HeuristicCounter,
    TokenCounter, UserTurnTokenizer,
};
pub use claude::{
    parse_claude_session, parse_claude_session_incremental,
    parse_claude_session_incremental_with_counter, parse_claude_session_with_counter,
    reconcile_claude_session_relationships, ClaudeRelationshipEvidence,
    ParseIncrementalOptions as ClaudeParseIncrementalOptions,
    ParseIncrementalResult as ClaudeParseIncrementalResult, ParseOptions as ClaudeParseOptions,
    ParseResult as ClaudeParseResult, ReconcileClaudeRelationshipsInput,
};
