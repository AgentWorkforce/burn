//! Codex session parser — Rust port of `packages/reader/src/codex.ts`.
//!
//! Mirrors the TS state-machine field-for-field: a single forward pass over
//! line-delimited JSON records, with all per-turn output buffered and only
//! committed at `task_complete` boundaries. Resume state is round-tripped as
//! [`CodexResumeState`] so an incremental ingest can pick up where the last
//! committed turn left off without re-reading prior bytes.
//!
//! Note: the TS parser defaults to a `cl100k` tokenizer for `UserTurnBlock`
//! sizing. The Rust port uses [`HeuristicCounter`] (bytes/4) — see #246 for
//! the cl100k hookup.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::reader::fidelity::classify_fidelity;
use crate::reader::git::ProjectResolver;
use crate::reader::hash::content_hash;
use crate::reader::types::{
    CompactionEvent, ContentRecord, ContentStoreMode, Coverage, Fidelity, RelationshipSourceKind,
    RelationshipType, SessionRelationshipRecord, SourceKind, ToolCall, ToolResultEventRecord,
    ToolResultStatus, TurnRecord, Usage, UsageGranularity, UserTurnBlock, UserTurnRecord,
};
use crate::reader::user_turn::{resolve_token_counter, UserTurnTokenizer};

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct ParseCodexOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub tokenizer: Option<UserTurnTokenizer>,
}

#[derive(Debug, Clone, Default)]
pub struct ParseCodexIncrementalOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub tokenizer: Option<UserTurnTokenizer>,
    pub start_offset: Option<u64>,
    pub resume: Option<CodexResumeState>,
}

#[derive(Debug, Clone, Default)]
pub struct CumulativeUsage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub reasoning: i64,
}

