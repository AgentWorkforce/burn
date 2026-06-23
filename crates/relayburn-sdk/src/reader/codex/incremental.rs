//! Codex incremental-parse engine.
//!
//! Mechanically split out of `reader/codex.rs` to shrink the oversized root
//! file. Holds the per-line streaming state machine (`CodexParseState`), its
//! parallel committed shadow (`CommittedSnapshot`), and the driver
//! (`parse_codex_buffer`) that the public `parse_codex_session*` entry points
//! wrap. Behavior is unchanged from the inline implementation; only module
//! placement, visibility, and imports differ.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::BufRead;

use serde_json::Value;

use crate::reader::classifier::{classify_activity, ClassificationInput};
use crate::reader::git::ProjectResolver;
use crate::reader::hash::args_hash;
use crate::reader::types::{
    CompactionEvent, ContentKind, ContentRecord, ContentRole, ContentStoreMode, ContentToolResult,
    ContentToolUse, SessionRelationshipRecord, SourceKind, ToolCall, ToolResultEventRecord,
    ToolResultEventSource, ToolResultStatus, TurnRecord, UserTurnBlock, UserTurnBlockKind,
    UserTurnRecord,
};
use crate::reader::user_turn::{join_nonempty, HeuristicCounter};

use super::{
    append_text, build_codex_compaction_event, build_codex_user_turn_record,
    build_root_relationship, build_session_meta_relationships, codex_relationship_key,
    collect_message_text, collect_reasoning_text, extract_spawned_agent_id, finalize_turn,
    is_subagent_terminal_notification, maybe_emit_spawn_relationship, measure_tool_output,
    pick_custom_tool_target, pick_function_call_target, pick_string_field, push_content,
    safe_parse_json_object, session_meta_payload_id, subagent_notification_status,
    CodexLastCompletedTurn, CodexResumeState, CodexTurnContext, CumulativeUsage, FinalizedTurn,
    OpenTurn, ParseCodexIncrementalOptions, ParseCodexIncrementalResult, SpawnCallInfo,
    UserTurnSlot,
};

#[derive(Debug, Clone)]
struct Pending<T> {
    offset: u64,
    record: T,
}

/// Owns the mutable WORKING state of the Codex parse pass. The driver
/// (`parse_codex_buffer`) keeps the parallel `committed_*` shadow set and
/// snapshots fields off this struct at `task_complete` commit boundaries, so
/// a trailing partial/un-terminated line's mutations are discarded. The final
/// result is assembled from the committed snapshots, never from this working
/// state directly.
struct CodexParseState {
    session_id: String,
    session_cwd: Option<String>,
    turn_contexts: HashMap<String, CodexTurnContext>,
    cumulative: CumulativeUsage,
    open_turn: Option<OpenTurn>,
    pending_user_text: String,
    pending_content: Vec<ContentRecord>,
    finalized: Vec<FinalizedTurn>,
    user_turn_slot: UserTurnSlot,
    user_turns: Vec<UserTurnRecord>,
    root_session_emitted: bool,
    seen_session_meta_keys: BTreeSet<String>,
    next_event_index: u64,
    tool_result_counters: HashMap<String, u64>,
    last_completed_turn: Option<CodexLastCompletedTurn>,
    pending_tool_result_events: Vec<Pending<ToolResultEventRecord>>,
    pending_relationships: Vec<Pending<SessionRelationshipRecord>>,
    pending_compactions: Vec<Pending<CompactionEvent>>,
}

impl CodexParseState {
    /// Initialize working state from the resume option, mirroring the prior
    /// inline `resume.map(...)` initializers verbatim.
    fn new(resume: Option<&CodexResumeState>) -> Self {
        Self {
            session_id: resume.map(|r| r.session_id.clone()).unwrap_or_default(),
            session_cwd: resume.and_then(|r| r.session_cwd.clone()),
            turn_contexts: resume.map(|r| r.turn_contexts.clone()).unwrap_or_default(),
            cumulative: resume.map(|r| r.cumulative.clone()).unwrap_or_default(),
            open_turn: None,
            pending_user_text: String::new(),
            pending_content: Vec::new(),
            finalized: Vec::new(),
            user_turn_slot: resume
                .and_then(|r| r.user_turn_slot.as_ref())
                .map(UserTurnSlot::from_persisted)
                .unwrap_or_default(),
            user_turns: Vec::new(),
            root_session_emitted: resume.map(|r| r.root_session_emitted).unwrap_or(false),
            seen_session_meta_keys: resume
                .map(|r| r.session_meta_relationship_keys.iter().cloned().collect())
                .unwrap_or_default(),
            next_event_index: resume.map(|r| r.next_event_index).unwrap_or(0),
            tool_result_counters: resume
                .map(|r| r.tool_result_counters.clone())
                .unwrap_or_default(),
            last_completed_turn: resume.and_then(|r| r.last_completed_turn.clone()),
            pending_tool_result_events: Vec::new(),
            pending_relationships: Vec::new(),
            pending_compactions: Vec::new(),
        }
    }

