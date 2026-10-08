//! claude-specific refinements of the shared evidence mapping: what burn
//! reads off Claude's record graph (`parentUuid` edges, Task/Agent
//! invocations) and off Claude-only markers (slash-command triads, tool
//! replacement metadata, compaction boundaries).

use std::collections::{HashMap, HashSet};

use ai_hist::{BlockKind, ControlKind, Message, Role, SessionEvidence};

use super::Context;
use crate::reader::types::TurnRecord;
use crate::source::SessionRecords;

mod graph;
mod records;
mod relationships;
mod user_turns;

/// Derive what the shared mapping cannot for claude sessions.
pub(super) fn refine(ctx: &Context<'_>, records: &mut SessionRecords) {
    let transcript = Transcript::new(ctx);
    let replacements = records::replacements(ctx.ev);
    for turn in &mut records.turns {
        transcript.refine_turn(turn, &replacements);
    }
    records::refine_content(&transcript, &records.turns, &mut records.content);
    records::refine_tool_result_events(
        ctx.ev,
        &records.turns,
        &mut records.tool_result_events,
        &replacements,
    );
    records.compactions = records::compactions(&transcript, &records.turns);
    records.user_turns = user_turns::user_turns(&transcript);
    relationships::refine(ctx.ev, &records.turns, &mut records.relationships);
}

/// One Claude session's records in transcript order, indexed the way burn
/// walks them.
pub(super) struct Transcript<'a> {
    ev: &'a SessionEvidence,
    by_id: HashMap<&'a str, &'a Message>,
    /// Index of each message in `ev.messages`.
    position: HashMap<&'a str, usize>,
    /// relayhistory message id → burn turn id, for assistant records.
    turn_of: HashMap<&'a str, String>,
    /// Burn turn id → its assistant records in transcript order.
    turn_messages: HashMap<String, Vec<&'a Message>>,
    /// Messages the slash-command triads consist of.
    skill_messages: HashSet<&'a str>,
}

impl<'a> Transcript<'a> {
    fn new(ctx: &Context<'a>) -> Self {
        let ev = ctx.ev;
        let by_id: HashMap<&str, &Message> = ev
            .messages
            .iter()
            .filter_map(|m| Some((m.message_id.as_deref()?, m)))
            .collect();
        let position = ev
            .messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| Some((m.message_id.as_deref()?, i)))
            .collect();
        let mut turn_of = HashMap::new();
        let mut turn_messages: HashMap<String, Vec<&Message>> = HashMap::new();
        for unit in &ctx.units {
            for id in &unit.request.message_ids {
                let Some(message) = by_id.get(id.as_str()) else {
                    continue;
                };
                turn_of.insert(id.as_str(), unit.id.clone());
                turn_messages
                    .entry(unit.id.clone())
                    .or_default()
                    .push(message);
            }
        }
        let skill_messages = skill_messages(ev);
        Self {
            ev,
            by_id,
            position,
            turn_of,
            turn_messages,
            skill_messages,
        }
    }

    fn session_id(&self) -> &str {
        &self.ev.session.session_id
    }

    fn messages_of(&self, turn: &TurnRecord) -> &[&'a Message] {
        self.turn_messages
            .get(&turn.message_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn turn_id(&self, message: &Message) -> Option<&str> {
        self.turn_of
            .get(message.message_id.as_deref()?)
            .map(String::as_str)
    }

    /// Transcript position of a content record's message: a turn sits where
    /// its first record does.
    fn position_of(&self, message_id: &str) -> usize {
        let first = self
            .turn_messages
            .get(message_id)
            .and_then(|m| m.first())
            .and_then(|m| m.message_id.as_deref())
            .unwrap_or(message_id);
        self.position.get(first).copied().unwrap_or(usize::MAX)
    }
}

/// A record Claude wrote as `type: "user"` — a prompt, a control row, or a
/// tool-result envelope.
fn is_user_record(message: &Message) -> bool {
    match message.role {
        Role::User => true,
        Role::ToolResult => !is_system_notification(message),
        _ => false,
    }
}

/// A `type: "system"` subagent notification, which relayhistory stores as a
/// standalone tool-result message.
fn is_system_notification(message: &Message) -> bool {
    message
        .blocks
        .iter()
        .any(|b| b.raw_kind.as_deref() == Some("system_subagent_notification"))
}

/// Harness-injected `<task-notification>` rows share the user envelope but
/// are system events, not prompts.
fn is_task_notification(message: &Message) -> bool {
    message
        .blocks
        .iter()
        .any(|b| b.control == Some(ControlKind::TaskNotification))
}

/// The user text a prompt record carries; `None` for tool-result envelopes,
/// task notifications, and non-user records.
fn prompt_text(message: &Message) -> Option<String> {
    if !is_user_record(message) || is_task_notification(message) {
        return None;
    }
    let parts: Vec<&str> = message
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Text)
        .filter_map(|b| b.text.as_deref())
        .filter(|t| !t.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Records of every slash-command triad (caveat, invocation, stdout).
fn skill_messages(ev: &SessionEvidence) -> HashSet<&str> {
    let message_of: HashMap<&str, &str> = ev
        .messages
        .iter()
        .flat_map(|m| {
            let id = m.message_id.as_deref();
            m.blocks
                .iter()
                .filter_map(move |b| Some((b.event_uid.as_str(), id?)))
        })
        .collect();
    let mut out = HashSet::new();
    for marker in ev.markers.iter().filter(|m| m.kind == "slash_command") {
        let Some(payload) = marker.payload.as_ref() else {
            continue;
        };
        for key in [
            "caveat_event_uid",
            "invocation_event_uid",
            "output_event_uid",
        ] {
            let uid = payload.get(key).and_then(|v| v.as_str());
            if let Some(id) = uid.and_then(|u| message_of.get(u)) {
                out.insert(*id);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