#[derive(Debug, Clone, Default)]
pub struct PersistedUserTurnSlot {
    pub blocks: Vec<UserTurnBlock>,
    pub preceding_message_id: Option<String>,
    pub ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexLastCompletedTurn {
    pub message_id: String,
    pub cache_read: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CodexTurnContext {
    pub turn_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexResumeState {
    pub cumulative: CumulativeUsage,
    pub session_id: String,
    pub session_cwd: Option<String>,
    pub turn_contexts: HashMap<String, CodexTurnContext>,
    pub user_turn_slot: Option<PersistedUserTurnSlot>,
    pub root_session_emitted: bool,
    pub session_meta_relationship_keys: Vec<String>,
    pub next_event_index: u64,
    pub tool_result_counters: HashMap<String, u64>,
    pub last_completed_turn: Option<CodexLastCompletedTurn>,
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct ParseCodexResult {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub events: Vec<CompactionEvent>,
    pub user_turns: Vec<UserTurnRecord>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
}

#[derive(Debug, Clone, Default)]
pub struct ParseCodexIncrementalResult {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub events: Vec<CompactionEvent>,
    pub user_turns: Vec<UserTurnRecord>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
    pub end_offset: u64,
    pub resume: CodexResumeState,
}

/// Best-effort sniff of the `session_meta.payload.id` from the first JSONL
/// line. Used by ingest to map a renamed-on-disk rollout file back to its
/// canonical session id without parsing the full file.
pub fn read_codex_session_id_hint(file_path: impl AsRef<Path>) -> Option<String> {
    let mut file = File::open(file_path.as_ref()).ok()?;
    let mut buf = vec![0u8; 8192];
    let n = file.read(&mut buf).ok()?;
    if n == 0 {
        return None;
    }
    let raw = std::str::from_utf8(&buf[..n]).ok()?;
    let first = match raw.find('\n') {
        Some(idx) => &raw[..idx],
        None => raw,
    };
    let first = first.trim_end_matches('\r').trim();
    if first.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(first).ok()?;
    if parsed.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let payload = parsed.get("payload")?;
    session_meta_payload_id(payload)
}

#[cfg(test)]
pub fn parse_codex_session(
    file_path: impl AsRef<Path>,
    options: &ParseCodexOptions,
) -> std::io::Result<ParseCodexResult> {
    let inc_opts = ParseCodexIncrementalOptions {
        session_path: options.session_path.clone(),
        content_mode: options.content_mode,
        tokenizer: options.tokenizer,
        start_offset: Some(0),
        resume: None,
    };
    parse_codex_session_incremental(file_path, &inc_opts).map(ParseCodexResult::from)
}

#[cfg(test)]
impl From<ParseCodexIncrementalResult> for ParseCodexResult {
    fn from(r: ParseCodexIncrementalResult) -> Self {
        Self {
            turns: r.turns,
            content: r.content,
            events: r.events,
            user_turns: r.user_turns,
            relationships: r.relationships,
            tool_result_events: r.tool_result_events,
        }
    }
}

pub fn parse_codex_session_incremental(
    file_path: impl AsRef<Path>,
    options: &ParseCodexIncrementalOptions,
) -> std::io::Result<ParseCodexIncrementalResult> {
    // The TS parser defaults to cl100k; the Rust port only ships
    // `HeuristicCounter` until a tiktoken-equivalent lands (#246). Honor the
    // option by accepting `None` / `Some(Heuristic)` and rejecting
    // `Some(Cl100k)` with a clear error rather than silently falling back.
    resolve_token_counter(options.tokenizer)?;
    let start_offset = options.start_offset.unwrap_or(0);
    let mut file = File::open(file_path.as_ref())?;
    let size = file.metadata()?.len();
    if start_offset >= size {
        return Ok(ParseCodexIncrementalResult {
            turns: vec![],
            content: vec![],
            events: vec![],
            user_turns: vec![],
            relationships: vec![],
            tool_result_events: vec![],
            end_offset: start_offset,
            resume: clone_resume(options.resume.as_ref()),
        });
    }
    file.seek(SeekFrom::Start(start_offset))?;
    // Stream from `start_offset` line-by-line. The previous implementation
    // pre-allocated `vec![0u8; (size - start_offset) as usize]` and
    // `read_exact` into it — for a multi-GB session that was a multi-GB
    // up-front allocation. With BufReader + `read_until` only the longest
    // single line stays resident.
    let reader = BufReader::new(file);

    let project_resolver = ProjectResolver::new();
    parse_codex_buffer(reader, start_offset, options, &project_resolver)
}

// ---------------------------------------------------------------------------
// Internal parser state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(in crate::reader::codex) struct UserTurnSlot {
    pub(in crate::reader::codex) blocks: Vec<UserTurnBlock>,
    pub(in crate::reader::codex) preceding_message_id: Option<String>,
    pub(in crate::reader::codex) ts: String,
}

impl UserTurnSlot {
    pub(in crate::reader::codex) fn from_persisted(p: &PersistedUserTurnSlot) -> Self {
        Self {
            blocks: p.blocks.clone(),
            preceding_message_id: p.preceding_message_id.clone(),
            ts: p.ts.clone(),
        }
    }
    pub(in crate::reader::codex) fn to_persisted(&self) -> PersistedUserTurnSlot {
        PersistedUserTurnSlot {
            blocks: self.blocks.clone(),
            preceding_message_id: self.preceding_message_id.clone(),
            ts: self.ts.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(in crate::reader::codex) struct SpawnCallInfo {
    pub(in crate::reader::codex) call_id: String,
    pub(in crate::reader::codex) ts: String,
    pub(in crate::reader::codex) subagent_type: Option<String>,
    pub(in crate::reader::codex) description: Option<String>,
    pub(in crate::reader::codex) spawned_agent_id: Option<String>,
    pub(in crate::reader::codex) emitted: bool,
}

#[derive(Debug, Clone)]
pub(in crate::reader::codex) struct OpenTurn {
    pub(in crate::reader::codex) turn_id: String,
    pub(in crate::reader::codex) ts: String,
    pub(in crate::reader::codex) model: String,
    pub(in crate::reader::codex) project: Option<String>,
    pub(in crate::reader::codex) start_cumulative: CumulativeUsage,
    pub(in crate::reader::codex) tool_calls: Vec<ToolCall>,
    pub(in crate::reader::codex) seen_call_ids: BTreeSet<String>,
    pub(in crate::reader::codex) files_touched: BTreeSet<String>,
    pub(in crate::reader::codex) user_text: String,
    pub(in crate::reader::codex) assistant_text: String,
    pub(in crate::reader::codex) errored_call_ids: BTreeSet<String>,
    pub(in crate::reader::codex) content: Vec<ContentRecord>,
    pub(in crate::reader::codex) pending_tool_result_events: Vec<ToolResultEventRecord>,
    pub(in crate::reader::codex) pending_relationships: Vec<SessionRelationshipRecord>,
    pub(in crate::reader::codex) spawn_calls: HashMap<String, SpawnCallInfo>,
    pub(in crate::reader::codex) usage_observed: bool,
}

pub(in crate::reader::codex) struct FinalizedTurn {
    pub(in crate::reader::codex) turn_id: String,
    pub(in crate::reader::codex) ts: String,
    pub(in crate::reader::codex) model: String,
    pub(in crate::reader::codex) project: Option<String>,
    pub(in crate::reader::codex) tool_calls: Vec<ToolCall>,
    pub(in crate::reader::codex) files_touched: Vec<String>,
    pub(in crate::reader::codex) user_text: String,
    pub(in crate::reader::codex) assistant_text: String,
    pub(in crate::reader::codex) errored_call_ids: BTreeSet<String>,
    pub(in crate::reader::codex) content: Vec<ContentRecord>,
    pub(in crate::reader::codex) usage: Usage,
    pub(in crate::reader::codex) fidelity: Fidelity,
}

pub(in crate::reader::codex) fn finalize_turn(
    open: OpenTurn,
    cumulative: &CumulativeUsage,
) -> FinalizedTurn {
    let usage = Usage {
        input: (cumulative.input - open.start_cumulative.input).max(0) as u64,
        output: (cumulative.output - open.start_cumulative.output).max(0) as u64,
        reasoning: (cumulative.reasoning - open.start_cumulative.reasoning).max(0) as u64,
        cache_read: (cumulative.cache_read - open.start_cumulative.cache_read).max(0) as u64,
        cache_create_5m: 0,
        cache_create_1h: 0,
    };
    let mut files: Vec<String> = open.files_touched.into_iter().collect();
    files.sort();
    FinalizedTurn {
        turn_id: open.turn_id,
        ts: open.ts,
        model: open.model,
        project: open.project,
        tool_calls: open.tool_calls,
        files_touched: files,
        user_text: open.user_text,
        assistant_text: open.assistant_text,
        errored_call_ids: open.errored_call_ids,
        content: open.content,
        usage,
        fidelity: build_codex_fidelity(open.usage_observed),
    }
}

fn build_codex_fidelity(usage_observed: bool) -> Fidelity {
    let coverage = Coverage {
        has_input_tokens: usage_observed,
        has_output_tokens: usage_observed,
        has_reasoning_tokens: usage_observed,
        has_cache_read_tokens: usage_observed,
        has_cache_create_tokens: false,
        has_tool_calls: true,
        has_tool_result_events: true,
        has_session_relationships: true,
        has_raw_content: true,
    };
    let class = classify_fidelity(UsageGranularity::PerTurn, &coverage);
    Fidelity {
        granularity: UsageGranularity::PerTurn,
        coverage,
        class,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(in crate::reader::codex) fn session_meta_payload_id(payload: &Value) -> Option<String> {
    let id = payload.get("id")?.as_str()?;
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

pub(in crate::reader::codex) fn collect_message_text(payload: &Value, role: &str) -> String {
    let Some(content) = payload.get("content").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for block in content {
        let Some(text) = block.get("text").and_then(|v| v.as_str()) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        if role == "user" && is_codex_boilerplate(text) {
            continue;
        }
        parts.push(text.to_string());
    }
    parts.join("\n")
}

fn is_codex_boilerplate(text: &str) -> bool {
    let trimmed = text.trim_start();
    let lower_first16: String = trimmed.chars().take(32).collect::<String>().to_lowercase();
    if lower_first16.starts_with("<environment_context")
        || lower_first16.starts_with("<permissions")
        || lower_first16.starts_with("<collaboration_mode")
    {
        return true;
    }
    if trimmed.starts_with("<INSTRUCTIONS>") {
        return true;
    }
    // `^\s*#\s*AGENTS\.md`/i — case-insensitive.
    let after_hash = trimmed.strip_prefix('#').map(str::trim_start);
    if let Some(rest) = after_hash {
        let rest_lower: String = rest.chars().take(16).collect::<String>().to_lowercase();
        if rest_lower.starts_with("agents.md") {
            return true;
        }
    }
    false
}

pub(in crate::reader::codex) fn collect_reasoning_text(payload: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(arr) = payload.get("summary").and_then(|v| v.as_array()) {
        for s in arr {
            if let Some(text) = s.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }
    if let Some(arr) = payload.get("content").and_then(|v| v.as_array()) {
        for c in arr {
            if let Some(text) = c.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }
    parts.join("\n")
}

pub(in crate::reader::codex) fn append_text(existing: &str, next: &str) -> String {
    if existing.is_empty() {
        next.to_string()
    } else {
        format!("{}\n{}", existing, next)
    }
}

pub(in crate::reader::codex) fn safe_parse_json_object(
    s: &str,
) -> Option<serde_json::Map<String, Value>> {
    if s.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(s).ok()?;
    match v {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

pub(in crate::reader::codex) fn pick_function_call_target(
    name: &str,
    args: Option<&serde_json::Map<String, Value>>,
) -> Option<String> {
    let args = args?;
    let s =
        |k: &str| -> Option<String> { args.get(k).and_then(|v| v.as_str()).map(|x| x.to_string()) };
    match name {
        "exec_command" | "shell" => s("cmd").or_else(|| s("command")),
        "read_file" => s("path").or_else(|| s("file_path")),
        "write_file" => s("path").or_else(|| s("file_path")),
        _ => s("path")
            .or_else(|| s("file_path"))
            .or_else(|| s("cmd"))
            .or_else(|| s("command"))
            .or_else(|| s("url")),
    }
}

pub(in crate::reader::codex) fn pick_custom_tool_target(name: &str, input: &str) -> Option<String> {
    if name != "apply_patch" {
        return None;
    }
    // Match `^***\s+(?:Update|Add|Delete)\s+File:\s+(\S.*?)\s*$` per-line.
    for line in input.lines() {
        let l = line.trim_start();
        if !l.starts_with("***") {
            continue;
        }
        let rest = l.trim_start_matches('*').trim_start();
        for verb in ["Update", "Add", "Delete"] {
            if let Some(rest) = rest.strip_prefix(verb) {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix("File:") {
                    let rest = rest.trim();
                    if !rest.is_empty() {
                        return Some(rest.to_string());
                    }
                }
            }
        }
    }
    None
}

pub(in crate::reader::codex) fn pick_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    for k in keys {
        if let Some(s) = obj.get(*k).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

pub(in crate::reader::codex) fn extract_spawned_agent_id(output: &Value) -> Option<String> {
    let obj_owner;
    let obj = match output {
        Value::Object(_) => output,
        Value::String(s) => {
            obj_owner = serde_json::from_str::<Value>(s).ok()?;
            if !obj_owner.is_object() {
                return None;
            }
            &obj_owner
        }
        _ => return None,
    };
    pick_string_field(obj, &["agent_id", "subagent_id", "session_id"])
}

#[derive(Default)]
pub(in crate::reader::codex) struct Measured {
    pub(in crate::reader::codex) length: Option<u64>,
    pub(in crate::reader::codex) hash: Option<String>,
    /// Raw UTF-8 byte length of the materialized payload. Same value as
    /// `length` for Codex (the legacy `content_length` already counted
    /// bytes here, not chars), but tracked separately so the
    /// `ToolResultEventRecord` shape stays consistent across sources.
    pub(in crate::reader::codex) byte_length: Option<u64>,
}

pub(in crate::reader::codex) fn measure_tool_output(output: &Value) -> Measured {
    match output {
        Value::Null => Measured::default(),
        Value::String(s) => Measured {
            length: Some(s.len() as u64),
            hash: Some(content_hash(s)),
            byte_length: Some(s.len() as u64),
        },
        other => match serde_json::to_string(other) {
            Ok(serialized) => Measured {
                length: Some(serialized.len() as u64),
                hash: Some(content_hash(&serialized)),
                byte_length: Some(serialized.len() as u64),
            },
            Err(_) => Measured::default(),
        },
    }
}

pub(in crate::reader::codex) fn is_subagent_terminal_notification(t: &str) -> bool {
    if !t.starts_with("subagent_") {
        return false;
    }
    t.ends_with("_complete")
        || t.ends_with("_done")
        || t.ends_with("_finished")
        || t.ends_with("_terminated")
}

pub(in crate::reader::codex) fn subagent_notification_status(payload: &Value) -> ToolResultStatus {
    if let Some(b) = payload.get("success").and_then(|v| v.as_bool()) {
        return if b {
            ToolResultStatus::Completed
        } else {
            ToolResultStatus::Errored
        };
    }
    if let Some(s) = payload.get("status").and_then(|v| v.as_str()) {
        let s = s.to_ascii_lowercase();
        if s == "errored" || s == "failed" || s == "error" {
            return ToolResultStatus::Errored;
        }
        if s == "cancelled" || s == "canceled" {
            return ToolResultStatus::Cancelled;
        }
        if s == "completed" || s == "success" || s == "succeeded" {
            return ToolResultStatus::Completed;
        }
    }
    ToolResultStatus::Completed
}

pub(in crate::reader::codex) fn build_root_relationship(
    session_id: &str,
    ts: &str,
    meta: &Value,
) -> SessionRelationshipRecord {
    let mut row = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::Codex,
        session_id: session_id.to_string(),
        related_session_id: None,
        relationship_type: RelationshipType::Root,
        ts: if ts.is_empty() {
            None
        } else {
            Some(ts.to_string())
        },
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    apply_codex_session_meta_provenance(&mut row, meta);
    row
}

pub(in crate::reader::codex) fn build_session_meta_relationships(
    session_id: &str,
    meta: &Value,
    fallback_ts: &str,
) -> Vec<SessionRelationshipRecord> {
    let mut rows = Vec::new();
    let ts = pick_string_field(meta, &["timestamp"]).unwrap_or_else(|| fallback_ts.to_string());
    if let Some(fork_id) = pick_string_field(meta, &["forkSessionId", "fork_session_id"]) {
        if fork_id != session_id {
            let mut row = SessionRelationshipRecord {
                v: 1,
                source: RelationshipSourceKind::Codex,
                session_id: session_id.to_string(),
                related_session_id: Some(fork_id),
                relationship_type: RelationshipType::Fork,
                ts: if ts.is_empty() {
                    None
                } else {
                    Some(ts.clone())
                },
                source_session_id: None,
                source_version: None,
                parent_tool_use_id: None,
                agent_id: None,
                subagent_type: None,
                description: None,
            };
            apply_codex_session_meta_provenance(&mut row, meta);
            rows.push(row);
        }
    }
    if let Some(prev_id) = pick_string_field(
        meta,
        &["continuedFromSessionId", "continued_from_session_id"],
    ) {
        if prev_id != session_id {
            let mut row = SessionRelationshipRecord {
                v: 1,
                source: RelationshipSourceKind::Codex,
                session_id: session_id.to_string(),
                related_session_id: Some(prev_id),
                relationship_type: RelationshipType::Continuation,
                ts: if ts.is_empty() {
                    None
                } else {
                    Some(ts.clone())
                },
                source_session_id: None,
                source_version: None,
                parent_tool_use_id: None,
                agent_id: None,
                subagent_type: None,
                description: None,
            };
            apply_codex_session_meta_provenance(&mut row, meta);
            rows.push(row);
        }
    }
    rows
}

fn apply_codex_session_meta_provenance(row: &mut SessionRelationshipRecord, meta: &Value) {
    if let Some(s) = pick_string_field(meta, &["sourceSessionId", "source_session_id"]) {
        row.source_session_id = Some(s);
    }
    if let Some(s) = pick_string_field(meta, &["cli_version", "version"]) {
        row.source_version = Some(s);
    }
}

pub(in crate::reader::codex) fn codex_relationship_key(row: &SessionRelationshipRecord) -> String {
    let source = match row.source {
        RelationshipSourceKind::Codex => "codex",
        RelationshipSourceKind::ClaudeCode => "claude-code",
        RelationshipSourceKind::Opencode => "opencode",
        RelationshipSourceKind::AnthropicApi => "anthropic-api",
        RelationshipSourceKind::OpenaiApi => "openai-api",
        RelationshipSourceKind::GeminiApi => "gemini-api",
        RelationshipSourceKind::SpawnEnv => "spawn-env",
        RelationshipSourceKind::NativeClaude => "native-claude",
        RelationshipSourceKind::NativeOpencode => "native-opencode",
    };
    let rel = match row.relationship_type {
        RelationshipType::Root => "root",
        RelationshipType::Continuation => "continuation",
        RelationshipType::Fork => "fork",
        RelationshipType::Subagent => "subagent",
    };
    format!(
        "{}|{}|{}|{}|{}|{}",
        source,
        row.session_id,
        rel,
        row.related_session_id.as_deref().unwrap_or(""),
        row.agent_id.as_deref().unwrap_or(""),
        row.parent_tool_use_id.as_deref().unwrap_or(""),
    )
}

pub(in crate::reader::codex) fn maybe_emit_spawn_relationship(
    open_turn: &mut OpenTurn,
    session_id: &str,
    info: &SpawnCallInfo,
    ts: &str,
) {
    let already = match open_turn.spawn_calls.get(&info.call_id) {
        Some(s) => s.emitted,
        None => false,
    };
    if already {
        return;
    }
    let Some(spawned_id) = info.spawned_agent_id.as_ref() else {
        return;
    };
    let stamp = if ts.is_empty() {
        info.ts.clone()
    } else {
        ts.to_string()
    };
    let row = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::Codex,
        session_id: spawned_id.clone(),
        related_session_id: Some(session_id.to_string()),
        relationship_type: RelationshipType::Subagent,
        ts: if stamp.is_empty() { None } else { Some(stamp) },
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: Some(info.call_id.clone()),
        agent_id: Some(spawned_id.clone()),
        subagent_type: info.subagent_type.clone(),
        description: info.description.clone(),
    };
    open_turn.pending_relationships.push(row);
    if let Some(s) = open_turn.spawn_calls.get_mut(&info.call_id) {
        s.emitted = true;
    }
}

pub(in crate::reader::codex) fn push_content(
    open_turn: &mut Option<OpenTurn>,
    pending: &mut Vec<ContentRecord>,
    record: ContentRecord,
) {
    if let Some(open) = open_turn.as_mut() {
        open.content.push(record);
    } else {
        pending.push(record);
    }
}

pub(in crate::reader::codex) fn build_codex_user_turn_record(
    slot: &UserTurnSlot,
    session_id: &str,
    following_message_id: &str,
    fallback_ts: &str,
) -> UserTurnRecord {
    let preceding_tag = slot.preceding_message_id.as_deref().unwrap_or("start");
    let user_uuid = format!("{}:{}->{}", session_id, preceding_tag, following_message_id);
    let ts = if !slot.ts.is_empty() {
        slot.ts.clone()
    } else {
        fallback_ts.to_string()
    };
    UserTurnRecord {
        v: 1,
        source: SourceKind::Codex,
        session_id: session_id.to_string(),
        user_uuid,
        ts,
        preceding_message_id: slot.preceding_message_id.clone(),
        following_message_id: Some(following_message_id.to_string()),
        blocks: slot.blocks.clone(),
    }
}

pub(in crate::reader::codex) fn build_codex_compaction_event(
    session_id: &str,
    ts: &str,
    preceding: Option<&CodexLastCompletedTurn>,
) -> CompactionEvent {
    let mut event = CompactionEvent {
        v: 1,
        source: SourceKind::Codex,
        session_id: session_id.to_string(),
        ts: ts.to_string(),
        preceding_message_id: None,
        tokens_before_compact: None,
    };
    if let Some(prev) = preceding {
        event.preceding_message_id = Some(prev.message_id.clone());
        event.tokens_before_compact = Some(prev.cache_read);
    }
    event
}

fn clone_resume(r: Option<&CodexResumeState>) -> CodexResumeState {
    match r {
        None => CodexResumeState {
            user_turn_slot: Some(PersistedUserTurnSlot::default()),
            ..Default::default()
        },
        Some(r) => CodexResumeState {
            cumulative: r.cumulative.clone(),
            session_id: r.session_id.clone(),
            session_cwd: r.session_cwd.clone(),
            turn_contexts: r.turn_contexts.clone(),
            user_turn_slot: r
                .user_turn_slot
                .clone()
                .or_else(|| Some(PersistedUserTurnSlot::default())),
            root_session_emitted: r.root_session_emitted,
            session_meta_relationship_keys: r.session_meta_relationship_keys.clone(),
            next_event_index: r.next_event_index,
            tool_result_counters: r.tool_result_counters.clone(),
            last_completed_turn: r.last_completed_turn.clone(),
        },
    }
}

// Per-turn span tree builder. Pure projection over `TurnRecord` +
// paired `tool_result_event` rows. Codex has strictly less hierarchy
// to project than Claude (no requestId, no subagent sidecars); see
// AgentWorkforce/burn#430 and the module's own doc comment for the
// scope discussion.
pub mod span_tree;

// Incremental parse engine: the `CodexParseState` streaming state machine,
// its `CommittedSnapshot` shadow, and the `parse_codex_buffer` driver the
// public `parse_codex_session*` entry points wrap. Split out of this file; the
// helpers/types above feed it per line.
mod incremental;

use self::incremental::parse_codex_buffer;

#[cfg(test)]
mod tests;