    fn handle_session_meta(&mut self, payload: &Value, rec_timestamp: &str, line_end_offset: u64) {
        if let Some(id) = session_meta_payload_id(payload) {
            self.session_id = id;
        }
        if let Some(cwd) = payload.get("cwd").and_then(|v| v.as_str()) {
            self.session_cwd = Some(cwd.to_string());
            if let Some(open) = self.open_turn.as_mut() {
                if open.project.is_none() {
                    open.project = Some(cwd.to_string());
                }
            }
        }
        if !self.session_id.is_empty() && !self.root_session_emitted {
            self.root_session_emitted = true;
            let ts = payload
                .get("timestamp")
                .and_then(|v| v.as_str())
                .unwrap_or(rec_timestamp);
            self.pending_relationships.push(Pending {
                offset: line_end_offset,
                record: build_root_relationship(&self.session_id, ts, payload),
            });
        }
        if !self.session_id.is_empty() {
            for row in build_session_meta_relationships(&self.session_id, payload, rec_timestamp) {
                let key = codex_relationship_key(&row);
                if self.seen_session_meta_keys.contains(&key) {
                    continue;
                }
                self.seen_session_meta_keys.insert(key);
                self.pending_relationships.push(Pending {
                    offset: line_end_offset,
                    record: row,
                });
            }
        }
    }

