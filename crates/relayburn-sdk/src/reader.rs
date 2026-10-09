//! Burn's record model and the derivations that run over it: the
//! [`types`] every ledger row is, the activity [`classifier`], inference
//! grouping, project resolution, fidelity, hashing, and the span tree
//! builders. Harness sessions are sourced through relayhistory (see
//! `crate::source`), never parsed here.

pub mod classifier;
pub mod fidelity;
pub mod git;
pub mod hash;
pub mod inference;
pub mod reasoning;
pub mod span_tree;
pub mod subagent;
pub mod types;
pub mod user_turn;

pub use span_tree::claude::{build_claude_span_tree, ClaudeSpanTreeInputs};
pub use span_tree::codex::{build_codex_span_tree, CodexSpanTreeInputs};
pub use subagent::{SubagentCounts, SubagentTranscript};

pub use classifier::{
    count_retries, normalize_tool_name, parse_bash_command, BashParse, ClassificationInput,
    ClassificationResult,
};
pub use fidelity::classify_fidelity;
pub use git::{resolve_project, ProjectResolver, ResolvedProject};
pub use inference::{
    build_inferences, Inference, InferenceKeySource, InferenceKind, RequestIdLookup, ToolUseRef,
    TurnKey,
};
pub use reasoning::ReasoningConfig;
pub use types::{
    ActivityCategory, CompactionEvent, ContentKind, ContentRecord, ContentRole, ContentStoreMode,
    ContentToolResult, ContentToolUse, Coverage, Fidelity, FidelityClass, Harness,
    RelationshipSourceKind, RelationshipType, SessionRelationshipRecord, SourceKind, StopReason,
    Subagent, ToolCall, ToolResultEventRecord, ToolResultEventSource, ToolResultStatus, TurnRecord,
    Usage, UsageAttribution, UsageGranularity, UserTurnBlock, UserTurnBlockKind, UserTurnRecord,
};
