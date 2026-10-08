//! Map relayhistory [`SessionEvidence`] onto burn's [`SessionRecords`].
//!
//! relayhistory captures what the harness wrote; burn decides what it
//! means. Usage comes from each message's raw provider counters (see
//! [`super::usage`]), never from relayhistory's normalized view.

use std::collections::{HashMap, HashSet};

use ai_hist::{BlockKind, Message, Role, SessionEvidence, SessionRequest, Source};
use serde_json::Value;

use super::usage::{add as add_usage, usage_from_raw};
use super::SessionRecords;
use crate::reader::claude::{apply_edit_hashes, extract_files_touched, pick_target};
use crate::reader::classifier::{classify_activity, ClassificationInput};
use crate::reader::hash::args_hash;
use crate::reader::types::{
    CompactionEvent, ContentKind, ContentRecord, ContentRole, ContentToolResult, ContentToolUse,
    Coverage, Fidelity, RelationshipSourceKind, RelationshipType, SessionRelationshipRecord,
    SourceKind, StopReason, ToolCall, Usage, ToolResultEventRecord, ToolResultEventSource,
    ToolResultStatus, TurnRecord, UsageGranularity, UserTurnBlock, UserTurnBlockKind,
    UserTurnRecord,
};
use crate::reader::user_turn::bytes_to_approx_tokens;
use crate::reader::{resolve_project, RequestIdLookup, TurnKey};
use crate::util::time::format_iso_ms;

mod claude;
mod codex;
mod opencode;

pub(crate) fn source_kind(source: Source) -> Option<SourceKind> {
    match source {
        Source::Claude => Some(SourceKind::ClaudeCode),
        Source::Codex => Some(SourceKind::Codex),
        Source::OpenCode => Some(SourceKind::Opencode),
        _ => None,
    }
}

/// Burn's record set for one session. Sources burn does not account for
/// yield empty records.
pub(crate) fn records_from_evidence(ev: &SessionEvidence) -> SessionRecords {
    let Some(source) = source_kind(ev.session.source) else {
        return SessionRecords::default();
    };
    let session_id = ev.session.session_id.clone();
    let ctx = Context::new(ev, source);

    let mut records = SessionRecords {
        turns: ctx.turns(),
        ..Default::default()
    };
    records.content = ctx.content(&records.turns);
    records.tool_result_events = ctx.tool_result_events();
    records.user_turns = ctx.user_turns();
    records.compactions = ctx.compactions();
    records.relationships = vec![SessionRelationshipRecord {
        v: 1,
        source: relationship_source(source),
        session_id: session_id.clone(),
        related_session_id: None,
        relationship_type: RelationshipType::Root,
        ts: ev.session.first_activity_ms.map(format_iso_ms),
        source_session_id: None,
        source_version: ev
            .messages
            .iter()
            .find_map(|m| m.agent_version.clone())
            .or_else(|| ev.session.agent_version.clone()),
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    }];
    records.request_id_lookup = ctx.request_ids(&records.turns);
    match source {
        SourceKind::Codex => codex::refine(&ctx, &mut records),
        SourceKind::Opencode => opencode::refine(&ctx, &mut records),
        _ => claude::refine(&ctx, &mut records),
    }
    records
}

fn relationship_source(source: SourceKind) -> RelationshipSourceKind {
    match source {
        SourceKind::Codex => RelationshipSourceKind::Codex,
        SourceKind::Opencode => RelationshipSourceKind::Opencode,
        _ => RelationshipSourceKind::ClaudeCode,
    }
}

/// Indexes over one session's evidence.
pub(super) struct Context<'a> {
    ev: &'a SessionEvidence,
    source: SourceKind,
    by_id: HashMap<&'a str, &'a Message>,
    /// Burn turns in order: one per request, except Codex, whose turn is
    /// the task (`turn_id`) spanning one or more requests.
    units: Vec<Unit<'a>>,
    /// relayhistory message id → burn turn message id.
    turn_id_of: HashMap<&'a str, String>,
    errored: HashSet<&'a str>,
}

