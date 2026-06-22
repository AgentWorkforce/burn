//! OpenCode session parser — Rust port of `packages/reader/src/opencode.ts`.
//!
//! OpenCode lays sessions out as a directory tree under a storage root:
//!
//! ```text
//! storage/session/<scope>/<sessionId>.json
//! storage/message/<sessionId>/<messageId>.json
//! storage/part/<messageId>/<partId>.json
//! ```
//!
//! The parser reads the session payload, enumerates messages for that session,
//! sorts them chronologically, and walks the assistant messages to emit
//! [`TurnRecord`]s. Tool calls and tool-result events come from the per-message
//! `part/<messageId>/*.json` files; [`UserTurnRecord`]s bridge consecutive
//! assistant turns by combining the previous turn's tool outputs with any user
//! message text preceding the next assistant.
//!
//! Note: the TS parser defaults to a `cl100k` tokenizer for `UserTurnBlock`
//! sizing. The Rust port uses [`HeuristicCounter`] (bytes/4) — see #246 for
//! the cl100k hookup.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::reader::classifier::{classify_activity, ClassificationInput};
use crate::reader::fidelity::classify_fidelity;
use crate::reader::git::ProjectResolver;
use crate::reader::hash::{args_hash, content_hash};
use crate::reader::types::{
    CompactionEvent, ContentKind, ContentRecord, ContentRole, ContentStoreMode, ContentToolResult,
    ContentToolUse, Coverage, Fidelity, RelationshipSourceKind, RelationshipType,
    SessionRelationshipRecord, SourceKind, StopReason, Subagent, ToolCall, ToolResultEventRecord,
    ToolResultEventSource, ToolResultStatus, TurnRecord, Usage, UsageAttribution, UsageGranularity,
    UserTurnBlock, UserTurnRecord,
};
use crate::reader::user_turn::{HeuristicCounter, TokenCounter, UserTurnTokenizer};

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct ParseOpencodeOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub tokenizer: Option<UserTurnTokenizer>,
}

#[derive(Debug, Clone, Default)]
pub struct ParseOpencodeIncrementalOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub tokenizer: Option<UserTurnTokenizer>,
    pub seen_message_ids: Option<BTreeSet<String>>,
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct ParseOpencodeResult {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub events: Vec<CompactionEvent>,
    pub user_turns: Vec<UserTurnRecord>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
}

#[derive(Debug, Clone, Default)]
pub struct ParseOpencodeIncrementalResult {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub events: Vec<CompactionEvent>,
    pub user_turns: Vec<UserTurnRecord>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
    pub seen_message_ids: BTreeSet<String>,
}

#[cfg(test)]
pub fn parse_opencode_session(
    session_file_path: impl AsRef<Path>,
    options: &ParseOpencodeOptions,
) -> std::io::Result<ParseOpencodeResult> {
    let inc_opts = ParseOpencodeIncrementalOptions {
        session_path: options.session_path.clone(),
        content_mode: options.content_mode,
        tokenizer: options.tokenizer,
        seen_message_ids: None,
    };
    parse_opencode_session_incremental(session_file_path, &inc_opts).map(ParseOpencodeResult::from)
}

