//! opencode-specific refinements of the shared evidence mapping.
//!
//! OpenCode stores one assistant message per model step, so a burn turn is
//! exactly one relayhistory assistant message and its id is the message id.
//! Tool outputs live on the assistant message that called the tool; the
//! user turn that follows an assistant turn carries those outputs forward.

use std::collections::{BTreeSet, HashSet};

use ai_hist::{Message, RelationshipSide};
use serde_json::Value;

use super::Context;
use crate::reader::types::{
    CompactionEvent, RelationshipSourceKind, RelationshipType, SessionRelationshipRecord,
    SourceKind, Subagent, ToolResultEventRecord, ToolResultEventSource, TurnRecord, Usage,
    UsageAttribution,
};
use crate::source::usage::usage_from_raw;
use crate::source::SessionRecords;
use crate::util::time::format_iso_ms;

mod tools;
mod user_turns;

/// Derive what the shared mapping cannot for opencode sessions.
pub(super) fn refine(ctx: &Context<'_>, records: &mut SessionRecords) {
    let parent = parent_session(ctx);
    for turn in &mut records.turns {
        refine_turn(ctx, turn, parent.is_some());
    }
    refine_relationships(ctx, records, parent);
    for event in &mut records.tool_result_events {
        // OpenCode keeps a tool's output whole on the tool part: the result
        // is the part itself, and nothing about it is truncated.
        event.event_source = ToolResultEventSource::ToolResult;
        event.output_truncated = None;
    }
    split_tool_usage(&records.turns, &mut records.tool_result_events);
    let assistants = turn_messages(ctx, &records.turns);
    records.user_turns = user_turns::derive(ctx, &assistants);
    records.compactions = compactions(ctx, &assistants);
}

/// The session that delegated this one, as OpenCode's `session.parentID`
/// names it.
fn parent_session(ctx: &Context<'_>) -> Option<String> {
    ctx.ev
        .relationships
        .iter()
        .find(|r| {
            r.side == RelationshipSide::Child
                && r.child_session_id.as_deref() == Some(ctx.session_id())
        })
        .map(|r| r.parent_session_id.clone())
}

/// Each turn's assistant message, in turn order.
fn turn_messages<'a>(ctx: &Context<'a>, turns: &[TurnRecord]) -> Vec<&'a Message> {
    turns
        .iter()
        .filter_map(|t| ctx.by_id.get(t.message_id.as_str()).copied())
        .collect()
}

/// OpenCode's tool vocabulary for targets, skills and touched files, then
/// the activity classification over the corrected calls.
fn refine_turn(ctx: &Context<'_>, turn: &mut TurnRecord, is_sidechain: bool) {
    let mut files = BTreeSet::new();
    for call in &mut turn.tool_calls {
        let input = ctx
            .ev
            .tool_calls
            .iter()
            .find(|c| c.tool_use_id == call.id)
            .and_then(|c| c.args.clone())
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Default::default()));
        call.target = tools::pick_target(&call.name, &input);
        call.skill_name = tools::skill_name(&call.name, &input);
        if let Some(target) = call
            .target
            .as_ref()
            .filter(|_| tools::is_file_tool(&call.name))
        {
            files.insert(target.clone());
        }
    }
    turn.files_touched = (!files.is_empty()).then(|| files.into_iter().collect());
    if is_sidechain {
        turn.subagent = Some(Subagent {
            is_sidechain: true,
            parent_tool_use_id: None,
            agent_id: None,
            parent_agent_id: None,
            subagent_type: None,
            description: None,
        });
    }
    let messages: Vec<&Message> = ctx
        .by_id
        .get(turn.message_id.as_str())
        .copied()
        .into_iter()
        .collect();
    ctx.classify(turn, &messages);
}

