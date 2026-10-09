//! A Codex session's evidence as one stream in rollout order.
//!
//! relayhistory files a rollout line under messages, markers, tool results
//! and file edits; every Codex record id is `{line_index}:{record type}`
//! (`MessageIdOrigin::Synthesized`), so the line index restores the order
//! burn's task state machine reads them in.

use std::collections::{HashMap, HashSet};

use ai_hist::{BlockKind, FileEdit, Marker, Role, SessionEvidence, ToolCall, ToolResult};
use serde_json::Value;

/// One rollout record burn derives from.
pub(super) enum Event<'a> {
    TaskStarted {
        turn_id: &'a str,
        ts_ms: i64,
    },
    TaskComplete {
        turn_id: &'a str,
    },
    /// A `compacted` record (not the `context_compacted` notice).
    Compacted {
        ts_ms: i64,
    },
    /// A `turn_context` record's payload.
    TurnContext {
        payload: &'a Value,
    },
    /// A `token_count`'s `info`, verbatim.
    UsageSnapshot {
        info: &'a Value,
    },
    /// A `subagent_*_complete`-style lifecycle notification.
    SubagentDone {
        marker: &'a Marker,
    },
    UserText {
        text: &'a str,
        ts_ms: i64,
    },
    AssistantText {
        text: &'a str,
        ts_ms: i64,
    },
    Reasoning {
        text: &'a str,
        ts_ms: i64,
    },
    ToolUse(ToolUse<'a>),
    ToolOutput {
        result: &'a ToolResult,
    },
    /// A file a successful `patch_apply_end` changed.
    FileEdit {
        path: &'a str,
    },
}

pub(super) struct ToolUse<'a> {
    pub call: &'a ToolCall,
    /// A `custom_tool_call` (freeform `input`) rather than a `function_call`.
    pub custom: bool,
    pub ts_ms: i64,
}

/// Where one event sits in the rollout.
pub(super) struct Located<'a> {
    pub line: u64,
    pub event: Event<'a>,
}

/// The rollout line a Codex record id names.
pub(super) fn line_of(uid: &str) -> u64 {
    uid.split_once(':')
        .and_then(|(line, _)| line.parse().ok())
        .unwrap_or(u64::MAX)
}

fn record_type(uid: &str) -> &str {
    uid.split_once(':').map_or("", |(_, kind)| kind)
}

/// Every event in rollout order; events of one line keep their order.
pub(super) fn stream(ev: &SessionEvidence) -> Vec<Located<'_>> {
    let mut out = Vec::new();
    out.extend(ev.markers.iter().filter_map(marker_event));
    out.extend(message_events(ev));
    out.extend(
        ev.tool_results
            .iter()
            .filter(|r| r.event_source.as_deref() == Some("function_call_output"))
            .map(|result| Located {
                line: line_of(&result.event_uid),
                event: Event::ToolOutput { result },
            }),
    );
    out.extend(file_edits(ev));
    out.sort_by_key(|located| located.line);
    out
}

fn marker_event(marker: &Marker) -> Option<Located<'_>> {
    let ts_ms = marker.ts_ms.unwrap_or_default();
    let turn_id = marker.turn_id.as_deref();
    let event = match (marker.kind.as_str(), marker.subkind.as_deref()) {
        ("task_started", _) => Event::TaskStarted {
            turn_id: turn_id?,
            ts_ms,
        },
        ("task_complete", _) => Event::TaskComplete { turn_id: turn_id? },
        ("compaction_boundary", Some("compacted")) => Event::Compacted { ts_ms },
        ("turn_context", _) => Event::TurnContext {
            payload: marker.payload.as_ref()?,
        },
        ("usage_snapshot", _) => Event::UsageSnapshot {
            info: marker.payload.as_ref()?,
        },
        ("subagent_notification", Some(kind)) if is_terminal_notification(kind) => {
            Event::SubagentDone { marker }
        }
        _ => return None,
    };
    Some(Located {
        line: line_of(&marker.marker_uid),
        event,
    })
}

fn is_terminal_notification(kind: &str) -> bool {
    kind.starts_with("subagent_")
        && ["_complete", "_done", "_finished", "_terminated"]
            .iter()
            .any(|end| kind.ends_with(end))
}

fn message_events(ev: &SessionEvidence) -> Vec<Located<'_>> {
    let calls: HashMap<&str, &ToolCall> = ev
        .tool_calls
        .iter()
        .filter_map(|c| Some((c.message_id.as_deref()?, c)))
        .collect();
    let mut out = Vec::new();
    for message in &ev.messages {
        for block in &message.blocks {
            let ts_ms = block.ts_ms;
            let text = block.text.as_deref().filter(|t| !t.is_empty());
            let event = match (message.role, block.kind) {
                (Role::User, BlockKind::Text) if block.control.is_none() => {
                    text.map(|text| Event::UserText { text, ts_ms })
                }
                (Role::Assistant, BlockKind::Text) => {
                    text.map(|text| Event::AssistantText { text, ts_ms })
                }
                (Role::Assistant, BlockKind::Thinking) => {
                    text.map(|text| Event::Reasoning { text, ts_ms })
                }
                (Role::Assistant, BlockKind::ToolUse) => tool_use(&calls, message, block),
                _ => None,
            };
            out.extend(event.map(|event| Located {
                line: line_of(&block.event_uid),
                event,
            }));
        }
    }
    out
}

fn tool_use<'a>(
    calls: &HashMap<&str, &'a ToolCall>,
    message: &ai_hist::Message,
    block: &ai_hist::Block,
) -> Option<Event<'a>> {
    let custom = match record_type(&block.event_uid) {
        "function_call" => false,
        "custom_tool_call" => true,
        _ => return None,
    };
    let call = calls.get(message.message_id.as_deref()?)?;
    Some(Event::ToolUse(ToolUse {
        call,
        custom,
        ts_ms: block.ts_ms,
    }))
}

/// Files changed by `patch_apply_end`s that did not report failure.
fn file_edits(ev: &SessionEvidence) -> Vec<Located<'_>> {
    let failed: HashSet<&str> = ev
        .tool_calls
        .iter()
        .filter(|c| c.is_error == Some(true))
        .map(|c| c.tool_use_id.as_str())
        .collect();
    ev.file_edits
        .iter()
        .filter(|edit| !failed.contains(call_of(edit)))
        .filter_map(|edit| {
            Some(Located {
                line: line_of(edit.message_id.as_deref()?),
                event: Event::FileEdit {
                    path: &edit.file_path,
                },
            })
        })
        .collect()
}

/// The call a Codex file edit came from: its id is `{call_id}#{path}`.
fn call_of(edit: &FileEdit) -> &str {
    edit.tool_use_id
        .split_once('#')
        .map_or(edit.tool_use_id.as_str(), |(call, _)| call)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_orders_record_ids() {
        assert_eq!(line_of("12:function_call"), 12);
        assert_eq!(line_of("garbage"), u64::MAX);
        assert_eq!(record_type("15:custom_tool_call"), "custom_tool_call");
        assert_eq!(record_type("garbage"), "");
    }

    #[test]
    fn terminal_notifications() {
        for kind in [
            "subagent_message_complete",
            "subagent_done",
            "subagent_x_finished",
            "subagent_terminated",
        ] {
            assert!(is_terminal_notification(kind), "{kind}");
        }
        assert!(!is_terminal_notification("subagent_started"));
        assert!(!is_terminal_notification("task_complete"));
    }
}