#[cfg(test)]
impl From<ParseOpencodeIncrementalResult> for ParseOpencodeResult {
    fn from(r: ParseOpencodeIncrementalResult) -> Self {
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

pub fn parse_opencode_session_incremental(
    session_file_path: impl AsRef<Path>,
    options: &ParseOpencodeIncrementalOptions,
) -> std::io::Result<ParseOpencodeIncrementalResult> {
    resolve_token_counter(options.tokenizer)?;

    let session_path_ref = session_file_path.as_ref();
    let session = match read_session(session_path_ref) {
        Some(s) => s,
        None => {
            return Ok(ParseOpencodeIncrementalResult {
                turns: vec![],
                content: vec![],
                events: vec![],
                user_turns: vec![],
                relationships: vec![],
                tool_result_events: vec![],
                seen_message_ids: options.seen_message_ids.clone().unwrap_or_default(),
            });
        }
    };

    let storage_root = derive_storage_root(session_path_ref);

    let messages = read_messages(&storage_root, &session.id);
    let mut assistants: Vec<AssistantMessage> = messages
        .iter()
        .filter_map(parse_complete_assistant)
        .collect();
    let mut users: Vec<UserMessage> = messages.iter().filter_map(parse_complete_user).collect();
    assistants.sort_by_key(|a| a.time_created);
    users.sort_by_key(|u| u.time_created);

    let capture_content = matches!(options.content_mode, Some(ContentStoreMode::Full));
    let counter = HeuristicCounter;
    let is_sidechain = session
        .parent_id
        .as_ref()
        .map(|p| !p.is_empty())
        .unwrap_or(false);

    let mut seen: BTreeSet<String> = options.seen_message_ids.clone().unwrap_or_default();
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut content_out: Vec<ContentRecord> = Vec::new();
    let mut events: Vec<CompactionEvent> = Vec::new();
    let mut user_turns: Vec<UserTurnRecord> = Vec::new();
    let mut tool_result_events: Vec<ToolResultEventRecord> = Vec::new();
    let mut call_index_counters: HashMap<String, u64> = HashMap::new();
    let mut next_event_index: u64 = 0;

    let project_resolver = ProjectResolver::new();

    for i in 0..assistants.len() {
        let m_id = assistants[i].id.clone();
        if seen.contains(&m_id) {
            continue;
        }
        let prev_idx = if i > 0 { Some(i - 1) } else { None };
        let m_time = assistants[i].time_created;
        let user_msg = find_preceding_user(&users, m_time);

        let prev_time = prev_idx.map(|pi| assistants[pi].time_created);
        let user_msg_for_gap = match (&user_msg, prev_time) {
            (Some(um), Some(pt)) if um.time_created <= pt => None,
            (Some(um), _) => Some(um.clone()),
            _ => None,
        };

        let prev_for_build = prev_idx.map(|pi| assistants[pi].clone());
        let user_turn = build_opencode_user_turn_record(
            &storage_root,
            &session.id,
            prev_for_build.as_ref(),
            &assistants[i],
            user_msg_for_gap.as_ref(),
            &counter,
        );
        if let Some(ut) = user_turn {
            user_turns.push(ut);
        }

        let parts = read_parts(&storage_root, &m_id);
        let extracted = extract_tools_and_files(&parts);
        let stop_reason = last_step_finish_reason(&parts);

        let m = &assistants[i];
        let model = build_model(m.provider_id.as_deref(), m.model_id.as_deref());
        let project = m.path_cwd.clone().or_else(|| session.directory.clone());

        let usage = to_usage(m.tokens.as_ref());
        let mut usage_coverage = coverage_from_tokens(m.tokens.as_ref());
        for sf in step_finish_tokens(&parts) {
            usage_coverage =
                merge_usage_coverage(&usage_coverage, &coverage_from_tokens(Some(&sf)));
        }

        let ts = ms_to_iso(m.time_created);
        let mut record = TurnRecord {
            v: 1,
            source: SourceKind::Opencode,
            session_id: m.session_id.clone(),
            session_path: options.session_path.clone(),
            message_id: m.id.clone(),
            turn_index: i as u64,
            ts: ts.clone(),
            model,
            project: None,
            project_key: None,
            usage: usage.clone(),
            tool_calls: extracted.tool_calls.clone(),
            files_touched: if extracted.files_touched.is_empty() {
                None
            } else {
                Some(extracted.files_touched.clone())
            },
            subagent: None,
            stop_reason: stop_reason
                .as_deref()
                .map(|s| StopReason::from_wire(s).unwrap_or(StopReason::Silent)),
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: Some(build_opencode_fidelity(&usage_coverage)),
        };
        if let Some(p) = project.as_ref() {
            let resolved = project_resolver.resolve(p);
            record.project = Some(resolved.project);
            record.project_key = resolved.project_key;
        }
        if is_sidechain {
            record.subagent = Some(Subagent {
                is_sidechain: true,
                parent_tool_use_id: None,
                agent_id: None,
                parent_agent_id: None,
                subagent_type: None,
                description: None,
            });
        }

        let user_text = match &user_msg {
            Some(u) => read_user_text(&storage_root, &u.id),
            None => String::new(),
        };
        let assistant_text = extract_assistant_text(&parts);
        let combined_text = join_nonempty(&[user_text.as_str(), assistant_text.as_str()], "\n");
        let has_failed_tool = extracted
            .tool_calls
            .iter()
            .any(|tc| extracted.errored_call_ids.contains(&tc.id));
        let classified = classify_activity(ClassificationInput {
            tool_calls: &extracted.tool_calls,
            text: &combined_text,
            has_failed_tool,
            reasoning_tokens: usage.reasoning,
        });
        record.activity = Some(classified.activity);
        record.retries = Some(classified.retries);
        record.has_edits = Some(classified.has_edits);

        turns.push(record);
        seen.insert(m_id.clone());

        next_event_index = collect_opencode_tool_result_events(
            &parts,
            &m.session_id,
            &m.id,
            &ts,
            &usage,
            &mut tool_result_events,
            &mut call_index_counters,
            next_event_index,
        );

        if capture_content {
            let assistant_ts = ts.clone();
            if let Some(um) = user_msg.as_ref() {
                let user_ts = ms_to_iso(um.time_created);
                for t in read_user_text_parts(&storage_root, &um.id) {
                    content_out.push(ContentRecord {
                        v: 1,
                        source: SourceKind::Opencode,
                        session_id: m.session_id.clone(),
                        message_id: um.id.clone(),
                        ts: user_ts.clone(),
                        role: ContentRole::User,
                        kind: ContentKind::Text,
                        text: Some(t),
                        tool_use: None,
                        tool_result: None,
                    });
                }
            }
            for rec in extract_assistant_content(&parts, &m.session_id, &m.id, &assistant_ts) {
                content_out.push(rec);
            }
        }
    }

    // Compaction events.
    for u in &users {
        if seen.contains(&u.id) {
            continue;
        }
        let user_parts = read_parts(&storage_root, &u.id);
        if !user_parts.iter().any(|p| p.kind == "compaction") {
            continue;
        }
        let preceding = find_preceding_assistant_by_time(&assistants, u.time_created);
        let mut ev = CompactionEvent {
            v: 1,
            source: SourceKind::Opencode,
            session_id: session.id.clone(),
            ts: ms_to_iso(u.time_created),
            preceding_message_id: None,
            tokens_before_compact: None,
        };
        if let Some(p) = preceding {
            ev.preceding_message_id = Some(p.id.clone());
            ev.tokens_before_compact = Some(to_usage(p.tokens.as_ref()).cache_read);
        }
        events.push(ev);
        seen.insert(u.id.clone());
    }

    let relationships = build_opencode_relationships(&session, &assistants);

    Ok(ParseOpencodeIncrementalResult {
        turns,
        content: content_out,
        events,
        user_turns,
        relationships,
        tool_result_events,
        seen_message_ids: seen,
    })
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SessionInfo {
    id: String,
    parent_id: Option<String>,
    directory: Option<String>,
}

#[derive(Debug, Clone)]
struct MessageTokens {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

#[derive(Debug, Clone)]
struct AssistantMessage {
    id: String,
    session_id: String,
    time_created: i64,
    provider_id: Option<String>,
    model_id: Option<String>,
    path_cwd: Option<String>,
    tokens: Option<MessageTokens>,
}

#[derive(Debug, Clone)]
struct UserMessage {
    id: String,
    #[allow(dead_code)]
    session_id: String,
    time_created: i64,
}

#[derive(Debug, Clone)]
struct ParsedPart {
    id: Option<String>,
    kind: String,
    raw: Map<String, Value>,
}

impl ParsedPart {
    fn get(&self, k: &str) -> Option<&Value> {
        self.raw.get(k)
    }
}

struct ToolPart<'a> {
    call_id: &'a str,
    tool: Option<&'a str>,
    state: Option<&'a Map<String, Value>>,
}

fn as_tool_part(p: &ParsedPart) -> Option<ToolPart<'_>> {
    if p.kind != "tool" {
        return None;
    }
    let call_id = p.get("callID")?.as_str()?;
    if call_id.is_empty() {
        return None;
    }
    let tool = p.get("tool").and_then(|v| v.as_str());
    let state = p.get("state").and_then(|v| v.as_object());
    Some(ToolPart {
        call_id,
        tool,
        state,
    })
}

fn is_terminal_tool(p: &ToolPart<'_>) -> bool {
    match p.state {
        Some(s) => s.contains_key("output"),
        None => false,
    }
}

struct Extracted {
    tool_calls: Vec<ToolCall>,
    files_touched: Vec<String>,
    errored_call_ids: BTreeSet<String>,
}

fn extract_tools_and_files(parts: &[ParsedPart]) -> Extracted {
    let mut tool_calls = Vec::new();
    let mut seen = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut errored = BTreeSet::new();
    for p in parts {
        let Some(tp) = as_tool_part(p) else { continue };
        let tool = match tp.tool {
            Some(t) => t,
            None => continue,
        };
        if seen.contains(tp.call_id) {
            continue;
        }
        seen.insert(tp.call_id.to_string());
        let input_val = tp
            .state
            .and_then(|s| s.get("input"))
            .cloned()
            .unwrap_or(Value::Object(Map::new()));
        let input_obj = match &input_val {
            Value::Object(_) => input_val.clone(),
            _ => Value::Object(Map::new()),
        };
        let mut call = ToolCall {
            id: tp.call_id.to_string(),
            name: tool.to_string(),
            target: pick_target(tool, &input_obj),
            args_hash: args_hash(&input_obj),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        };
        if tool == "skill" {
            if let Value::Object(input_map) = &input_obj {
                for k in ["skill", "name", "skill_name"] {
                    if let Some(v) = input_map.get(k).and_then(|x| x.as_str()) {
                        call.skill_name = Some(v.to_string());
                        break;
                    }
                }
            }
        }
        if let Some(target) = call.target.clone() {
            if is_file_tool(tool) {
                files.insert(target);
            }
        }
        if is_failed_tool(tp.state) {
            errored.insert(tp.call_id.to_string());
        }
        tool_calls.push(call);
    }
    Extracted {
        tool_calls,
        files_touched: files.into_iter().collect(),
        errored_call_ids: errored,
    }
}

fn pick_target(name: &str, input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    let s =
        |k: &str| -> Option<String> { obj.get(k).and_then(|v| v.as_str()).map(|x| x.to_string()) };
    match name {
        "read" | "write" | "edit" => s("filePath")
            .or_else(|| s("file_path"))
            .or_else(|| s("path")),
        "bash" => s("command"),
        "grep" => s("pattern"),
        "glob" => s("pattern"),
        "webfetch" => s("url"),
        "task" => s("subagent_type")
            .or_else(|| s("description"))
            .or_else(|| s("prompt")),
        _ => s("filePath")
            .or_else(|| s("file_path"))
            .or_else(|| s("path"))
            .or_else(|| s("url"))
            .or_else(|| s("command")),
    }
}

fn is_file_tool(name: &str) -> bool {
    matches!(name, "read" | "write" | "edit")
}

fn is_failed_tool(state: Option<&Map<String, Value>>) -> bool {
    let Some(s) = state else {
        return false;
    };
    if s.get("status").and_then(|v| v.as_str()) == Some("error") {
        return true;
    }
    let exit = s
        .get("metadata")
        .and_then(|m| m.as_object())
        .and_then(|m| m.get("exit"))
        .and_then(|v| v.as_i64());
    matches!(exit, Some(e) if e != 0)
}

fn last_step_finish_reason(parts: &[ParsedPart]) -> Option<String> {
    for p in parts.iter().rev() {
        if p.kind == "step-finish" {
            if let Some(r) = p.get("reason").and_then(|v| v.as_str()) {
                return Some(r.to_string());
            }
        }
    }
    None
}

fn step_finish_tokens(parts: &[ParsedPart]) -> Vec<MessageTokens> {
    let mut out = Vec::new();
    for p in parts {
        if p.kind != "step-finish" {
            continue;
        }
        if let Some(tokens_val) = p.get("tokens") {
            out.push(parse_tokens(Some(tokens_val)));
        }
    }
    out
}

fn extract_assistant_text(parts: &[ParsedPart]) -> String {
    let mut chunks = Vec::new();
    for p in parts {
        if p.kind != "text" {
            continue;
        }
        if p.get("synthetic").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
            if !t.is_empty() {
                chunks.push(t.to_string());
            }
        }
    }
    chunks.join("\n")
}

fn read_user_text(storage_root: &Path, user_message_id: &str) -> String {
    let parts = read_parts(storage_root, user_message_id);
    let mut chunks = Vec::new();
    for p in parts {
        if p.kind != "text" {
            continue;
        }
        if p.get("synthetic").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
            if !t.is_empty() {
                chunks.push(t.to_string());
            }
        }
    }
    chunks.join("\n")
}