    fn handle_turn_context(&mut self, payload: &Value) {
        let ctx = CodexTurnContext {
            turn_id: payload
                .get("turn_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            cwd: payload
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            model: payload
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        };
        if let Some(tid) = ctx.turn_id.clone() {
            self.turn_contexts.insert(tid.clone(), ctx.clone());
            if let Some(open) = self.open_turn.as_mut() {
                if open.turn_id == tid {
                    if open.model.is_empty() {
                        if let Some(m) = ctx.model.as_deref() {
                            open.model = m.to_string();
                        }
                    }
                    if open.project.is_none() {
                        if let Some(c) = ctx.cwd.as_deref() {
                            open.project = Some(c.to_string());
                        }
                    }
                }
            }
        }
    }

    fn handle_compacted(&mut self, rec_timestamp: &str, line_end_offset: u64) {
        if !self.session_id.is_empty() {
            self.pending_compactions.push(Pending {
                offset: line_end_offset,
                record: build_codex_compaction_event(
                    &self.session_id,
                    rec_timestamp,
                    self.last_completed_turn.as_ref(),
                ),
            });
        }
    }

    fn handle_event_msg(
        &mut self,
        payload: &Value,
        rec_timestamp: &str,
        capture_content: bool,
        line_end_offset: u64,
        committed: &mut CommittedSnapshot,
    ) {
        let pl_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match pl_type {
            "token_count" => {
                if let Some(total) = payload.get("info").and_then(|i| {
                    if i.is_null() {
                        None
                    } else {
                        i.get("total_token_usage")
                    }
                }) {
                    let input_total = total
                        .get("input_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let cached = total
                        .get("cached_input_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    self.cumulative.input = input_total - cached;
                    self.cumulative.cache_read = cached;
                    self.cumulative.output = total
                        .get("output_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    self.cumulative.reasoning = total
                        .get("reasoning_output_tokens")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    if let Some(open) = self.open_turn.as_mut() {
                        open.usage_observed = true;
                    }
                }
            }
            "task_started" => {
                let ts = rec_timestamp;
                let turn_id = match payload.get("turn_id").and_then(|v| v.as_str()) {
                    Some(t) => t.to_string(),
                    None => return,
                };
                if let Some(open) = self.open_turn.take() {
                    self.finalized.push(finalize_turn(open, &self.cumulative));
                }
                if !self.user_turn_slot.blocks.is_empty() {
                    self.user_turns.push(build_codex_user_turn_record(
                        &self.user_turn_slot,
                        &self.session_id,
                        &turn_id,
                        ts,
                    ));
                }
                self.user_turn_slot = UserTurnSlot::default();
                let ctx = self.turn_contexts.get(&turn_id).cloned();
                let project = ctx
                    .as_ref()
                    .and_then(|c| c.cwd.clone())
                    .or_else(|| self.session_cwd.clone());
                let mut open = OpenTurn {
                    turn_id: turn_id.clone(),
                    ts: ts.to_string(),
                    model: ctx
                        .as_ref()
                        .and_then(|c| c.model.clone())
                        .unwrap_or_default(),
                    project,
                    start_cumulative: self.cumulative.clone(),
                    tool_calls: vec![],
                    seen_call_ids: BTreeSet::new(),
                    files_touched: BTreeSet::new(),
                    user_text: std::mem::take(&mut self.pending_user_text),
                    assistant_text: String::new(),
                    errored_call_ids: BTreeSet::new(),
                    content: vec![],
                    pending_tool_result_events: vec![],
                    pending_relationships: vec![],
                    spawn_calls: HashMap::new(),
                    usage_observed: false,
                };
                if capture_content && !self.pending_content.is_empty() {
                    for c in self.pending_content.iter_mut() {
                        c.message_id = turn_id.clone();
                    }
                    open.content.append(&mut self.pending_content);
                }
                self.open_turn = Some(open);
            }
            "task_complete" => {
                let payload_turn_id = payload
                    .get("turn_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mut took: Option<OpenTurn> = None;
                if let Some(open) = self.open_turn.as_ref() {
                    if open.turn_id == payload_turn_id {
                        took = self.open_turn.take();
                    }
                }
                if let Some(mut open) = took {
                    // Patch isError on tool-result blocks accumulated this turn
                    for b in self.user_turn_slot.blocks.iter_mut() {
                        if matches!(b.kind, UserTurnBlockKind::ToolResult) {
                            if let Some(id) = &b.tool_use_id {
                                if open.errored_call_ids.contains(id) {
                                    b.is_error = Some(true);
                                }
                            }
                        }
                    }
                    // Drain pending tool result events / relationships
                    let mut events = std::mem::take(&mut open.pending_tool_result_events);
                    for ev in events.iter_mut() {
                        if open.errored_call_ids.contains(&ev.tool_use_id) {
                            ev.status = ToolResultStatus::Errored;
                            ev.is_error = Some(true);
                        } else if matches!(ev.status, ToolResultStatus::Unknown) {
                            ev.status = ToolResultStatus::Completed;
                        }
                    }
                    for ev in events {
                        self.pending_tool_result_events.push(Pending {
                            offset: line_end_offset,
                            record: ev,
                        });
                    }
                    let rels = std::mem::take(&mut open.pending_relationships);
                    for r in rels {
                        self.pending_relationships.push(Pending {
                            offset: line_end_offset,
                            record: r,
                        });
                    }
                    self.user_turn_slot.preceding_message_id = Some(open.turn_id.clone());
                    let closed = finalize_turn(open, &self.cumulative);
                    self.last_completed_turn = Some(CodexLastCompletedTurn {
                        message_id: closed.turn_id.clone(),
                        cache_read: closed.usage.cache_read,
                    });
                    self.finalized.push(closed);
                    // Commit snapshot
                    committed.snapshot(self, line_end_offset);
                }
            }
            "patch_apply_end" => {
                if let Some(open) = self.open_turn.as_mut() {
                    let turn_id = payload.get("turn_id").and_then(|v| v.as_str());
                    if turn_id != Some(open.turn_id.as_str()) {
                        return;
                    }
                    let success = payload.get("success").and_then(|v| v.as_bool());
                    if success == Some(false) {
                        if let Some(call_id) = payload.get("call_id").and_then(|v| v.as_str()) {
                            open.errored_call_ids.insert(call_id.to_string());
                        }
                        return;
                    }
                    if let Some(changes) = payload.get("changes").and_then(|v| v.as_object()) {
                        for file in changes.keys() {
                            open.files_touched.insert(file.clone());
                        }
                    }
                }
            }
            "exec_command_end" => {
                if let Some(open) = self.open_turn.as_mut() {
                    let turn_id = payload.get("turn_id").and_then(|v| v.as_str());
                    if turn_id != Some(open.turn_id.as_str()) {
                        return;
                    }
                    let exit_code = payload.get("exit_code").and_then(|v| v.as_i64());
                    if let (Some(code), Some(call_id)) =
                        (exit_code, payload.get("call_id").and_then(|v| v.as_str()))
                    {
                        if code != 0 {
                            open.errored_call_ids.insert(call_id.to_string());
                        }
                    }
                }
            }
            other if is_subagent_terminal_notification(other) => {
                let call_id = match payload.get("call_id").and_then(|v| v.as_str()) {
                    Some(c) if !c.is_empty() => c.to_string(),
                    _ => return,
                };
                let entry = self
                    .tool_result_counters
                    .entry(call_id.clone())
                    .or_insert(0);
                let call_index = *entry;
                *entry += 1;
                let status = subagent_notification_status(payload);
                let mut ev = ToolResultEventRecord {
                    v: 1,
                    source: SourceKind::Codex,
                    session_id: self.session_id.clone(),
                    message_id: self.open_turn.as_ref().map(|o| o.turn_id.clone()),
                    tool_use_id: call_id.clone(),
                    call_index: Some(call_index),
                    event_index: self.next_event_index,
                    ts: if rec_timestamp.is_empty() {
                        None
                    } else {
                        Some(rec_timestamp.to_string())
                    },
                    status,
                    event_source: ToolResultEventSource::SubagentNotification,
                    content_length: None,
                    output_bytes: None,
                    output_truncated: None,
                    content_hash: None,
                    is_error: matches!(status, ToolResultStatus::Errored).then_some(true),
                    usage: None,
                    usage_attribution: None,
                    subagent_session_id: None,
                    agent_id: None,
                    replaced_tools: None,
                    collapsed_calls: None,
                };
                self.next_event_index += 1;
                let spawned_id =
                    pick_string_field(payload, &["agent_id", "subagent_id", "session_id"]);
                if let Some(sid) = spawned_id.as_ref() {
                    ev.agent_id = Some(sid.clone());
                    ev.subagent_session_id = Some(sid.clone());
                    if let Some(open) = self.open_turn.as_mut() {
                        if let Some(spawn) = open.spawn_calls.get_mut(&call_id) {
                            if spawn.spawned_agent_id.is_none() {
                                spawn.spawned_agent_id = Some(sid.clone());
                                let info = spawn.clone();
                                maybe_emit_spawn_relationship(
                                    open,
                                    &self.session_id,
                                    &info,
                                    rec_timestamp,
                                );
                            }
                        }
                    }
                }
                if let Some(open) = self.open_turn.as_mut() {
                    open.pending_tool_result_events.push(ev);
                } else {
                    self.pending_tool_result_events.push(Pending {
                        offset: line_end_offset,
                        record: ev,
                    });
                }
            }
            _ => {}
        }
    }

    fn handle_response_item(
        &mut self,
        payload: &Value,
        rec_timestamp: &str,
        capture_content: bool,
        line_end_offset: u64,
        counter: &HeuristicCounter,
    ) {
        let item_ts = rec_timestamp;
        let pl_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match pl_type {
            "message" => {
                let role = payload
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let text = collect_message_text(payload, &role);
                if text.is_empty() {
                    return;
                }
                if role == "user" {
                    if let Some(open) = self.open_turn.as_mut() {
                        open.user_text = append_text(&open.user_text, &text);
                    } else {
                        self.pending_user_text = append_text(&self.pending_user_text, &text);
                    }
                    self.user_turn_slot
                        .blocks
                        .push(UserTurnBlock::text(&text, counter));
                    if self.user_turn_slot.ts.is_empty() && !item_ts.is_empty() {
                        self.user_turn_slot.ts = item_ts.to_string();
                    }
                    if capture_content {
                        let rec = ContentRecord {
                            v: 1,
                            source: SourceKind::Codex,
                            session_id: self.session_id.clone(),
                            message_id: self
                                .open_turn
                                .as_ref()
                                .map(|o| o.turn_id.clone())
                                .unwrap_or_default(),
                            ts: item_ts.to_string(),
                            role: ContentRole::User,
                            kind: ContentKind::Text,
                            text: Some(text.clone()),
                            tool_use: None,
                            tool_result: None,
                        };
                        push_content(&mut self.open_turn, &mut self.pending_content, rec);
                    }
                } else if role == "assistant" {
                    if let Some(open) = self.open_turn.as_mut() {
                        open.assistant_text = append_text(&open.assistant_text, &text);
                        if capture_content {
                            open.content.push(ContentRecord {
                                v: 1,
                                source: SourceKind::Codex,
                                session_id: self.session_id.clone(),
                                message_id: open.turn_id.clone(),
                                ts: item_ts.to_string(),
                                role: ContentRole::Assistant,
                                kind: ContentKind::Text,
                                text: Some(text),
                                tool_use: None,
                                tool_result: None,
                            });
                        }
                    }
                }
            }
            "reasoning" => {
                if !capture_content {
                    return;
                }
                let Some(open) = self.open_turn.as_mut() else {
                    return;
                };
                let text = collect_reasoning_text(payload);
                if !text.is_empty() {
                    open.content.push(ContentRecord {
                        v: 1,
                        source: SourceKind::Codex,
                        session_id: self.session_id.clone(),
                        message_id: open.turn_id.clone(),
                        ts: item_ts.to_string(),
                        role: ContentRole::Assistant,
                        kind: ContentKind::Thinking,
                        text: Some(text),
                        tool_use: None,
                        tool_result: None,
                    });
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                let call_id = match payload.get("call_id").and_then(|v| v.as_str()) {
                    Some(c) => c.to_string(),
                    None => return,
                };
                let output = payload.get("output").cloned().unwrap_or(Value::Null);
                self.user_turn_slot.blocks.push(UserTurnBlock::tool_result(
                    call_id.clone(),
                    &output,
                    None,
                    counter,
                ));
                if self.user_turn_slot.ts.is_empty() && !item_ts.is_empty() {
                    self.user_turn_slot.ts = item_ts.to_string();
                }
                let entry = self
                    .tool_result_counters
                    .entry(call_id.clone())
                    .or_insert(0);
                let call_index = *entry;
                *entry += 1;
                let initial_status = if self
                    .open_turn
                    .as_ref()
                    .map(|o| o.errored_call_ids.contains(&call_id))
                    .unwrap_or(false)
                {
                    ToolResultStatus::Errored
                } else {
                    ToolResultStatus::Unknown
                };
                let measured = measure_tool_output(&output);
                let mut ev = ToolResultEventRecord {
                    v: 1,
                    source: SourceKind::Codex,
                    session_id: self.session_id.clone(),
                    message_id: self.open_turn.as_ref().map(|o| o.turn_id.clone()),
                    tool_use_id: call_id.clone(),
                    call_index: Some(call_index),
                    event_index: self.next_event_index,
                    ts: if item_ts.is_empty() {
                        None
                    } else {
                        Some(item_ts.to_string())
                    },
                    status: initial_status,
                    event_source: ToolResultEventSource::FunctionCallOutput,
                    content_length: measured.length,
                    output_bytes: measured.byte_length,
                    // Codex doesn't carry an explicit truncation marker
                    // distinct from its general output; leave None until
                    // we have a concrete signal to flip on.
                    output_truncated: None,
                    content_hash: measured.hash,
                    is_error: matches!(initial_status, ToolResultStatus::Errored).then_some(true),
                    usage: None,
                    usage_attribution: None,
                    subagent_session_id: None,
                    agent_id: None,
                    replaced_tools: None,
                    collapsed_calls: None,
                };
                self.next_event_index += 1;
                if let Some(open) = self.open_turn.as_mut() {
                    if let Some(spawn) = open.spawn_calls.get_mut(&call_id) {
                        if let Some(sid) = extract_spawned_agent_id(&output) {
                            spawn.spawned_agent_id = Some(sid.clone());
                            ev.agent_id = Some(sid.clone());
                            ev.subagent_session_id = Some(sid);
                        }
                        let info = spawn.clone();
                        maybe_emit_spawn_relationship(open, &self.session_id, &info, item_ts);
                    }
                    open.pending_tool_result_events.push(ev);
                } else {
                    self.pending_tool_result_events.push(Pending {
                        offset: line_end_offset,
                        record: ev,
                    });
                }
                if capture_content {
                    let rec = ContentRecord {
                        v: 1,
                        source: SourceKind::Codex,
                        session_id: self.session_id.clone(),
                        message_id: self
                            .open_turn
                            .as_ref()
                            .map(|o| o.turn_id.clone())
                            .unwrap_or_default(),
                        ts: item_ts.to_string(),
                        role: ContentRole::ToolResult,
                        kind: ContentKind::ToolResult,
                        text: None,
                        tool_use: None,
                        tool_result: Some(ContentToolResult {
                            tool_use_id: call_id,
                            content: output,
                            is_error: None,
                        }),
                    };
                    push_content(&mut self.open_turn, &mut self.pending_content, rec);
                }
            }
            "function_call" => {
                let Some(open) = self.open_turn.as_mut() else {
                    return;
                };
                let name = match payload.get("name").and_then(|v| v.as_str()) {
                    Some(n) => n.to_string(),
                    None => return,
                };
                let call_id = match payload.get("call_id").and_then(|v| v.as_str()) {
                    Some(c) => c.to_string(),
                    None => return,
                };
                if open.seen_call_ids.contains(&call_id) {
                    return;
                }
                open.seen_call_ids.insert(call_id.clone());
                let arg_str = payload.get("arguments").and_then(|v| v.as_str());
                let parsed_args = arg_str.and_then(safe_parse_json_object);
                let hash_input = parsed_args
                    .clone()
                    .map(Value::Object)
                    .unwrap_or_else(|| Value::Object(Default::default()));
                let target = pick_function_call_target(&name, parsed_args.as_ref());
                let call = ToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    target,
                    args_hash: args_hash(&hash_input),
                    is_error: None,
                    edit_pre_hash: None,
                    edit_post_hash: None,
                    skill_name: None,
                    replaced_tools: None,
                    collapsed_calls: None,
                };
                open.tool_calls.push(call);
                if name == "spawn_agent" {
                    let mut info = SpawnCallInfo {
                        call_id: call_id.clone(),
                        ts: item_ts.to_string(),
                        subagent_type: None,
                        description: None,
                        spawned_agent_id: None,
                        emitted: false,
                    };
                    if let Some(args) = parsed_args.as_ref() {
                        let v = Value::Object(args.clone());
                        info.subagent_type =
                            pick_string_field(&v, &["subagent_type", "agent_type", "type"]);
                        info.description =
                            pick_string_field(&v, &["description", "task", "prompt"]);
                        info.spawned_agent_id =
                            pick_string_field(&v, &["agent_id", "subagent_id", "session_id"]);
                    }
                    open.spawn_calls.insert(call_id.clone(), info.clone());
                    maybe_emit_spawn_relationship(open, &self.session_id, &info, item_ts);
                }
                if capture_content {
                    let input = parsed_args
                        .map(|m| m.into_iter().collect())
                        .unwrap_or_default();
                    open.content.push(ContentRecord {
                        v: 1,
                        source: SourceKind::Codex,
                        session_id: self.session_id.clone(),
                        message_id: open.turn_id.clone(),
                        ts: item_ts.to_string(),
                        role: ContentRole::Assistant,
                        kind: ContentKind::ToolUse,
                        text: None,
                        tool_use: Some(ContentToolUse {
                            id: call_id,
                            name,
                            input,
                        }),
                        tool_result: None,
                    });
                }
            }
            "custom_tool_call" => {
                let Some(open) = self.open_turn.as_mut() else {
                    return;
                };
                let name = match payload.get("name").and_then(|v| v.as_str()) {
                    Some(n) => n.to_string(),
                    None => return,
                };
                let call_id = match payload.get("call_id").and_then(|v| v.as_str()) {
                    Some(c) => c.to_string(),
                    None => return,
                };
                if open.seen_call_ids.contains(&call_id) {
                    return;
                }
                open.seen_call_ids.insert(call_id.clone());
                let input = payload
                    .get("input")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let hash_input = serde_json::json!({ "input": input });
                let target = pick_custom_tool_target(&name, &input);
                let call = ToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    target,
                    args_hash: args_hash(&hash_input),
                    is_error: None,
                    edit_pre_hash: None,
                    edit_post_hash: None,
                    skill_name: None,
                    replaced_tools: None,
                    collapsed_calls: None,
                };
                open.tool_calls.push(call);
                if capture_content {
                    let mut input_map = BTreeMap::new();
                    input_map.insert("input".to_string(), Value::String(input));
                    open.content.push(ContentRecord {
                        v: 1,
                        source: SourceKind::Codex,
                        session_id: self.session_id.clone(),
                        message_id: open.turn_id.clone(),
                        ts: item_ts.to_string(),
                        role: ContentRole::Assistant,
                        kind: ContentKind::ToolUse,
                        text: None,
                        tool_use: Some(ContentToolUse {
                            id: call_id,
                            name,
                            input: input_map,
                        }),
                        tool_result: None,
                    });
                }
            }
            _ => {}
        }
    }
}

/// The parallel `committed_*` shadow set. Snapshots advance only at
/// `task_complete` boundaries; the final result is assembled from these
/// fields so a trailing partial line's working-state mutations are dropped.
struct CommittedSnapshot {
    end_offset: u64,
    cumulative: CumulativeUsage,
    session_id: String,
    session_cwd: Option<String>,
    turn_contexts: HashMap<String, CodexTurnContext>,
    finalized_count: usize,
    user_turns_count: usize,
    user_turn_slot: UserTurnSlot,
    root_session_emitted: bool,
    seen_session_meta_keys: BTreeSet<String>,
    next_event_index: u64,
    tool_result_counters: HashMap<String, u64>,
    last_completed_turn: Option<CodexLastCompletedTurn>,
}

impl CommittedSnapshot {
    fn initial(state: &CodexParseState, start_offset: u64) -> Self {
        Self {
            end_offset: start_offset,
            cumulative: state.cumulative.clone(),
            session_id: state.session_id.clone(),
            session_cwd: state.session_cwd.clone(),
            turn_contexts: state.turn_contexts.clone(),
            finalized_count: 0,
            user_turns_count: 0,
            user_turn_slot: state.user_turn_slot.clone(),
            root_session_emitted: state.root_session_emitted,
            seen_session_meta_keys: state.seen_session_meta_keys.clone(),
            next_event_index: state.next_event_index,
            tool_result_counters: state.tool_result_counters.clone(),
            last_completed_turn: state.last_completed_turn.clone(),
        }
    }

    /// Advance the committed snapshot to the current working state at a
    /// `task_complete` commit boundary. Mirrors the prior inline assignments
    /// verbatim.
    fn snapshot(&mut self, state: &CodexParseState, line_end_offset: u64) {
        self.end_offset = line_end_offset;
        self.cumulative = state.cumulative.clone();
        self.session_id = state.session_id.clone();
        self.session_cwd = state.session_cwd.clone();
        self.turn_contexts = state.turn_contexts.clone();
        self.finalized_count = state.finalized.len();
        self.user_turns_count = state.user_turns.len();
        self.user_turn_slot = state.user_turn_slot.clone();
        self.root_session_emitted = state.root_session_emitted;
        self.seen_session_meta_keys = state.seen_session_meta_keys.clone();
        self.next_event_index = state.next_event_index;
        self.tool_result_counters = state.tool_result_counters.clone();
        self.last_completed_turn = state.last_completed_turn.clone();
    }
}

pub(super) fn parse_codex_buffer<R: BufRead>(
    mut reader: R,
    start_offset: u64,
    options: &ParseCodexIncrementalOptions,
    project_resolver: &ProjectResolver,
) -> std::io::Result<ParseCodexIncrementalResult> {
    let capture_content = matches!(options.content_mode, Some(ContentStoreMode::Full));
    // Validated by `resolve_token_counter` at the public entry point.
    let counter = HeuristicCounter;

    let mut state = CodexParseState::new(options.resume.as_ref());
    let mut committed = CommittedSnapshot::initial(&state, start_offset);

    let mut line_buf: Vec<u8> = Vec::new();
    let mut current_offset: u64 = start_offset;
    loop {
        line_buf.clear();
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            break;
        }
        // Drop trailing partial lines — the next incremental call resumes
        // from the committed end offset, which only advances past `\n`.
        if line_buf.last() != Some(&b'\n') {
            break;
        }
        let line_end_offset = current_offset + n as u64;
        current_offset = line_end_offset;
        let text = std::str::from_utf8(&line_buf[..n - 1]).unwrap_or("").trim();
        if text.is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !parsed.is_object() {
            continue;
        }
        let rec_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let rec_timestamp = parsed
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let payload = match parsed.get("payload") {
            Some(p) if p.is_object() => p,
            _ => continue,
        };

        match rec_type {
            "session_meta" => state.handle_session_meta(payload, rec_timestamp, line_end_offset),
            "turn_context" => state.handle_turn_context(payload),
            "compacted" => state.handle_compacted(rec_timestamp, line_end_offset),
            "event_msg" => state.handle_event_msg(
                payload,
                rec_timestamp,
                capture_content,
                line_end_offset,
                &mut committed,
            ),
            "response_item" => state.handle_response_item(
                payload,
                rec_timestamp,
                capture_content,
                line_end_offset,
                &counter,
            ),
            _ => continue,
        }
    }

    // Emit only committed turns.
    let committed_turns = &state.finalized[..committed.finalized_count];
    let mut turns: Vec<TurnRecord> = Vec::with_capacity(committed_turns.len());
    let mut content_out: Vec<ContentRecord> = Vec::new();
    for (i, f) in committed_turns.iter().enumerate() {
        let mut record = TurnRecord {
            v: 1,
            source: SourceKind::Codex,
            session_id: committed.session_id.clone(),
            session_path: options.session_path.clone(),
            message_id: f.turn_id.clone(),
            turn_index: i as u64,
            ts: f.ts.clone(),
            model: f.model.clone(),
            project: None,
            project_key: None,
            usage: f.usage.clone(),
            tool_calls: f.tool_calls.clone(),
            files_touched: if f.files_touched.is_empty() {
                None
            } else {
                Some(f.files_touched.clone())
            },
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: Some(f.fidelity.clone()),
        };
        if let Some(p) = f.project.as_ref() {
            let resolved = project_resolver.resolve(p);
            record.project = Some(resolved.project);
            record.project_key = resolved.project_key;
        }
        let combined_text = join_nonempty(&[f.user_text.as_str(), f.assistant_text.as_str()], "\n");
        let has_failed_tool = f
            .tool_calls
            .iter()
            .any(|tc| f.errored_call_ids.contains(&tc.id));
        let classified = classify_activity(ClassificationInput {
            tool_calls: &f.tool_calls,
            text: &combined_text,
            has_failed_tool,
            reasoning_tokens: f.usage.reasoning,
        });
        record.activity = Some(classified.activity);
        record.retries = Some(classified.retries);
        record.has_edits = Some(classified.has_edits);
        turns.push(record);
        if capture_content {
            content_out.extend(f.content.clone());
        }
    }

    let resume = CodexResumeState {
        cumulative: committed.cumulative.clone(),
        session_id: committed.session_id.clone(),
        session_cwd: committed.session_cwd.clone(),
        turn_contexts: committed.turn_contexts.clone(),
        user_turn_slot: Some(committed.user_turn_slot.to_persisted()),
        root_session_emitted: committed.root_session_emitted,
        session_meta_relationship_keys: committed.seen_session_meta_keys.iter().cloned().collect(),
        next_event_index: committed.next_event_index,
        tool_result_counters: committed.tool_result_counters.clone(),
        last_completed_turn: committed.last_completed_turn.clone(),
    };

    let user_turns_out = state.user_turns[..committed.user_turns_count].to_vec();
    let mut events_out: Vec<CompactionEvent> = Vec::new();
    for e in state.pending_compactions {
        if e.offset <= committed.end_offset {
            events_out.push(e.record);
        }
    }
    let mut relationships_out: Vec<SessionRelationshipRecord> = Vec::new();
    for r in state.pending_relationships {
        if r.offset <= committed.end_offset {
            relationships_out.push(r.record);
        }
    }
    let mut tool_events_out: Vec<ToolResultEventRecord> = Vec::new();
    for ev in state.pending_tool_result_events {
        if ev.offset <= committed.end_offset {
            tool_events_out.push(ev.record);
        }
    }

    Ok(ParseCodexIncrementalResult {
        turns,
        content: content_out,
        events: events_out,
        user_turns: user_turns_out,
        relationships: relationships_out,
        tool_result_events: tool_events_out,
        end_offset: committed.end_offset,
        resume,
    })
}
