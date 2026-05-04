//! Rust port of `@relayburn/reader`. See AgentWorkforce/burn#242.
//!
//! This crate is a work-in-progress port of the TS reader package. Foundational
//! modules (`types`, `hash`, `fidelity`, `git`, `classifier`, `user_turn`) are
//! ported with native conformance tests; the per-harness parsers (`claude`,
//! `codex`, `opencode`, `opencode_stream`) are scaffolded but not yet
//! implemented — see #255–#258.

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