fn read_user_text_parts(storage_root: &Path, user_message_id: &str) -> Vec<String> {
    let parts = read_parts(storage_root, user_message_id);
    let mut out = Vec::new();
    for p in parts {
        if p.kind != "text" {
            continue;
        }
        if p.get("synthetic").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    out
}

fn extract_assistant_content(
    parts: &[ParsedPart],
    session_id: &str,
    message_id: &str,
    ts: &str,
) -> Vec<ContentRecord> {
    let mut out = Vec::new();
    for p in parts {
        if p.kind == "text" {
            if p.get("synthetic").and_then(|v| v.as_bool()) == Some(true) {
                continue;
            }
            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    out.push(ContentRecord {
                        v: 1,
                        source: SourceKind::Opencode,
                        session_id: session_id.to_string(),
                        message_id: message_id.to_string(),
                        ts: ts.to_string(),
                        role: ContentRole::Assistant,
                        kind: ContentKind::Text,
                        text: Some(t.to_string()),
                        tool_use: None,
                        tool_result: None,
                    });
                }
            }
            continue;
        }
        if p.kind == "tool" {
            let Some(tp) = as_tool_part(p) else { continue };
            let Some(tool) = tp.tool else { continue };
            let input_val = tp
                .state
                .and_then(|s| s.get("input"))
                .cloned()
                .unwrap_or(Value::Object(Map::new()));
            let input_map = match input_val {
                Value::Object(m) => m.into_iter().collect(),
                _ => Default::default(),
            };
            out.push(ContentRecord {
                v: 1,
                source: SourceKind::Opencode,
                session_id: session_id.to_string(),
                message_id: message_id.to_string(),
                ts: ts.to_string(),
                role: ContentRole::Assistant,
                kind: ContentKind::ToolUse,
                text: None,
                tool_use: Some(ContentToolUse {
                    id: tp.call_id.to_string(),
                    name: tool.to_string(),
                    input: input_map,
                }),
                tool_result: None,
            });
            if let Some(state) = tp.state {
                if state.contains_key("output") {
                    let output = state
                        .get("output")
                        .cloned()
                        .unwrap_or(Value::String(String::new()));
                    let mut is_error = None;
                    if state.get("status").and_then(|v| v.as_str()) == Some("error") {
                        is_error = Some(true);
                    } else {
                        let exit = state
                            .get("metadata")
                            .and_then(|m| m.as_object())
                            .and_then(|m| m.get("exit"))
                            .and_then(|v| v.as_i64());
                        if matches!(exit, Some(e) if e != 0) {
                            is_error = Some(true);
                        }
                    }
                    out.push(ContentRecord {
                        v: 1,
                        source: SourceKind::Opencode,
                        session_id: session_id.to_string(),
                        message_id: message_id.to_string(),
                        ts: ts.to_string(),
                        role: ContentRole::ToolResult,
                        kind: ContentKind::ToolResult,
                        text: None,
                        tool_use: None,
                        tool_result: Some(ContentToolResult {
                            tool_use_id: tp.call_id.to_string(),
                            content: output,
                            is_error,
                        }),
                    });
                }
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn collect_opencode_tool_result_events(
    parts: &[ParsedPart],
    session_id: &str,
    message_id: &str,
    ts: &str,
    turn_usage: &Usage,
    out: &mut Vec<ToolResultEventRecord>,
    call_index_counters: &mut HashMap<String, u64>,
    start_event_index: u64,
) -> u64 {
    let mut next_index = start_event_index;
    let terminals: Vec<ToolPart<'_>> = parts
        .iter()
        .filter_map(as_tool_part)
        .filter(is_terminal_tool)
        .collect();
    let n = terminals.len() as u64;
    let usage_attribution = match terminals.len() {
        1 => Some(UsageAttribution::SingleToolTurn),
        x if x > 1 => Some(UsageAttribution::EvenSplitTurn),
        _ => None,
    };
    let mut terminal_idx: u64 = 0;
    for p in parts {
        let Some(tp) = as_tool_part(p) else { continue };
        if !is_terminal_tool(&tp) {
            continue;
        }
        let state = tp.state.unwrap();
        let is_error = is_failed_tool(Some(state));
        let call_id = tp.call_id.to_string();
        let call_index = *call_index_counters.get(&call_id).unwrap_or(&0);
        call_index_counters.insert(call_id.clone(), call_index + 1);
        let measured = measure_opencode_tool_output(state.get("output"));
        let mut record = ToolResultEventRecord {
            v: 1,
            source: SourceKind::Opencode,
            session_id: session_id.to_string(),
            message_id: Some(message_id.to_string()),
            tool_use_id: call_id,
            call_index: Some(call_index),
            event_index: next_index,
            ts: Some(ts.to_string()),
            status: if is_error {
                ToolResultStatus::Errored
            } else {
                ToolResultStatus::Completed
            },
            event_source: ToolResultEventSource::ToolResult,
            content_length: measured.length,
            output_bytes: measured.byte_length,
            output_truncated: None,
            content_hash: measured.hash,
            is_error: if is_error { Some(true) } else { None },
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        };
        next_index += 1;
        if n >= 1 {
            record.usage = Some(per_tool_usage_share(turn_usage, n, terminal_idx));
            record.usage_attribution = usage_attribution;
        }
        out.push(record);
        terminal_idx += 1;
    }
    next_index
}

/// Per-tool slice of an even-split usage. The TS port uses floating-point
/// division, which preserves the sum at the cost of fractional usage values;
/// `Usage` is `u64` here, so we instead distribute the integer remainder one
/// extra unit at a time to the first `total % n` tools. The sum of shares
/// across the turn equals the original total exactly, which keeps downstream
/// aggregations (cost, summary) honest when the totals don't divide evenly.
fn per_tool_usage_share(total: &Usage, n: u64, idx: u64) -> Usage {
    Usage {
        input: split_field(total.input, n, idx),
        output: split_field(total.output, n, idx),
        reasoning: split_field(total.reasoning, n, idx),
        cache_read: split_field(total.cache_read, n, idx),
        cache_create_5m: split_field(total.cache_create_5m, n, idx),
        cache_create_1h: split_field(total.cache_create_1h, n, idx),
    }
}

fn split_field(total: u64, n: u64, idx: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    if n == 1 {
        return total;
    }
    let base = total / n;
    let remainder = total % n;
    if idx < remainder {
        base + 1
    } else {
        base
    }
}

#[derive(Default)]
struct Measured {
    length: Option<u64>,
    hash: Option<String>,
    /// Raw UTF-8 byte length of the materialized payload (#436). Same as
    /// `length` for opencode (which already measured bytes), tracked
    /// separately so the `ToolResultEventRecord` shape stays consistent.
    byte_length: Option<u64>,
}

fn measure_opencode_tool_output(output: Option<&Value>) -> Measured {
    match output {
        None | Some(Value::Null) => Measured::default(),
        Some(Value::String(s)) => Measured {
            length: Some(s.len() as u64),
            hash: Some(content_hash(s)),
            byte_length: Some(s.len() as u64),
        },
        Some(other) => match serde_json::to_string(other) {
            Ok(serialized) => Measured {
                length: Some(serialized.len() as u64),
                hash: Some(content_hash(&serialized)),
                byte_length: Some(serialized.len() as u64),
            },
            Err(_) => Measured::default(),
        },
    }
}

fn build_opencode_relationships(
    session: &SessionInfo,
    assistants: &[AssistantMessage],
) -> Vec<SessionRelationshipRecord> {
    let mut out = Vec::new();
    let first_ts = assistants.first().map(|a| ms_to_iso(a.time_created));
    let mut root = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::Opencode,
        session_id: session.id.clone(),
        related_session_id: None,
        relationship_type: RelationshipType::Root,
        ts: None,
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    if let Some(t) = first_ts.clone() {
        root.ts = Some(t);
    }
    out.push(root);
    if let Some(parent) = session.parent_id.as_ref() {
        if !parent.is_empty() {
            let mut sub = SessionRelationshipRecord {
                v: 1,
                source: RelationshipSourceKind::NativeOpencode,
                session_id: session.id.clone(),
                related_session_id: Some(parent.clone()),
                relationship_type: RelationshipType::Subagent,
                ts: None,
                source_session_id: None,
                source_version: None,
                parent_tool_use_id: None,
                agent_id: None,
                subagent_type: None,
                description: None,
            };
            if let Some(t) = first_ts {
                sub.ts = Some(t);
            }
            out.push(sub);
        }
    }
    out
}

fn build_opencode_user_turn_record<C: TokenCounter + ?Sized>(
    storage_root: &Path,
    session_id: &str,
    prev: Option<&AssistantMessage>,
    next: &AssistantMessage,
    user_msg: Option<&UserMessage>,
    counter: &C,
) -> Option<UserTurnRecord> {
    let mut blocks: Vec<UserTurnBlock> = Vec::new();

    if let Some(prev) = prev {
        let prev_parts = read_parts(storage_root, &prev.id);
        for p in &prev_parts {
            let Some(tp) = as_tool_part(p) else { continue };
            let Some(state) = tp.state else { continue };
            if !state.contains_key("output") {
                continue;
            }
            let is_error = is_failed_tool(Some(state));
            let output = state
                .get("output")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            blocks.push(UserTurnBlock::tool_result(
                tp.call_id.to_string(),
                &output,
                if is_error { Some(true) } else { None },
                counter,
            ));
        }
    }

    let mut ts = user_msg
        .map(|u| ms_to_iso(u.time_created))
        .unwrap_or_default();
    if let Some(um) = user_msg {
        let user_parts = read_parts(storage_root, &um.id);
        for p in &user_parts {
            if p.kind != "text" {
                continue;
            }
            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    blocks.push(UserTurnBlock::text(t, counter));
                }
            }
        }
    }

    if blocks.is_empty() {
        return None;
    }
    if ts.is_empty() {
        ts = ms_to_iso(next.time_created);
    }
    let user_uuid = match user_msg {
        Some(um) => um.id.clone(),
        None => format!(
            "{}:{}->{}",
            session_id,
            prev.map(|p| p.id.as_str()).unwrap_or("start"),
            next.id
        ),
    };
    let mut record = UserTurnRecord {
        v: 1,
        source: SourceKind::Opencode,
        session_id: session_id.to_string(),
        user_uuid,
        ts,
        preceding_message_id: None,
        following_message_id: Some(next.id.clone()),
        blocks,
    };
    if let Some(prev) = prev {
        record.preceding_message_id = Some(prev.id.clone());
    }
    Some(record)
}

// ---------------------------------------------------------------------------
// Disk I/O helpers
// ---------------------------------------------------------------------------

fn derive_storage_root(session_file_path: &Path) -> PathBuf {
    let mut p = session_file_path.to_path_buf();
    for _ in 0..3 {
        if !p.pop() {
            break;
        }
    }
    p
}

fn read_session(session_file_path: &Path) -> Option<SessionInfo> {
    let raw = fs::read_to_string(session_file_path).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let obj = parsed.as_object()?;
    let id = obj.get("id")?.as_str()?.to_string();
    Some(SessionInfo {
        id,
        parent_id: obj
            .get("parentID")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        directory: obj
            .get("directory")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

fn read_messages(storage_root: &Path, session_id: &str) -> Vec<Map<String, Value>> {
    let dir = storage_root.join("message").join(session_id);
    let entries = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    names.sort();
    let mut out = Vec::new();
    for path in names {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Value::Object(obj) = parsed {
            let has_role = obj.get("role").and_then(|v| v.as_str()).is_some();
            let has_id = obj.get("id").and_then(|v| v.as_str()).is_some();
            if has_role && has_id {
                out.push(obj);
            }
        }
    }
    out
}

fn read_parts(storage_root: &Path, message_id: &str) -> Vec<ParsedPart> {
    let dir = storage_root.join("part").join(message_id);
    let entries = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    let mut parts: Vec<ParsedPart> = Vec::new();
    for path in paths {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Value::Object(obj) = parsed {
            let kind = obj
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let id = obj.get("id").and_then(|v| v.as_str()).map(str::to_string);
            parts.push(ParsedPart { id, kind, raw: obj });
        }
    }
    parts.sort_by(|a, b| {
        a.id.as_deref()
            .unwrap_or("")
            .cmp(b.id.as_deref().unwrap_or(""))
    });
    parts
}

// ---------------------------------------------------------------------------
// Message parsing
// ---------------------------------------------------------------------------

fn parse_complete_assistant(m: &Map<String, Value>) -> Option<AssistantMessage> {
    if m.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return None;
    }
    let id = m.get("id")?.as_str()?.to_string();
    let session_id = m.get("sessionID")?.as_str()?.to_string();
    let time_created = m
        .get("time")
        .and_then(|v| v.as_object())
        .and_then(|t| t.get("created"))
        .and_then(|v| v.as_i64())?;
    let provider_id = m
        .get("providerID")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let model_id = m
        .get("modelID")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let path_cwd = m
        .get("path")
        .and_then(|v| v.as_object())
        .and_then(|p| p.get("cwd"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let tokens = m.get("tokens").map(|t| parse_tokens(Some(t)));
    Some(AssistantMessage {
        id,
        session_id,
        time_created,
        provider_id,
        model_id,
        path_cwd,
        tokens,
    })
}

fn parse_complete_user(m: &Map<String, Value>) -> Option<UserMessage> {
    if m.get("role").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    let id = m.get("id")?.as_str()?.to_string();
    let session_id = m.get("sessionID")?.as_str()?.to_string();
    let time_created = m
        .get("time")
        .and_then(|v| v.as_object())
        .and_then(|t| t.get("created"))
        .and_then(|v| v.as_i64())?;
    Some(UserMessage {
        id,
        session_id,
        time_created,
    })
}

fn parse_tokens(v: Option<&Value>) -> MessageTokens {
    let Some(v) = v.and_then(|x| x.as_object()) else {
        return MessageTokens {
            input: None,
            output: None,
            reasoning: None,
            cache_read: None,
            cache_write: None,
        };
    };
    let cache = v.get("cache").and_then(|x| x.as_object());
    MessageTokens {
        input: v.get("input").and_then(|x| x.as_u64()),
        output: v.get("output").and_then(|x| x.as_u64()),
        reasoning: v.get("reasoning").and_then(|x| x.as_u64()),
        cache_read: cache.and_then(|c| c.get("read")).and_then(|x| x.as_u64()),
        cache_write: cache.and_then(|c| c.get("write")).and_then(|x| x.as_u64()),
    }
}

fn to_usage(t: Option<&MessageTokens>) -> Usage {
    let input = t.and_then(|t| t.input).unwrap_or(0);
    let output = t.and_then(|t| t.output).unwrap_or(0);
    let reasoning = t.and_then(|t| t.reasoning).unwrap_or(0);
    let cache_read = t.and_then(|t| t.cache_read).unwrap_or(0);
    let cache_write = t.and_then(|t| t.cache_write).unwrap_or(0);
    Usage {
        input,
        output,
        reasoning,
        cache_read,
        cache_create_5m: cache_write,
        cache_create_1h: 0,
    }
}

#[derive(Debug, Clone, Default)]
struct OpencodeUsageCoverage {
    has_input_tokens: bool,
    has_output_tokens: bool,
    has_reasoning_tokens: bool,
    has_cache_read_tokens: bool,
    has_cache_create_tokens: bool,
}

fn coverage_from_tokens(t: Option<&MessageTokens>) -> OpencodeUsageCoverage {
    let Some(t) = t else {
        return OpencodeUsageCoverage::default();
    };
    OpencodeUsageCoverage {
        has_input_tokens: t.input.is_some(),
        has_output_tokens: t.output.is_some(),
        has_reasoning_tokens: t.reasoning.is_some(),
        has_cache_read_tokens: t.cache_read.is_some(),
        has_cache_create_tokens: t.cache_write.is_some(),
    }
}

fn merge_usage_coverage(
    a: &OpencodeUsageCoverage,
    b: &OpencodeUsageCoverage,
) -> OpencodeUsageCoverage {
    OpencodeUsageCoverage {
        has_input_tokens: a.has_input_tokens || b.has_input_tokens,
        has_output_tokens: a.has_output_tokens || b.has_output_tokens,
        has_reasoning_tokens: a.has_reasoning_tokens || b.has_reasoning_tokens,
        has_cache_read_tokens: a.has_cache_read_tokens || b.has_cache_read_tokens,
        has_cache_create_tokens: a.has_cache_create_tokens || b.has_cache_create_tokens,
    }
}

fn build_opencode_fidelity(usage_coverage: &OpencodeUsageCoverage) -> Fidelity {
    let coverage = Coverage {
        has_input_tokens: usage_coverage.has_input_tokens,
        has_output_tokens: usage_coverage.has_output_tokens,
        has_reasoning_tokens: usage_coverage.has_reasoning_tokens,
        has_cache_read_tokens: usage_coverage.has_cache_read_tokens,
        has_cache_create_tokens: usage_coverage.has_cache_create_tokens,
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

fn build_model(provider_id: Option<&str>, model_id: Option<&str>) -> String {
    match (provider_id, model_id) {
        (Some(p), Some(m)) if !p.is_empty() && !m.is_empty() => format!("{}/{}", p, m),
        (_, Some(m)) => m.to_string(),
        (Some(p), _) => p.to_string(),
        _ => String::new(),
    }
}

fn find_preceding_user(users: &[UserMessage], ts_created: i64) -> Option<UserMessage> {
    let mut best: Option<UserMessage> = None;
    for u in users {
        if u.time_created <= ts_created {
            best = Some(u.clone());
        } else {
            break;
        }
    }
    best
}

fn find_preceding_assistant_by_time(
    assistants: &[AssistantMessage],
    ts: i64,
) -> Option<&AssistantMessage> {
    let mut best: Option<&AssistantMessage> = None;
    for a in assistants {
        if a.time_created < ts {
            best = Some(a);
        } else {
            break;
        }
    }
    best
}

fn join_nonempty(parts: &[&str], sep: &str) -> String {
    let mut out: Vec<&str> = Vec::with_capacity(parts.len());
    for p in parts {
        if !p.is_empty() {
            out.push(p);
        }
    }
    out.join(sep)
}

fn ms_to_iso(ms: i64) -> String {
    // `new Date(ms).toISOString()` — UTC with millisecond precision.
    const MS_PER_DAY: i64 = 86_400_000;
    let total_days_since_epoch = ms.div_euclid(MS_PER_DAY);
    let ms_in_day = ms.rem_euclid(MS_PER_DAY);
    let (y, mo, d) = crate::util::time::days_to_ymd(total_days_since_epoch);
    let h = ms_in_day / 3_600_000;
    let m = (ms_in_day / 60_000) % 60;
    let s = (ms_in_day / 1_000) % 60;
    let frac = ms_in_day % 1_000;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, h, m, s, frac
    )
}

fn resolve_token_counter(
    tokenizer: Option<UserTurnTokenizer>,
) -> std::io::Result<HeuristicCounter> {
    match tokenizer {
        None | Some(UserTurnTokenizer::Heuristic) => Ok(HeuristicCounter),
        Some(UserTurnTokenizer::Cl100k) => Err(std::io::Error::other(
            "cl100k tokenizer is not yet available in the Rust port; \
             omit `tokenizer` or pass `Some(Heuristic)` (see AgentWorkforce/burn#246)",
        )),
    }
}

#[cfg(test)]
mod tests;
