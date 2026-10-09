//! Content records: the text, thinking, tool-use, and tool-result blocks of
//! every message, keyed by the id burn derives the message's records under.

use std::collections::HashMap;

use ai_hist::{BlockKind, Message, Role};
use serde_json::Value;

use super::Context;
use crate::reader::types::{
    ContentKind, ContentRecord, ContentRole, ContentToolResult, ContentToolUse, TurnRecord,
};
use crate::util::time::format_iso_ms;

impl Context<'_> {
    pub(super) fn content(&self, turns: &[TurnRecord]) -> Vec<ContentRecord> {
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
                    // A block with no text (a signed thinking record, an
                    // OpenCode step-only envelope) carries no content.
                    (_, BlockKind::Text | BlockKind::Thinking)
                        if block.text.as_deref().is_none_or(str::is_empty) =>
                    {
                        continue
                    }
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
    pub(super) fn record_message_id(&self, message: &Message) -> String {
        let id = message.message_id.as_deref().unwrap_or_default();
        self.turn_id_of
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }
}

fn content_role(role: Role) -> ContentRole {
    match role {
        Role::User => ContentRole::User,
        Role::Assistant => ContentRole::Assistant,
        _ => ContentRole::ToolResult,
    }
}