impl<'a> Context<'a> {
    fn new(ev: &'a SessionEvidence, source: SourceKind) -> Self {
        let by_id: HashMap<&str, &Message> =
            ev.messages
                .iter()
                .filter_map(|m| Some((m.message_id.as_deref()?, m)))
                .collect();
        let units = units(ev, source, &by_id);
        let mut turn_id_of = HashMap::new();
        for unit in &units {
            for request in &unit.requests {
                for id in &request.message_ids {
                    turn_id_of.insert(id.as_str(), unit.id.clone());
                }
            }
        }
        let errored = ev
            .tool_results
            .iter()
            .filter(|r| r.result_status.as_deref() == Some("errored"))
            .filter_map(|r| r.tool_use_id.as_deref())
            .collect();
        Self { ev, source, by_id, units, turn_id_of, errored }
    }

    fn session_id(&self) -> &str {
        &self.ev.session.session_id
    }

    fn turns(&self) -> Vec<TurnRecord> {
        let mut turns = Vec::new();
        for (index, unit) in self.units.iter().enumerate() {
            let message_ids: Vec<String> = unit
                .requests
                .iter()
                .flat_map(|r| r.message_ids.iter().cloned())
                .collect();
            let messages: Vec<&Message> = message_ids
                .iter()
                .filter_map(|id| self.by_id.get(id.as_str()).copied())
                .collect();
            let Some(first) = messages.first() else { continue };
            let mut usage = Usage::default();
            let mut coverage = Coverage::default();
            for request in &unit.requests {
                let raw = request
                    .message_ids
                    .iter()
                    .rev()
                    .filter_map(|id| self.by_id.get(id.as_str()))
                    .find_map(|m| m.raw_usage());
                let (u, c) = usage_from_raw(self.source, raw);
                usage = add_usage(&usage, &u);
                coverage = merge_coverage(&coverage, &c);
            }
            let tool_calls = self.tool_calls(&message_ids);
            let files_touched = extract_files_touched(&tool_calls);
            let mut turn = TurnRecord {
                v: 1,
                source: self.source,
                session_id: self.session_id().to_string(),
                session_path: self
                    .ev
                    .session
                    .raw_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                message_id: unit.id.clone(),
                turn_index: index as u64,
                ts: format_iso_ms(first.ts_ms),
                model: unit
                    .requests
                    .iter()
                    .find_map(|r| r.model.clone())
                    .unwrap_or_default(),
                project: None,
                project_key: None,
                usage,
                tool_calls,
                files_touched: (!files_touched.is_empty()).then_some(files_touched),
                subagent: None,
                stop_reason: messages
                    .iter()
                    .rev()
                    .find_map(|m| m.stop_reason.as_deref())
                    .map(|s| StopReason::from_wire(s).unwrap_or(StopReason::Silent)),
                activity: None,
                retries: None,
                has_edits: None,
                fidelity: Some(Fidelity::new(
                    UsageGranularity::PerTurn,
                    Coverage {
                        has_tool_calls: true,
                        has_tool_result_events: true,
                        has_session_relationships: true,
                        has_raw_content: true,
                        ..coverage
                    },
                )),
            };
            if let Some(cwd) = first.cwd.as_deref().or(self.ev.session.cwd.as_deref()) {
                let resolved = resolve_project(cwd);
                turn.project = Some(resolved.project);
                turn.project_key = resolved.project_key;
            }
            self.classify(&mut turn, &messages);
            turns.push(turn);
        }
        turns
    }

    fn tool_calls(&self, message_ids: &[String]) -> Vec<ToolCall> {
        let ids: HashSet<&str> = message_ids.iter().map(String::as_str).collect();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for call in &self.ev.tool_calls {
            if !call.message_id.as_deref().is_some_and(|m| ids.contains(m)) {
                continue;
            }
            if !seen.insert(call.tool_use_id.as_str()) {
                continue;
            }
            let input = call.args.clone().unwrap_or(Value::Object(Default::default()));
            let mut tool_call = ToolCall {
                id: call.tool_use_id.clone(),
                name: call.name.clone(),
                target: pick_target(&call.name, &input),
                args_hash: args_hash(&input),
                is_error: self
                    .errored
                    .contains(call.tool_use_id.as_str())
                    .then_some(true),
                edit_pre_hash: None,
                edit_post_hash: None,
                skill_name: None,
                replaced_tools: None,
                collapsed_calls: None,
            };
            apply_edit_hashes(&mut tool_call, &input);
            out.push(tool_call);
        }
        out
    }

