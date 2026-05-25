//! Ledger / record types — Rust port of `packages/reader/src/types.ts`.
//!
//! Each record uses `#[serde(rename_all = "camelCase")]` so the on-wire JSONL
//! shape stays aligned with the TypeScript schemas without per-field
//! `rename` attributes. Optional fields use `Option<T>` plus
//! `skip_serializing_if = "Option::is_none"` so missing-vs-null on the wire
//! continues to mean "absent" rather than "explicit null" — the ledger is
//! append-only JSONL and we want byte-identical output to the TS writer.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Lenient deserializer for `TurnRecord.stop_reason`. Accepts the canonical
/// kebab-case variant (`end-turn`, `max-tokens`, …) plus the legacy free-text
/// shapes from upstream harnesses (`end_turn`, `tool_use`, opencode's
/// `tool-calls`, etc.). An unrecognized string decodes to
/// [`StopReason::Silent`] instead of an error so a pre-3.0 ledger replays
/// cleanly through the new column.
fn deserialize_optional_stop_reason<'de, D>(d: D) -> std::result::Result<Option<StopReason>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(d)?;
    Ok(opt.map(|s| StopReason::from_wire(&s).unwrap_or(StopReason::Silent)))
}

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

impl SourceKind {
    /// Kebab-case label as emitted on the wire (matches `#[serde(rename_all = "kebab-case")]`).
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::AnthropicApi => "anthropic-api",
            Self::OpenaiApi => "openai-api",
            Self::GeminiApi => "gemini-api",
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_str())
    }
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

impl RelationshipSourceKind {
    /// Kebab-case label as emitted on the wire.
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::AnthropicApi => "anthropic-api",
            Self::OpenaiApi => "openai-api",
            Self::GeminiApi => "gemini-api",
            Self::SpawnEnv => "spawn-env",
            Self::NativeClaude => "native-claude",
            Self::NativeOpencode => "native-opencode",
        }
    }
}

impl fmt::Display for RelationshipSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_str())
    }
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
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_create_5m: u64,
    pub cache_create_1h: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub args_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_pre_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_post_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapsed_calls: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subagent {
    pub is_sidechain: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

impl UsageGranularity {
    /// Kebab-case label as emitted on the wire (matches `#[serde(rename_all = "kebab-case")]`).
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::PerTurn => "per-turn",
            Self::PerMessage => "per-message",
            Self::PerSessionAggregate => "per-session-aggregate",
            Self::CostOnly => "cost-only",
        }
    }
}

impl fmt::Display for UsageGranularity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    pub has_input_tokens: bool,
    pub has_output_tokens: bool,
    pub has_reasoning_tokens: bool,
    pub has_cache_read_tokens: bool,
    pub has_cache_create_tokens: bool,
    pub has_tool_calls: bool,
    pub has_tool_result_events: bool,
    pub has_session_relationships: bool,
    pub has_raw_content: bool,
}

impl Coverage {
    /// All-false coverage. Parsers should clone this and flip the flags they
    /// actually populate — defaulting to `false` keeps "do we know X?" honest.
    pub const EMPTY: Self = Self {
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

    /// True iff every field required for command-level "full" fidelity is
    /// populated. See `FidelityClass::Full` for what this gates.
    pub fn is_full(&self) -> bool {
        self.has_input_tokens
            && self.has_output_tokens
            && self.has_cache_read_tokens
            && self.has_tool_calls
            && self.has_tool_result_events
            && self.has_session_relationships
    }

    /// True iff per-turn input/output tokens are reported. Coarser bar than
    /// `is_full` — usable for usage-only commands like `summary`.
    pub fn has_per_turn_usage(&self) -> bool {
        self.has_input_tokens && self.has_output_tokens
    }
}

impl Default for Coverage {
    fn default() -> Self {
        Self::EMPTY
    }
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

impl FidelityClass {
    /// Kebab-case label as emitted on the wire (matches `#[serde(rename_all = "kebab-case")]`).
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::UsageOnly => "usage-only",
            Self::AggregateOnly => "aggregate-only",
            Self::CostOnly => "cost-only",
            Self::Partial => "partial",
        }
    }
}

