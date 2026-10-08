//! Claude user turns: every user record but a task notification, each
//! anchored to the assistant turn before it and the first new turn after.

use std::collections::{HashMap, HashSet};

use ai_hist::{BlockKind, Message, Role, ToolResult};

use super::{is_task_notification, is_user_record, Transcript};
use crate::reader::types::{SourceKind, UserTurnBlock, UserTurnBlockKind, UserTurnRecord};
use crate::reader::user_turn::bytes_to_approx_tokens;
use crate::util::time::format_iso_ms;

pub(super) fn user_turns(t: &Transcript<'_>) -> Vec<UserTurnRecord> {
    let results: HashMap<&str, &ToolResult> =
        t.ev.tool_results
            .iter()
            .map(|r| (r.event_uid.as_str(), r))
            .collect();
    let mut out: Vec<UserTurnRecord> = Vec::new();
    let mut seen_turns: HashSet<&str> = HashSet::new();
    let mut last_turn: Option<&str> = None;
    let mut awaiting: Option<usize> = None;
    for message in &t.ev.messages {
        if message.role == Role::Assistant {
            let Some(turn) = t.turn_id(message) else {
                continue;
            };
            if seen_turns.insert(turn) {
                if let Some(i) = awaiting.take() {
                    out[i].following_message_id = Some(turn.to_string());
                }
            }
            last_turn = Some(turn);
            continue;
        }
        if !is_user_record(message) || is_task_notification(message) {
            continue;
        }
        let blocks = blocks(message, &results);
        if blocks.is_empty() {
            continue;
        }
        awaiting = Some(out.len());
        out.push(UserTurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: t.session_id().to_string(),
            user_uuid: message.message_id.clone().unwrap_or_default(),
            ts: format_iso_ms(message.ts_ms),
            preceding_message_id: last_turn.map(str::to_string),
            following_message_id: None,
            blocks,
        });
    }
    out
}

/// Text blocks measure their UTF-8 text; tool results the payload bytes
/// relayhistory measured.
fn blocks(message: &Message, results: &HashMap<&str, &ToolResult>) -> Vec<UserTurnBlock> {
    let mut out = Vec::new();
    for block in &message.blocks {
        let (kind, byte_len, is_error) = match block.kind {
            BlockKind::Text => (UserTurnBlockKind::Text, block.text_bytes, None),
            BlockKind::ToolResult => {
                let result = results.get(block.event_uid.as_str());
                let errored = result.is_some_and(|r| r.result_status.as_deref() == Some("errored"));
                (
                    UserTurnBlockKind::ToolResult,
                    result.and_then(|r| r.payload_bytes).or(block.text_bytes),
                    errored.then_some(true),
                )
            }
            _ => continue,
        };
        let byte_len = byte_len.unwrap_or_default().max(0) as u64;
        if kind == UserTurnBlockKind::Text && byte_len == 0 {
            continue;
        }
        out.push(UserTurnBlock {
            kind,
            tool_use_id: block
                .tool_use_id
                .clone()
                .filter(|_| kind == UserTurnBlockKind::ToolResult),
            byte_len,
            approx_tokens: bytes_to_approx_tokens(byte_len),
            is_error,
        });
    }
    out
}