/// The root record dates from the first assistant turn; a delegated
/// session also records the subagent edge to its parent.
fn refine_relationships(ctx: &Context<'_>, records: &mut SessionRecords, parent: Option<String>) {
    let first_ts = records.turns.first().map(|t| t.ts.clone());
    for relationship in &mut records.relationships {
        relationship.ts = first_ts.clone();
    }
    if let Some(parent) = parent {
        records.relationships.push(SessionRelationshipRecord {
            v: 1,
            source: RelationshipSourceKind::NativeOpencode,
            session_id: ctx.session_id().to_string(),
            related_session_id: Some(parent),
            relationship_type: RelationshipType::Subagent,
            ts: first_ts,
            source_session_id: None,
            source_version: None,
            parent_tool_use_id: None,
            agent_id: None,
            subagent_type: None,
            description: None,
        });
    }
}

/// A turn's usage shared across the tool results it produced: whole to a
/// single result, evenly split across several.
fn split_tool_usage(turns: &[TurnRecord], events: &mut [ToolResultEventRecord]) {
    for turn in turns {
        let owned: Vec<&mut ToolResultEventRecord> = events
            .iter_mut()
            .filter(|e| e.message_id.as_deref() == Some(turn.message_id.as_str()))
            .collect();
        let n = owned.len() as u64;
        let attribution = match n {
            0 => continue,
            1 => UsageAttribution::SingleToolTurn,
            _ => UsageAttribution::EvenSplitTurn,
        };
        for (idx, event) in owned.into_iter().enumerate() {
            event.usage = Some(usage_share(&turn.usage, n, idx as u64));
            event.usage_attribution = Some(attribution);
        }
    }
}

/// The `idx`th of `n` integer shares of `total`; the remainder goes one
/// unit at a time to the first shares, so the shares sum to the total.
fn usage_share(total: &Usage, n: u64, idx: u64) -> Usage {
    let split = |value: u64| value / n + u64::from(idx < value % n);
    Usage {
        input: split(total.input),
        output: split(total.output),
        reasoning: split(total.reasoning),
        cache_read: split(total.cache_read),
        cache_create_5m: split(total.cache_create_5m),
        cache_create_1h: split(total.cache_create_1h),
    }
}

/// One event per user message carrying a `compaction` part. What the
/// context held before it is the cache the preceding assistant turn read.
fn compactions(ctx: &Context<'_>, assistants: &[&Message]) -> Vec<CompactionEvent> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for marker in &ctx.ev.markers {
        if marker.kind != "compaction_boundary" {
            continue;
        }
        if marker
            .message_id
            .as_ref()
            .is_some_and(|id| !seen.insert(id.as_str()))
        {
            continue;
        }
        let ts_ms = marker.ts_ms.unwrap_or_default();
        let preceding = assistants.iter().take_while(|a| a.ts_ms < ts_ms).last();
        out.push(CompactionEvent {
            v: 1,
            source: SourceKind::Opencode,
            session_id: ctx.session_id().to_string(),
            ts: format_iso_ms(ts_ms),
            preceding_message_id: preceding.and_then(|a| a.message_id.clone()),
            tokens_before_compact: preceding.map(|a| {
                usage_from_raw(SourceKind::Opencode, a.raw_usage())
                    .0
                    .cache_read
            }),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_sum_to_the_total_with_the_remainder_up_front() {
        let total = Usage {
            input: 7,
            output: 100,
            cache_create_5m: 20000,
            ..Usage::default()
        };
        let shares: Vec<Usage> = (0..3).map(|i| usage_share(&total, 3, i)).collect();
        assert_eq!(
            shares.iter().map(|u| u.input).collect::<Vec<_>>(),
            [3, 2, 2]
        );
        assert_eq!(
            shares.iter().map(|u| u.output).collect::<Vec<_>>(),
            [34, 33, 33]
        );
        assert_eq!(
            shares.iter().map(|u| u.cache_create_5m).collect::<Vec<_>>(),
            [6667, 6667, 6666]
        );
        assert_eq!(usage_share(&total, 1, 0), total);
    }
}