impl fmt::Display for FidelityClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fidelity {
    pub granularity: UsageGranularity,
    pub coverage: Coverage,
    pub class: FidelityClass,
}

/// Coarse outcome of an assistant turn, derived from the harness-reported
/// stop reason on the trailing assistant row.
///
/// Wire shape is kebab-case (`end-turn`, `max-tokens`, …). On-disk and on the
/// JSONL surface this is round-trippable as a string; absent rows decode as
/// `None`. `Silent` is reserved for the "we have an inference but it carries
/// no stop reason" case (mid-write, sidechain, harness that doesn't report
/// one in the row we parsed). The lossy mapping for non-Anthropic harnesses
/// is intentional; downstream presenters consume the variant, not the raw
/// string. See issue #437.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    PauseTurn,
    StopSequence,
    ToolUse,
    Refusal,
    Silent,
}

impl StopReason {
    /// Kebab-case label as emitted on the wire (matches
    /// `#[serde(rename_all = "kebab-case")]`).
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::EndTurn => "end-turn",
            Self::MaxTokens => "max-tokens",
            Self::PauseTurn => "pause-turn",
            Self::StopSequence => "stop-sequence",
            Self::ToolUse => "tool-use",
            Self::Refusal => "refusal",
            Self::Silent => "silent",
        }
    }

    /// Map a harness-emitted stop-reason string (e.g. Anthropic's
    /// `end_turn` / `max_tokens` or opencode's `tool-calls`) onto the
    /// canonical [`StopReason`]. Returns `None` for unrecognized inputs so
    /// callers can decide whether to fall back to [`StopReason::Silent`]
    /// or keep `None`.
    ///
    /// Matching is case-insensitive and accepts either snake_case or
    /// kebab-case so the same parser handles Anthropic (`max_tokens`),
    /// opencode (`tool-calls`), and the kebab-case wire form we emit
    /// ourselves on round-trip.
    pub fn from_wire(raw: &str) -> Option<Self> {
        let normalized: String = raw
            .trim()
            .chars()
            .map(|c| match c {
                '_' => '-',
                c => c.to_ascii_lowercase(),
            })
            .collect();
        match normalized.as_str() {
            // OpenAI / AI-SDK convention emits `"stop"` for ordinary
            // end-of-turn completions (this is what opencode forwards),
            // so it maps to `EndTurn`, not `StopSequence`. Anthropic's
            // actual stop-sequence outcome is the explicit
            // `"stop_sequence"` / `"stop-sequence"` variant below.
            "end-turn" | "stop" => Some(Self::EndTurn),
            "max-tokens" | "length" => Some(Self::MaxTokens),
            "pause-turn" => Some(Self::PauseTurn),
            "stop-sequence" => Some(Self::StopSequence),
            // `tool-calls` is the opencode AI-SDK wire form; `tool-use`
            // is the Anthropic one and our canonical kebab spelling.
            "tool-use" | "tool-calls" => Some(Self::ToolUse),
            "refusal" => Some(Self::Refusal),
            "silent" => Some(Self::Silent),
            _ => None,
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_str())
    }
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
#[serde(rename_all = "camelCase")]
pub struct TurnRecord {
    pub v: u32,
    pub source: SourceKind,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    pub message_id: String,
    pub turn_index: u64,
    pub ts: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    pub usage: Usage,
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_touched: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<Subagent>,
    /// Outcome of the assistant inference, as reported by the harness on the
    /// trailing assistant row (Anthropic `stop_reason`, opencode
    /// `step-finish.reason`, etc.). `None` means the row carried no field at
    /// all (e.g. Codex, which doesn't report one); deserialization is
    /// tolerant of the historical free-text shape — unknown strings decode
    /// as [`StopReason::Silent`] so a future harness value can't poison
    /// reads.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_stop_reason"
    )]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