    /// Activity, retries, and edit flag from the turn's own text plus the
    /// user prompt that rooted it.
    fn classify(&self, turn: &mut TurnRecord, messages: &[&Message]) {
        let mut parts = Vec::new();
        if let Some(prompt) = messages.first().and_then(|m| self.root_prompt_text(m)) {
            parts.push(prompt);
        }
        let assistant = text_of(messages.iter().copied(), BlockKind::Text);
        if !assistant.is_empty() {
            parts.push(assistant);
        }
        let text = parts.join("\n");
        let has_failed_tool = turn
            .tool_calls
            .iter()
            .any(|tc| self.errored.contains(tc.id.as_str()));
        let result = classify_activity(ClassificationInput {
            tool_calls: &turn.tool_calls,
            text: &text,
            has_failed_tool,
            reasoning_tokens: turn.usage.reasoning,
        });
        turn.activity = Some(result.activity);
        turn.retries = Some(result.retries);
        turn.has_edits = Some(result.has_edits);
    }

    /// Text of the nearest ancestor user message that carries prompt text.
    fn root_prompt_text(&self, start: &Message) -> Option<String> {
        let mut seen = HashSet::new();
        let mut cursor = start.parent_id.as_deref();
        while let Some(id) = cursor {
            if !seen.insert(id) {
                return None;
            }
            let message = self.by_id.get(id)?;
            if message.role == Role::User {
                let text = text_of(std::iter::once(*message), BlockKind::Text);
                if !text.is_empty() {
                    return Some(text);
                }
            }
            cursor = message.parent_id.as_deref();
        }
        None
    }

    fn content(&self, turns: &[TurnRecord]) -> Vec<ContentRecord> {
        let _ = turns;
        let mut out = Vec::new();
        // `tool_use` blocks name no call id; a message's calls are its
        // tool_use blocks in order.
        let mut calls: HashMap<&str, Vec<&ai_hist::ToolCall>> = HashMap::new();
        for call in &self.ev.tool_calls {
            if let Some(id) = call.message_id.as_deref() {
                calls.entry(id).or_default().push(call);
            }
        }
        let results: HashMap<&str, &ai_hist::ToolResult> = self
            .ev
            .tool_results
            .iter()
            .map(|r| (r.event_uid.as_str(), r))
            .collect();
        for message in &self.ev.messages {
            let message_id = self.record_message_id(message);
            let mut message_calls = message
                .message_id
                .as_deref()
                .and_then(|id| calls.get(id))
                .map(|c| c.iter())
                .into_iter()
                .flatten();
            for block in &message.blocks {
                let base = ContentRecord {
                    v: 1,
                    source: self.source,
                    session_id: self.session_id().to_string(),
                    message_id: message_id.clone(),
                    ts: format_iso_ms(block.ts_ms),
                    role: ContentRole::Assistant,
                    kind: ContentKind::Text,
                    text: None,
                    tool_use: None,
                    tool_result: None,
                };
                let record = match (message.role, block.kind) {
                    (role, BlockKind::Text) => ContentRecord {
                        role: content_role(role),
                        text: block.text.clone(),
                        ..base
                    },
                    (role, BlockKind::Thinking) => ContentRecord {
                        role: content_role(role),
                        kind: ContentKind::Thinking,
                        text: block.text.clone(),
                        ..base
                    },
                    (_, BlockKind::ToolUse) => {
                        let Some(call) = message_calls.next() else {
                            continue;
                        };
                        let input = match call.args.clone() {
                            Some(Value::Object(map)) => map.into_iter().collect(),
                            _ => Default::default(),
                        };
                        ContentRecord {
                            kind: ContentKind::ToolUse,
                            tool_use: Some(ContentToolUse {
                                id: call.tool_use_id.clone(),
                                name: call.name.clone(),
                                input,
                            }),
                            ..base
                        }
                    }
                    (_, BlockKind::ToolResult) => {
                        let result = results.get(block.event_uid.as_str());
                        ContentRecord {
                            role: ContentRole::ToolResult,
                            kind: ContentKind::ToolResult,
                            tool_result: Some(ContentToolResult {
                                tool_use_id: block.tool_use_id.clone().unwrap_or_default(),
                                content: Value::String(
                                    result.and_then(|r| r.text.clone()).unwrap_or_default(),
                                ),
                                is_error: result
                                    .filter(|r| r.result_status.as_deref() == Some("errored"))
                                    .map(|_| true),
                            }),
                            ..base
                        }
                    }
                    _ => continue,
                };
                out.push(record);
            }
        }
        out
    }

