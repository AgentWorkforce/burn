//! Ledger / record types — Rust port of `packages/reader/src/types.ts`.
//!
//! Records are designed to round-trip with the TypeScript schemas: optional
//! fields use `Option<T>` with `#[serde(skip_serializing_if = "Option::is_none")]`
//! so that absent fields stay absent on the wire (TS treats `undefined` and
//! "missing" identically and the ledger is JSONL — preserving missingness is
//! how Rust output stays byte-identical to TS output).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    ClaudeCode,
    Codex,
    Opencode,
    AnthropicApi,
    OpenaiApi,
    GeminiApi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelationshipSourceKind {
    ClaudeCode,
    Codex,
    Opencode,
    AnthropicApi,
    OpenaiApi,
    GeminiApi,
    SpawnEnv,
    NativeClaude,
    NativeOpencode,
}

/// Coarse harness identity. Mirrors `SourceKind` but exists as a separate type
/// because some downstream consumers (CLI/SDK) want to talk about a "harness"
/// without forcing an opinion on the underlying log format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Codex,
    Opencode,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    #[serde(rename = "cacheRead")]
    pub cache_read: u64,
    #[serde(rename = "cacheCreate5m")]
    pub cache_create_5m: u64,
    #[serde(rename = "cacheCreate1h")]
    pub cache_create_1h: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(rename = "argsHash")]
    pub args_hash: String,
    #[serde(default, rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(
        default,
        rename = "editPreHash",
        skip_serializing_if = "Option::is_none"
    )]
    pub edit_pre_hash: Option<String>,
    #[serde(
        default,
        rename = "editPostHash",
        skip_serializing_if = "Option::is_none"
    )]
    pub edit_post_hash: Option<String>,
    #[serde(default, rename = "skillName", skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
    #[serde(
        default,
        rename = "replacedTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub replaced_tools: Option<Vec<String>>,
    #[serde(
        default,
        rename = "collapsedCalls",
        skip_serializing_if = "Option::is_none"
    )]
    pub collapsed_calls: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subagent {
    #[serde(rename = "isSidechain")]
    pub is_sidechain: bool,
    #[serde(
        default,
        rename = "parentToolUseId",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_tool_use_id: Option<String>,
    #[serde(default, rename = "agentId", skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(
        default,
        rename = "parentAgentId",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_agent_id: Option<String>,
    #[serde(
        default,
        rename = "subagentType",
        skip_serializing_if = "Option::is_none"
    )]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsageGranularity {
    PerTurn,
    PerMessage,
    PerSessionAggregate,
    CostOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    #[serde(rename = "hasInputTokens")]
    pub has_input_tokens: bool,
    #[serde(rename = "hasOutputTokens")]
    pub has_output_tokens: bool,
    #[serde(rename = "hasReasoningTokens")]
    pub has_reasoning_tokens: bool,
    #[serde(rename = "hasCacheReadTokens")]
    pub has_cache_read_tokens: bool,
    #[serde(rename = "hasCacheCreateTokens")]
    pub has_cache_create_tokens: bool,
    #[serde(rename = "hasToolCalls")]
    pub has_tool_calls: bool,
    #[serde(rename = "hasToolResultEvents")]
    pub has_tool_result_events: bool,
    #[serde(rename = "hasSessionRelationships")]
    pub has_session_relationships: bool,
    #[serde(rename = "hasRawContent")]
    pub has_raw_content: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FidelityClass {
    Full,
    UsageOnly,
    AggregateOnly,
    CostOnly,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fidelity {
    pub granularity: UsageGranularity,
    pub coverage: Coverage,
    pub class: FidelityClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityCategory {
    Planning,
    Delegation,
    Testing,
    Review,
    Git,
    BuildDeploy,
    Deps,
    Format,
    Verification,
    Coding,
    Docs,
    Debugging,
    Refactoring,
    Feature,
    Exploration,
    Reasoning,
    Brainstorming,
    Conversation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub v: u32, // schema version (always 1 today)
    pub source: SourceKind,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(
        default,
        rename = "sessionPath",
        skip_serializing_if = "Option::is_none"
    )]
    pub session_path: Option<String>,
    #[serde(rename = "messageId")]
    pub message_id: String,
    #[serde(rename = "turnIndex")]
    pub turn_index: u64,
    pub ts: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(
        default,
        rename = "projectKey",
        skip_serializing_if = "Option::is_none"
    )]
    pub project_key: Option<String>,
    pub usage: Usage,
    #[serde(rename = "toolCalls")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(
        default,
        rename = "filesTouched",
        skip_serializing_if = "Option::is_none"
    )]
    pub files_touched: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<Subagent>,
    #[serde(
        default,
        rename = "stopReason",
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u64>,
    #[serde(default, rename = "hasEdits", skip_serializing_if = "Option::is_none")]
    pub has_edits: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fidelity: Option<Fidelity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserTurnBlockKind {
    ToolResult,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTurnBlock {
    pub kind: UserTurnBlockKind,
    #[serde(default, rename = "toolUseId", skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(rename = "byteLen")]
    pub byte_len: u64,
    #[serde(rename = "approxTokens")]
    pub approx_tokens: u64,
    #[serde(default, rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTurnRecord {
    pub v: u32,
    pub source: SourceKind,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "userUuid")]
    pub user_uuid: String,
    pub ts: String,
    #[serde(
        default,
        rename = "precedingMessageId",
        skip_serializing_if = "Option::is_none"
    )]
    pub preceding_message_id: Option<String>,
    #[serde(
        default,
        rename = "followingMessageId",
        skip_serializing_if = "Option::is_none"
    )]
    pub following_message_id: Option<String>,
    pub blocks: Vec<UserTurnBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelationshipType {
    Root,
    Continuation,
    Fork,
    Subagent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRelationshipRecord {
    pub v: u32,
    pub source: RelationshipSourceKind,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(
        default,
        rename = "relatedSessionId",
        skip_serializing_if = "Option::is_none"
    )]
    pub related_session_id: Option<String>,
    #[serde(rename = "relationshipType")]
    pub relationship_type: RelationshipType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    #[serde(
        default,
        rename = "sourceSessionId",
        skip_serializing_if = "Option::is_none"
    )]
    pub source_session_id: Option<String>,
    #[serde(
        default,
        rename = "sourceVersion",
        skip_serializing_if = "Option::is_none"
    )]
    pub source_version: Option<String>,
    #[serde(
        default,
        rename = "parentToolUseId",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_tool_use_id: Option<String>,
    #[serde(default, rename = "agentId", skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(
        default,
        rename = "subagentType",
        skip_serializing_if = "Option::is_none"
    )]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolResultStatus {
    Running,
    Completed,
    Errored,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultEventSource {
    ToolResult,
    SubagentNotification,
    QueueEvent,
    ProgressEvent,
    FunctionCallOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsageAttribution {
    SingleToolTurn,
    EvenSplitTurn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultEventRecord {
    pub v: u32,
    pub source: SourceKind,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default, rename = "messageId", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(rename = "toolUseId")]
    pub tool_use_id: String,
    #[serde(default, rename = "callIndex", skip_serializing_if = "Option::is_none")]
    pub call_index: Option<u64>,
    #[serde(rename = "eventIndex")]
    pub event_index: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    pub status: ToolResultStatus,
    #[serde(rename = "eventSource")]
    pub event_source: ToolResultEventSource,
    #[serde(
        default,
        rename = "contentLength",
        skip_serializing_if = "Option::is_none"
    )]
    pub content_length: Option<u64>,
    #[serde(
        default,
        rename = "contentHash",
        skip_serializing_if = "Option::is_none"
    )]
    pub content_hash: Option<String>,
    #[serde(default, rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(
        default,
        rename = "usageAttribution",
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_attribution: Option<UsageAttribution>,
    #[serde(
        default,
        rename = "subagentSessionId",
        skip_serializing_if = "Option::is_none"
    )]
    pub subagent_session_id: Option<String>,
    #[serde(default, rename = "agentId", skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(
        default,
        rename = "replacedTools",
        skip_serializing_if = "Option::is_none"
    )]
    pub replaced_tools: Option<Vec<String>>,
    #[serde(
        default,
        rename = "collapsedCalls",
        skip_serializing_if = "Option::is_none"
    )]
    pub collapsed_calls: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionEvent {
    pub v: u32,
    pub source: SourceKind,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub ts: String,
    #[serde(
        default,
        rename = "precedingMessageId",
        skip_serializing_if = "Option::is_none"
    )]
    pub preceding_message_id: Option<String>,
    #[serde(
        default,
        rename = "tokensBeforeCompact",
        skip_serializing_if = "Option::is_none"
    )]
    pub tokens_before_compact: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentRole {
    User,
    Assistant,
    ToolResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Thinking,
    ToolUse,
    ToolResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentToolUse {
    pub id: String,
    pub name: String,
    pub input: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentToolResult {
    #[serde(rename = "toolUseId")]
    pub tool_use_id: String,
    pub content: Value,
    #[serde(default, rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentRecord {
    pub v: u32,
    pub source: SourceKind,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "messageId")]
    pub message_id: String,
    pub ts: String,
    pub role: ContentRole,
    pub kind: ContentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, rename = "toolUse", skip_serializing_if = "Option::is_none")]
    pub tool_use: Option<ContentToolUse>,
    #[serde(
        default,
        rename = "toolResult",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_result: Option<ContentToolResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentStoreMode {
    Full,
    HashOnly,
    Off,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kind_round_trips_kebab_case() {
        let s = serde_json::to_string(&SourceKind::ClaudeCode).unwrap();
        assert_eq!(s, "\"claude-code\"");
        let back: SourceKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, SourceKind::ClaudeCode);
    }

    #[test]
    fn activity_category_serializes_kebab_case() {
        let s = serde_json::to_string(&ActivityCategory::BuildDeploy).unwrap();
        assert_eq!(s, "\"build-deploy\"");
    }

    #[test]
    fn turn_record_omits_optional_fields_when_none() {
        let rec = TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "abc".into(),
            session_path: None,
            message_id: "m1".into(),
            turn_index: 0,
            ts: "2025-01-01T00:00:00Z".into(),
            model: "claude-sonnet-4-6".into(),
            project: None,
            project_key: None,
            usage: Usage::default(),
            tool_calls: vec![],
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        };
        let s = serde_json::to_string(&rec).unwrap();
        // Match TS shape: optional fields are omitted, not emitted as null.
        assert!(!s.contains("null"));
        assert!(!s.contains("sessionPath"));
        assert!(!s.contains("subagent"));
        assert!(s.contains("\"toolCalls\":[]"));
    }

    #[test]
    fn coverage_all_off_round_trips() {
        let cov = Coverage {
            has_input_tokens: false,
            has_output_tokens: false,
            has_reasoning_tokens: false,
            has_cache_read_tokens: false,
            has_cache_create_tokens: false,
            has_tool_calls: false,
            has_tool_result_events: false,
            has_session_relationships: false,
            has_raw_content: false,
        };
        let s = serde_json::to_string(&cov).unwrap();
        let back: Coverage = serde_json::from_str(&s).unwrap();
        assert_eq!(cov, back);
    }
}