#[serde(rename_all = "camelCase")]
pub struct UserTurnBlock {
    pub kind: UserTurnBlockKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    pub byte_len: u64,
    pub approx_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTurnRecord {
    pub v: u32,
    pub source: SourceKind,
    pub session_id: String,
    pub user_uuid: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preceding_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

impl RelationshipType {
    /// Kebab-case label as emitted on the wire.
    pub fn wire_str(&self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Continuation => "continuation",
            Self::Fork => "fork",
            Self::Subagent => "subagent",
        }
    }
}

impl fmt::Display for RelationshipType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRelationshipRecord {
    pub v: u32,
    pub source: RelationshipSourceKind,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_session_id: Option<String>,
    pub relationship_type: RelationshipType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
#[serde(rename_all = "camelCase")]
pub struct ToolResultEventRecord {
    pub v: u32,
    pub source: SourceKind,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_index: Option<u64>,
    pub event_index: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    pub status: ToolResultStatus,
    pub event_source: ToolResultEventSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_attribution: Option<UsageAttribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapsed_calls: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEvent {
    pub v: u32,
    pub source: SourceKind,
    pub session_id: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preceding_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
#[serde(rename_all = "camelCase")]
pub struct ContentToolResult {
    pub tool_use_id: String,
    pub content: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentRecord {
    pub v: u32,
    pub source: SourceKind,
    pub session_id: String,
    pub message_id: String,
    pub ts: String,
    pub role: ContentRole,
    pub kind: ContentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use: Option<ContentToolUse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
        assert_eq!(
            serde_json::from_str::<SourceKind>(&s).unwrap(),
            SourceKind::ClaudeCode
        );
    }

    #[test]
    fn activity_category_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&ActivityCategory::BuildDeploy).unwrap(),
            "\"build-deploy\"",
        );
    }

    #[test]
    fn usage_field_names_are_camel_case() {
        let u = Usage {
            input: 1,
            output: 2,
            reasoning: 3,
            cache_read: 4,
            cache_create_5m: 5,
            cache_create_1h: 6,
        };
        let s = serde_json::to_string(&u).unwrap();
        assert!(s.contains("\"cacheRead\":4"), "got {s}");
        assert!(s.contains("\"cacheCreate5m\":5"), "got {s}");
        assert!(s.contains("\"cacheCreate1h\":6"), "got {s}");
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
        assert!(!s.contains("null"));
        assert!(!s.contains("sessionPath"));
        assert!(!s.contains("subagent"));
        assert!(s.contains("\"toolCalls\":[]"));
        assert!(s.contains("\"sessionId\":\"abc\""));
        assert!(s.contains("\"messageId\":\"m1\""));
        assert!(s.contains("\"turnIndex\":0"));
    }

    #[test]
    fn coverage_default_is_empty_const() {
        assert_eq!(Coverage::default(), Coverage::EMPTY);
    }

    #[test]
    fn coverage_methods_match_field_logic() {
        let mut c = Coverage::EMPTY;
        assert!(!c.is_full());
        assert!(!c.has_per_turn_usage());
        c.has_input_tokens = true;
        c.has_output_tokens = true;
        assert!(!c.is_full());
        assert!(c.has_per_turn_usage());
        c.has_cache_read_tokens = true;
        c.has_tool_calls = true;
        c.has_tool_result_events = true;
        c.has_session_relationships = true;
        assert!(c.is_full());
    }

