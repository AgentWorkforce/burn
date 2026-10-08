//! Content, tool-result, and compaction records as burn keys them for
//! Claude.

use std::collections::{HashMap, HashSet};

use ai_hist::{Role, SessionEvidence};
use serde_json::Value;

use super::{is_system_notification, Transcript};
use crate::reader::types::{
    CompactionEvent, ContentRecord, ContentRole, SourceKind, ToolResultEventRecord,
    ToolResultEventSource, TurnRecord,
};
use crate::util::time::format_iso_ms;

/// Tool-replacement metadata a tool result carried (`_meta.replaces`,
/// `_meta.collapsedCalls`).
#[derive(Debug, Clone, Default)]
pub(super) struct Replacement {
    pub(super) replaced_tools: Option<Vec<String>>,
    pub(super) collapsed_calls: Option<u64>,
}

impl Replacement {
    fn from_payload(payload: &Value) -> Option<Self> {
        let names: Vec<String> = payload
            .get("replaces")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let collapsed = payload
            .get("collapsedCalls")
            .and_then(Value::as_f64)
            .filter(|c| c.is_finite() && *c >= 1.0)
            .map(|c| c.floor() as u64);
        let meta = Self {
            replaced_tools: (!names.is_empty()).then_some(names),
            collapsed_calls: collapsed,
        };
        (meta.replaced_tools.is_some() || meta.collapsed_calls.is_some()).then_some(meta)
    }
}

/// `tool_replacement` markers keyed by the tool-result event they annotate
/// and by the call that result answers.
pub(super) struct Replacements<'a> {
    by_event: HashMap<&'a str, Replacement>,
    by_call: HashMap<&'a str, Replacement>,
}

impl Replacements<'_> {
    pub(super) fn for_call(&self, tool_use_id: &str) -> Option<&Replacement> {
        self.by_call.get(tool_use_id)
    }
}

pub(super) fn replacements(ev: &SessionEvidence) -> Replacements<'_> {
    let call_of: HashMap<&str, &str> = ev
        .tool_results
        .iter()
        .filter_map(|r| Some((r.event_uid.as_str(), r.tool_use_id.as_deref()?)))
        .collect();
    let mut out = Replacements {
        by_event: HashMap::new(),
        by_call: HashMap::new(),
    };
    for marker in ev.markers.iter().filter(|m| m.kind == "tool_replacement") {
        let Some(event) = marker.marker_uid.strip_suffix(":replacement") else {
            continue;
        };
        let Some(meta) = marker.payload.as_ref().and_then(Replacement::from_payload) else {
            continue;
        };
        if let Some(call) = call_of.get(event) {
            out.by_call.insert(call, meta.clone());
        }
        out.by_event.insert(event, meta);
    }
    out
}

/// Burn stamps every block of an assistant message with the message's first
/// timestamp and orders a turn's blocks where its first record sits. System
/// subagent notifications are tool-result events, not content.
pub(super) fn refine_content(
    t: &Transcript<'_>,
    turns: &[TurnRecord],
    content: &mut Vec<ContentRecord>,
) {
    let notifications: HashSet<&str> =
        t.ev.messages
            .iter()
            .filter(|m| is_system_notification(m))
            .filter_map(|m| m.message_id.as_deref())
            .collect();
    content.retain(|c| !notifications.contains(c.message_id.as_str()));
    let turn_ts: HashMap<&str, &str> = turns
        .iter()
        .map(|t| (t.message_id.as_str(), t.ts.as_str()))
        .collect();
    for record in content.iter_mut() {
        if record.role == ContentRole::Assistant {
            if let Some(ts) = turn_ts.get(record.message_id.as_str()) {
                record.ts = ts.to_string();
            }
        }
    }
    content.sort_by_key(|c| t.position_of(&c.message_id));
}

/// Notifications carry no record id; results of a sidechain's spawn call
/// name the sidechain's agent; replacement metadata rides on the result
/// that reported it.
pub(super) fn refine_tool_result_events(
    ev: &SessionEvidence,
    turns: &[TurnRecord],
    events: &mut [ToolResultEventRecord],
    replacements: &Replacements<'_>,
) {
    let mut agent_by_call: HashMap<&str, &str> = HashMap::new();
    for sub in turns.iter().filter_map(|t| t.subagent.as_ref()) {
        if let (Some(call), Some(agent)) = (&sub.parent_tool_use_id, &sub.agent_id) {
            agent_by_call.entry(call.as_str()).or_insert(agent.as_str());
        }
    }
    for (event, result) in events.iter_mut().zip(&ev.tool_results) {
        if event.event_source == ToolResultEventSource::SubagentNotification {
            event.message_id = None;
        }
        if let Some(agent) = agent_by_call.get(event.tool_use_id.as_str()) {
            event.agent_id = Some(agent.to_string());
        }
        if let Some(meta) = replacements.by_event.get(result.event_uid.as_str()) {
            event.replaced_tools = meta.replaced_tools.clone();
            event.collapsed_calls = meta.collapsed_calls;
        }
    }
}

/// A compaction boundary follows the last assistant record before it; the
/// context it compacted is that turn's cache read.
pub(super) fn compactions(t: &Transcript<'_>, turns: &[TurnRecord]) -> Vec<CompactionEvent> {
    let cache_read: HashMap<&str, u64> = turns
        .iter()
        .map(|t| (t.message_id.as_str(), t.usage.cache_read))
        .collect();
    t.ev.markers
        .iter()
        .filter(|m| m.kind == "compaction_boundary")
        .map(|m| {
            let preceding = m.ts_ms.and_then(|ts| {
                t.ev.messages
                    .iter()
                    .rev()
                    .filter(|msg| msg.role == Role::Assistant && msg.ts_ms <= ts)
                    .find_map(|msg| t.turn_id(msg))
            });
            CompactionEvent {
                v: 1,
                source: SourceKind::ClaudeCode,
                session_id: t.session_id().to_string(),
                ts: m.ts_ms.map(format_iso_ms).unwrap_or_default(),
                preceding_message_id: preceding.map(str::to_string),
                tokens_before_compact: preceding.and_then(|p| cache_read.get(p).copied()),
            }
        })
        .collect()
}