    /// The id burn keys a message's derived records by: the turn id for
    /// assistant rows, the relayhistory message id otherwise.
    fn record_message_id(&self, message: &Message) -> String {
        let id = message.message_id.as_deref().unwrap_or_default();
        self.turn_id_of
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    fn tool_result_events(&self) -> Vec<ToolResultEventRecord> {
        self.ev
            .tool_results
            .iter()
            .map(|r| ToolResultEventRecord {
                v: 1,
                source: self.source,
                session_id: self.session_id().to_string(),
                message_id: r.message_id.clone(),
                tool_use_id: r.tool_use_id.clone().unwrap_or_default(),
                call_index: r.call_index.map(|i| i as u64),
                event_index: r.event_index.unwrap_or_default() as u64,
                ts: Some(format_iso_ms(r.ts_ms)),
                status: match r.result_status.as_deref() {
                    Some("errored") => ToolResultStatus::Errored,
                    Some("completed") => ToolResultStatus::Completed,
                    Some("running") => ToolResultStatus::Running,
                    Some("cancelled") => ToolResultStatus::Cancelled,
                    _ => ToolResultStatus::Unknown,
                },
                event_source: match r.event_source.as_deref() {
                    Some("subagent_notification") => ToolResultEventSource::SubagentNotification,
                    Some("function_call_output") => ToolResultEventSource::FunctionCallOutput,
                    _ => ToolResultEventSource::ToolResult,
                },
                content_length: r.text_bytes.map(|b| b as u64),
                output_bytes: r.payload_bytes.map(|b| b as u64),
                output_truncated: r.payload_truncated,
                content_hash: r.payload_hash.clone(),
                is_error: (r.result_status.as_deref() == Some("errored")).then_some(true),
                usage: None,
                usage_attribution: None,
                subagent_session_id: r.subagent_session_id.clone(),
                agent_id: r.agent_id.clone(),
                replaced_tools: None,
                collapsed_calls: None,
            })
            .collect()
    }

    fn user_turns(&self) -> Vec<UserTurnRecord> {
        self.ev
            .user_turns
            .iter()
            .map(|t| UserTurnRecord {
                v: 1,
                source: self.source,
                session_id: self.session_id().to_string(),
                user_uuid: t.message_id.clone().unwrap_or_default(),
                ts: format_iso_ms(t.ts_ms),
                preceding_message_id: t.preceding_message_id.as_deref().map(|id| self.turn_id(id)),
                following_message_id: t.following_message_id.as_deref().map(|id| self.turn_id(id)),
                blocks: t
                    .blocks
                    .iter()
                    .map(|b| {
                        let byte_len = b.byte_len.max(0) as u64;
                        UserTurnBlock {
                            kind: if b.kind == "tool_result" {
                                UserTurnBlockKind::ToolResult
                            } else {
                                UserTurnBlockKind::Text
                            },
                            tool_use_id: b.tool_use_id.clone(),
                            byte_len,
                            approx_tokens: bytes_to_approx_tokens(byte_len),
                            is_error: b.is_error.filter(|e| *e != 0).map(|_| true),
                        }
                    })
                    .collect(),
            })
            .collect()
    }

    fn turn_id(&self, message_id: &str) -> String {
        self.turn_id_of
            .get(message_id)
            .cloned()
            .unwrap_or_else(|| message_id.to_string())
    }

    fn compactions(&self) -> Vec<CompactionEvent> {
        self.ev
            .markers
            .iter()
            .filter(|m| m.kind == "compaction_boundary")
            .map(|m| CompactionEvent {
                v: 1,
                source: self.source,
                session_id: self.session_id().to_string(),
                ts: m.ts_ms.map(format_iso_ms).unwrap_or_default(),
                preceding_message_id: m.parent_id.as_deref().map(|id| self.turn_id(id)),
                tokens_before_compact: m
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("preTokens").or_else(|| p.get("pre_tokens")))
                    .and_then(Value::as_u64),
            })
            .collect()
    }