    #[test]
    fn stop_reason_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            "\"end-turn\"",
        );
        assert_eq!(
            serde_json::to_string(&StopReason::MaxTokens).unwrap(),
            "\"max-tokens\"",
        );
        assert_eq!(
            serde_json::to_string(&StopReason::ToolUse).unwrap(),
            "\"tool-use\"",
        );
    }

    #[test]
    fn stop_reason_from_wire_normalizes_underscored_and_legacy_variants() {
        // Anthropic snake_case.
        assert_eq!(
            StopReason::from_wire("end_turn"),
            Some(StopReason::EndTurn)
        );
        assert_eq!(
            StopReason::from_wire("max_tokens"),
            Some(StopReason::MaxTokens)
        );
        assert_eq!(
            StopReason::from_wire("tool_use"),
            Some(StopReason::ToolUse)
        );
        // Opencode finish reason for the same outcome ships as `tool-calls`.
        assert_eq!(
            StopReason::from_wire("tool-calls"),
            Some(StopReason::ToolUse)
        );
        // Canonical kebab-case round-trips identity.
        assert_eq!(
            StopReason::from_wire("pause-turn"),
            Some(StopReason::PauseTurn)
        );
        // OpenAI / AI-SDK (and therefore opencode) emit a bare `"stop"`
        // for normal end-of-turn completions — that's `EndTurn`, NOT
        // `StopSequence`. Misclassifying these would skew the
        // `burn summary` outcome buckets every time opencode wraps an
        // OpenAI-shaped provider.
        assert_eq!(StopReason::from_wire("stop"), Some(StopReason::EndTurn));
        // Anthropic's actual stop-sequence outcome is the explicit
        // `stop_sequence` (snake) / `stop-sequence` (kebab) variants and
        // those still resolve to `StopSequence`.
        assert_eq!(
            StopReason::from_wire("stop_sequence"),
            Some(StopReason::StopSequence)
        );
        assert_eq!(
            StopReason::from_wire("stop-sequence"),
            Some(StopReason::StopSequence)
        );
        // Unknown / harness-specific strings don't map.
        assert_eq!(StopReason::from_wire("magic"), None);
    }

    #[test]
    fn turn_record_stop_reason_deserializes_legacy_strings_into_enum() {
        // Pre-3.0 ledgers stored the raw Anthropic stop_reason on
        // `TurnRecord.stopReason` as a free-text string. The lenient
        // deserializer must keep replaying those rows.
        let raw = serde_json::json!({
            "v": 1,
            "source": "claude-code",
            "sessionId": "s",
            "messageId": "m",
            "turnIndex": 0,
            "ts": "2026-04-20T00:00:00.000Z",
            "model": "claude-sonnet-4-6",
            "usage": {
                "input": 0, "output": 0, "reasoning": 0,
                "cacheRead": 0, "cacheCreate5m": 0, "cacheCreate1h": 0
            },
            "toolCalls": [],
            "stopReason": "max_tokens"
        });
        let rec: TurnRecord = serde_json::from_value(raw).unwrap();
        assert_eq!(rec.stop_reason, Some(StopReason::MaxTokens));
    }

    #[test]
    fn turn_record_stop_reason_unknown_string_falls_back_to_silent() {
        // A ledger written by a future / unknown harness shouldn't break
        // reads — the parser maps to `Silent` instead of erroring.
        let raw = serde_json::json!({
            "v": 1,
            "source": "claude-code",
            "sessionId": "s",
            "messageId": "m",
            "turnIndex": 0,
            "ts": "2026-04-20T00:00:00.000Z",
            "model": "claude-sonnet-4-6",
            "usage": {
                "input": 0, "output": 0, "reasoning": 0,
                "cacheRead": 0, "cacheCreate5m": 0, "cacheCreate1h": 0
            },
            "toolCalls": [],
            "stopReason": "totally-unknown-future-value"
        });
        let rec: TurnRecord = serde_json::from_value(raw).unwrap();
        assert_eq!(rec.stop_reason, Some(StopReason::Silent));
    }

    #[test]
    fn coverage_round_trips_camel_case() {
        let c = Coverage {
            has_input_tokens: true,
            has_output_tokens: true,
            has_cache_create_tokens: true,
            ..Coverage::EMPTY
        };
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("\"hasInputTokens\":true"));
        assert!(s.contains("\"hasCacheCreateTokens\":true"));
        let back: Coverage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, c);
    }
}
