//! OpenCode user turns: what reached the model between two assistant
//! turns — the previous turn's tool outputs, then any human text sent
//! since it.

use ai_hist::{BlockKind, Message, Role};

use super::Context;
use crate::reader::types::{SourceKind, UserTurnBlock, UserTurnBlockKind, UserTurnRecord};
use crate::reader::user_turn::bytes_to_approx_tokens;
use crate::util::time::format_iso_ms;

/// A user message: its id, when it was sent, and its text blocks' sizes.
struct UserMessage<'a> {
    id: &'a str,
    ts_ms: i64,
    message: Option<&'a Message>,
}

/// One user turn before each assistant turn that had input: the opening
/// prompt, or the bridge from the previous assistant turn.
pub(super) fn derive(ctx: &Context<'_>, assistants: &[&Message]) -> Vec<UserTurnRecord> {
    let users = user_messages(ctx);
    let mut out = Vec::new();
    for (i, next) in assistants.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| assistants[p]);
        let user = users
            .iter()
            .take_while(|u| u.ts_ms <= next.ts_ms)
            .last()
            .filter(|u| prev.is_none_or(|p| u.ts_ms > p.ts_ms));
        let mut blocks = prev.map(|p| tool_output_blocks(ctx, p)).unwrap_or_default();
        if let Some(message) = user.and_then(|u| u.message) {
            blocks.extend(text_blocks(message));
        }
        if blocks.is_empty() {
            continue;
        }
        let next_id = id_of(next);
        out.push(UserTurnRecord {
            v: 1,
            source: SourceKind::Opencode,
            session_id: ctx.session_id().to_string(),
            user_uuid: user.map(|u| u.id.to_string()).unwrap_or_else(|| {
                let from = prev.map(id_of).unwrap_or("start");
                format!("{}:{from}->{next_id}", ctx.session_id())
            }),
            ts: format_iso_ms(user.map_or(next.ts_ms, |u| u.ts_ms)),
            preceding_message_id: prev.map(|p| id_of(p).to_string()),
            following_message_id: Some(next_id.to_string()),
            blocks,
        });
    }
    out
}

fn id_of(message: &Message) -> &str {
    message.message_id.as_deref().unwrap_or_default()
}

/// Every user message in time order. A compaction boundary is a user
/// message too, one that carries no text.
fn user_messages<'a>(ctx: &Context<'a>) -> Vec<UserMessage<'a>> {
    let mut users: Vec<UserMessage<'a>> = ctx
        .ev
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| {
            Some(UserMessage {
                id: m.message_id.as_deref()?,
                ts_ms: m.ts_ms,
                message: Some(m),
            })
        })
        .collect();
    for marker in &ctx.ev.markers {
        let (Some(id), Some(ts_ms)) = (marker.message_id.as_deref(), marker.ts_ms) else {
            continue;
        };
        if marker.kind == "compaction_boundary" && !users.iter().any(|u| u.id == id) {
            users.push(UserMessage { id, ts_ms, message: None });
        }
    }
    users.sort_by_key(|u| u.ts_ms);
    users
}

/// The tool outputs an assistant turn produced, in call order.
fn tool_output_blocks(ctx: &Context<'_>, assistant: &Message) -> Vec<UserTurnBlock> {
    assistant
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::ToolResult)
        .map(|b| {
            let tool_use_id = b.tool_use_id.clone().unwrap_or_default();
            let failed = ctx.errored.contains(tool_use_id.as_str());
            UserTurnBlock {
                is_error: failed.then_some(true),
                ..block(UserTurnBlockKind::ToolResult, Some(tool_use_id), b.text_bytes)
            }
        })
        .collect()
}

fn text_blocks(message: &Message) -> impl Iterator<Item = UserTurnBlock> + '_ {
    message
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Text)
        .map(|b| block(UserTurnBlockKind::Text, None, b.text_bytes))
}

fn block(kind: UserTurnBlockKind, tool_use_id: Option<String>, bytes: Option<i64>) -> UserTurnBlock {
    let byte_len = bytes.unwrap_or_default().max(0) as u64;
    UserTurnBlock {
        kind,
        tool_use_id,
        byte_len,
        approx_tokens: bytes_to_approx_tokens(byte_len),
        is_error: None,
    }
}