    fn request_ids(&self, turns: &[TurnRecord]) -> RequestIdLookup {
        let mut lookup = RequestIdLookup::new();
        for turn in turns {
            let request_id = self
                .ev
                .messages
                .iter()
                .filter(|m| {
                    m.message_id.as_deref().and_then(|id| self.turn_id_of.get(id))
                        == Some(&turn.message_id)
                })
                .find_map(|m| m.request_id.clone());
            if let Some(request_id) = request_id.filter(|r| !r.is_empty()) {
                lookup.insert(TurnKey::for_turn(turn), request_id);
            }
        }
        lookup
    }
}

struct Unit<'a> {
    id: String,
    requests: Vec<&'a SessionRequest>,
}

fn units<'a>(
    ev: &'a SessionEvidence,
    source: SourceKind,
    by_id: &HashMap<&str, &'a Message>,
) -> Vec<Unit<'a>> {
    let mut out: Vec<Unit<'a>> = Vec::new();
    for request in &ev.requests {
        let codex_turn = (source == SourceKind::Codex)
            .then(|| {
                request
                    .message_ids
                    .iter()
                    .filter_map(|id| by_id.get(id.as_str()))
                    .find_map(|m| m.turn_id.clone())
            })
            .flatten();
        match (&codex_turn, out.last_mut()) {
            (Some(turn), Some(last)) if &last.id == turn => last.requests.push(request),
            _ => out.push(Unit {
                id: codex_turn.unwrap_or_else(|| turn_message_id(&request.message_ids, by_id)),
                requests: vec![request],
            }),
        }
    }
    out
}

fn merge_coverage(a: &Coverage, b: &Coverage) -> Coverage {
    Coverage {
        has_input_tokens: a.has_input_tokens || b.has_input_tokens,
        has_output_tokens: a.has_output_tokens || b.has_output_tokens,
        has_reasoning_tokens: a.has_reasoning_tokens || b.has_reasoning_tokens,
        has_cache_read_tokens: a.has_cache_read_tokens || b.has_cache_read_tokens,
        has_cache_create_tokens: a.has_cache_create_tokens || b.has_cache_create_tokens,
        ..a.clone()
    }
}

/// Burn's turn id for a request: the provider message id when the harness
/// writes one, else relayhistory's first message id.
fn turn_message_id(message_ids: &[String], by_id: &HashMap<&str, &Message>) -> String {
    let first = message_ids.first().and_then(|id| by_id.get(id.as_str()));
    first
        .and_then(|m| m.provider_message_id.clone())
        .or_else(|| first.and_then(|m| m.message_id.clone()))
        .unwrap_or_default()
}

fn content_role(role: Role) -> ContentRole {
    match role {
        Role::User => ContentRole::User,
        Role::Assistant => ContentRole::Assistant,
        _ => ContentRole::ToolResult,
    }
}

fn text_of<'m>(messages: impl Iterator<Item = &'m Message>, kind: BlockKind) -> String {
    let mut parts = Vec::new();
    for message in messages {
        for block in &message.blocks {
            if block.kind == kind {
                if let Some(text) = block.text.as_deref().filter(|t| !t.is_empty()) {
                    parts.push(text.to_string());
                }
            }
        }
    }
    parts.join("\n")
}
