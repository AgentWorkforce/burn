//! Per-turn span tree builder for Codex rollouts. See
//! AgentWorkforce/burn#430.
//!
//! # Scope
//!
//! Codex (`~/.codex/sessions/<sid>/rollout-*.jsonl`) carries strictly
//! less hierarchical metadata than Claude Code:
//!
//! - **No `requestId`.** A "turn" is a single API call as far as we can
//!   tell from the rollout — `Inference` collapses to one node per
//!   `TurnRecord` keyed by `message_id`. This matches the
//!   [`crate::reader::build_inferences`] fallback path
//!   ([`InferenceKeySource::MessageId`]).
//! - **No subagent sidecars.** Codex's "subagent" notion lives in
//!   `tool_result_events` (notification rows pointing at a child
//!   session id) but there is no separate sidecar transcript file we
//!   walk. The builder accepts an empty `subagents` slice and emits
//!   no `Subagent` spans.
//! - **No `stop_reason` on the trailing assistant row.** Codex doesn't
//!   report one (the parser surfaces `stop_reason = None`), so the
//!   root status falls through to `Ok` unless a child `tool_use`
//!   reports an error.
//!
//! What we DO get: tool_use blocks (with `is_error` flags), paired
//! tool_result events (with `output_bytes` / status), and usage
//! aggregates. The tree shape is therefore identical to Claude's but
//! one level flatter — `Turn -> { UserPrompt, Inference -> ToolUse ->
//! ToolResult }` — and we do not fabricate Inference fanout the data
//! doesn't support.
//!
//! Downstream consumers that want the "Codex equivalent" of a Claude
//! span tree get an honest projection: one inference per API call,
//! with tool_use / tool_result nesting where the rollout records it.
//!
//! [`InferenceKeySource::MessageId`]: crate::reader::inference::InferenceKeySource::MessageId

use std::collections::HashMap;

use crate::analyze::span_tree::{AttrValue, SpanKind, SpanNode, SpanStatus, TurnSpanTree};
use crate::reader::inference::{Inference, InferenceKeySource, InferenceKind, ToolUseRef};
use crate::reader::types::{
    StopReason, ToolCall, ToolResultEventRecord, ToolResultStatus, TurnRecord,
};

/// Inputs to the Codex span-tree builder. Mirrors the Claude builder's
/// input struct minus the subagent transcripts.
#[derive(Debug)]
pub struct CodexSpanTreeInputs<'a> {
    /// The Codex turn this tree describes. Required.
    pub turn: &'a TurnRecord,
    /// Tool-result events for the same `(session_id, message_id)`.
    pub tool_result_events: &'a [ToolResultEventRecord],
    /// Inference aggregates the parser already built (the
    /// [`crate::reader::build_inferences`] fallback path returns one
    /// inference per turn keyed by `message_id` for Codex). Empty
    /// triggers the synthetic-inference fallback identical to the
    /// Claude builder's.
    pub inferences: &'a [Inference],
}

/// Project a [`TurnSpanTree`] for a Codex turn. See the module doc for
/// what this builder does and does not expose.
pub fn build_codex_span_tree(inputs: CodexSpanTreeInputs<'_>) -> TurnSpanTree {
    let turn = inputs.turn;

    let mut root = SpanNode::new(SpanKind::Turn, "turn");
    let start_ms = parse_iso_ms(&turn.ts).unwrap_or(0);
    root.start_ms = start_ms;
    root.end_ms = start_ms;
    root.set_attr("model", AttrValue::str(turn.model.clone()));
    if let Some(reason) = turn.stop_reason {
        // Codex normally doesn't report a stop_reason; if a future
        // rollout schema starts to, surface it on the root with the
        // same kebab-case wire string the Claude builder uses.
        root.set_attr("stop_reason", AttrValue::str(reason.wire_str()));
    }

    // UserPrompt placeholder — same contract as the Claude builder.
    let mut user_prompt = SpanNode::new(SpanKind::UserPrompt, "user-prompt");
    user_prompt.start_ms = start_ms;
    user_prompt.end_ms = start_ms;
    root.children.push(user_prompt);

    let tr_by_id = index_tool_results(inputs.tool_result_events);
    let toolcall_by_id: HashMap<&str, &ToolCall> = turn
        .tool_calls
        .iter()
        .map(|c| (c.id.as_str(), c))
        .collect();

    let inferences = effective_inferences(turn, inputs.inferences);

    let mut last_end = start_ms;
    for inf in inferences {
        let inference_node = build_inference_node(&inf, &toolcall_by_id, &tr_by_id);
        last_end = last_end.max(inference_node.end_ms);
        root.children.push(inference_node);
    }
    if last_end > root.end_ms {
        root.end_ms = last_end;
    }

    if root.status == SpanStatus::Ok {
        let has_child_error = root.children.iter().any(|c| c.status.is_error());
        if has_child_error {
            root.set_error("child_error");
        }
    }
    // MaxTokens / Refusal map the same way as in Claude. Codex won't
    // emit these today, but keeping the mapping symmetric avoids a
    // future drift if the harness starts reporting outcomes.
    apply_stop_reason_status(&mut root, turn.stop_reason);

    TurnSpanTree {
        session_id: turn.session_id.clone(),
        turn_id: turn.message_id.clone(),
        turn_number: u32::try_from(turn.turn_index).unwrap_or(u32::MAX),
        root,
    }
}

/// Same view as the Claude builder's `UsageView` — duplicated to keep
/// each builder's surface self-contained and free of cross-harness
/// imports.
struct UsageView {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
}

fn inference_usage_view(inf: &Inference) -> UsageView {
    let u = &inf.usage;
    UsageView {
        input: u.input as i64,
        output: u.output as i64,
        cache_read: u.cache_read as i64,
        cache_write: (u.cache_create_5m + u.cache_create_1h) as i64,
        reasoning: u.reasoning as i64,
    }
}

fn attach_token_attrs(node: &mut SpanNode, u: &UsageView) {
    node.set_attr("tokens.input", AttrValue::Int(u.input));
    node.set_attr("tokens.output", AttrValue::Int(u.output));
    node.set_attr("tokens.cache_read", AttrValue::Int(u.cache_read));
    node.set_attr("tokens.cache_write", AttrValue::Int(u.cache_write));
    node.set_attr("tokens.reasoning", AttrValue::Int(u.reasoning));
}

fn index_tool_results(events: &[ToolResultEventRecord]) -> HashMap<String, Vec<&ToolResultEventRecord>> {
    let mut out: HashMap<String, Vec<&ToolResultEventRecord>> = HashMap::new();
    for ev in events {
        out.entry(ev.tool_use_id.clone()).or_default().push(ev);
    }
    out
}

fn effective_inferences(turn: &TurnRecord, supplied: &[Inference]) -> Vec<Inference> {
    if !supplied.is_empty() {
        return supplied.to_vec();
    }
    vec![synthesize_inference(turn)]
}

fn synthesize_inference(turn: &TurnRecord) -> Inference {
    let start_ms = parse_iso_ms(&turn.ts).unwrap_or(0);
    Inference {
        v: 1,
        source: turn.source,
        session_id: turn.session_id.clone(),
        // Codex has no requestId — fall back to message_id, mirroring
        // what `build_inferences` does for this harness.
        request_id: turn.message_id.clone(),
        request_id_source: InferenceKeySource::MessageId,
        turn_id: turn.message_id.clone(),
        model: turn.model.clone(),
        usage: turn.usage.clone(),
        kind: if turn.tool_calls.is_empty() {
            InferenceKind::Message
        } else {
            InferenceKind::ToolUse
        },
        tool_uses: turn
            .tool_calls
            .iter()
            .map(|c| ToolUseRef {
                id: c.id.clone(),
                name: c.name.clone(),
            })
            .collect(),
        start_ts: turn.ts.clone(),
        end_ts: turn.ts.clone(),
        start_ms,
        end_ms: start_ms,
    }
}

fn build_inference_node(
    inf: &Inference,
    toolcall_by_id: &HashMap<&str, &ToolCall>,
    tr_by_id: &HashMap<String, Vec<&ToolResultEventRecord>>,
) -> SpanNode {
    let name = if !inf.model.is_empty() {
        inf.model.clone()
    } else {
        "inference".to_string()
    };
    let mut node = SpanNode::new(SpanKind::Inference, name);
    node.start_ms = inf.start_ms;
    node.end_ms = inf.end_ms;
    if !inf.model.is_empty() {
        node.set_attr("model", AttrValue::str(inf.model.clone()));
    }
    node.set_attr("request_id", AttrValue::str(inf.request_id.clone()));
    attach_token_attrs(&mut node, &inference_usage_view(inf));

    for tu in &inf.tool_uses {
        let toolcall = toolcall_by_id.get(tu.id.as_str()).copied();
        let mut tool_node = SpanNode::new(SpanKind::ToolUse, tu.name.clone());
        tool_node.set_attr("tool_use_id", AttrValue::str(tu.id.clone()));
        tool_node.start_ms = node.start_ms;
        tool_node.end_ms = node.end_ms;
        if let Some(c) = toolcall {
            if c.is_error.unwrap_or(false) {
                tool_node.set_error("tool_error");
            }
        }
        if let Some(events) = tr_by_id.get(tu.id.as_str()) {
            if let Some(result_node) = build_tool_result_node(events) {
                if tool_node.end_ms < result_node.end_ms {
                    tool_node.end_ms = result_node.end_ms;
                }
                tool_node.children.push(result_node);
            }
        }
        if tool_node.status.is_error() && node.status == SpanStatus::Ok {
            node.set_error("child_error");
        }
        node.children.push(tool_node);
    }
    node
}

fn build_tool_result_node(events: &[&ToolResultEventRecord]) -> Option<SpanNode> {
    if events.is_empty() {
        return None;
    }
    let final_event = events
        .iter()
        .rev()
        .find(|e| !matches!(e.status, ToolResultStatus::Running))
        .copied()
        .unwrap_or(*events.last().unwrap());
    let mut node = SpanNode::new(SpanKind::ToolResult, "tool-result");
    node.set_attr("tool_use_id", AttrValue::str(final_event.tool_use_id.clone()));
    if let Some(ts) = final_event.ts.as_deref() {
        let ms = parse_iso_ms(ts).unwrap_or(0);
        node.start_ms = ms;
        node.end_ms = ms;
    }
    if let Some(bytes) = final_event.output_bytes {
        node.set_attr("output_bytes", AttrValue::Int(bytes as i64));
    }
    if final_event.is_error.unwrap_or(false) {
        node.set_error("tool_error");
    }
    node.set_attr(
        "status",
        AttrValue::str(format!("{:?}", final_event.status).to_ascii_lowercase()),
    );
    Some(node)
}

fn apply_stop_reason_status(root: &mut SpanNode, reason: Option<StopReason>) {
    match reason {
        Some(StopReason::Refusal) => root.set_error("refusal"),
        Some(StopReason::MaxTokens) => root.set_error("max_tokens"),
        _ => return,
    };
}

/// Same Howard-Hinnant ISO parser the Claude builder uses. Duplicated
/// to keep each builder self-contained.
fn parse_iso_ms(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if !(bytes[4] == b'-'
        && bytes[7] == b'-'
        && (bytes[10] == b'T' || bytes[10] == b' ')
        && bytes[13] == b':'
        && bytes[16] == b':')
    {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let minute: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let second: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;
    let mut millis: i64 = 0;
    let mut idx = 19;
    if idx < bytes.len() && bytes[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        let mut frac = std::str::from_utf8(&bytes[frac_start..idx]).ok()?.to_string();
        if frac.len() > 3 {
            frac.truncate(3);
        }
        while frac.len() < 3 {
            frac.push('0');
        }
        millis = frac.parse().ok()?;
    }
    let m = month as i64;
    let d = day as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + (d as u64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_from_epoch = era * 146_097 + (doe as i64) - 719_468;
    let secs =
        days_from_epoch * 86_400 + (hour as i64) * 3_600 + (minute as i64) * 60 + (second as i64);
    Some(secs * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::types::{
        SourceKind, ToolCall, ToolResultEventRecord, ToolResultEventSource, ToolResultStatus, Usage,
    };

    fn codex_turn(usage: Usage, calls: Vec<ToolCall>) -> TurnRecord {
        TurnRecord {
            v: 1,
            // Codex's source label is what makes this an "honest"
            // Codex turn — the builder is harness-agnostic but
            // downstream consumers route on `source`.
            source: SourceKind::Codex,
            session_id: "codex-sess-1".into(),
            session_path: None,
            message_id: "msg-codex-1".into(),
            turn_index: 3,
            ts: "2026-04-20T00:00:01.000Z".into(),
            model: "gpt-5".into(),
            project: None,
            project_key: None,
            usage,
            tool_calls: calls,
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn tool_call(id: &str, name: &str, is_error: Option<bool>) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            target: None,
            args_hash: "h".into(),
            is_error,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn tr_event(id: &str, bytes: u64) -> ToolResultEventRecord {
        ToolResultEventRecord {
            v: 1,
            source: SourceKind::Codex,
            session_id: "codex-sess-1".into(),
            message_id: Some("msg-codex-1".into()),
            tool_use_id: id.into(),
            call_index: None,
            event_index: 0,
            ts: Some("2026-04-20T00:00:02.000Z".into()),
            status: ToolResultStatus::Completed,
            event_source: ToolResultEventSource::ToolResult,
            content_length: Some(bytes),
            output_bytes: Some(bytes),
            output_truncated: None,
            content_hash: None,
            is_error: None,
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    /// Empty-inferences (the default for Codex — no #434 lookup) →
    /// builder synthesizes a single Inference keyed by `message_id`,
    /// and the tree carries one ToolUse with a paired ToolResult.
    #[test]
    fn codex_flat_turn_projects_to_single_inference() {
        let turn = codex_turn(
            Usage {
                input: 60,
                output: 12,
                ..Usage::default()
            },
            vec![tool_call("call_1", "shell", None)],
        );
        let evt = tr_event("call_1", 128);
        let tree = build_codex_span_tree(CodexSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[evt],
            inferences: &[],
        });

        assert_eq!(tree.session_id, "codex-sess-1");
        assert_eq!(tree.turn_id, "msg-codex-1");
        assert_eq!(tree.turn_number, 3);

        let inf_children: Vec<&SpanNode> = tree
            .root
            .children
            .iter()
            .filter(|c| c.kind == SpanKind::Inference)
            .collect();
        assert_eq!(inf_children.len(), 1);
        // request_id falls back to message_id for Codex.
        match inf_children[0].attributes.get("request_id") {
            Some(AttrValue::String(s)) => assert_eq!(s, "msg-codex-1"),
            _ => panic!("expected request_id == message_id"),
        }
        // ToolUse -> ToolResult.
        let tool_use = &inf_children[0].children[0];
        assert_eq!(tool_use.kind, SpanKind::ToolUse);
        assert_eq!(tool_use.children.len(), 1);
        assert_eq!(tool_use.children[0].kind, SpanKind::ToolResult);
        match tool_use.children[0].attributes.get("output_bytes") {
            Some(AttrValue::Int(n)) => assert_eq!(*n, 128),
            _ => panic!("expected output_bytes"),
        }

        // Token sums roll up from the inference span.
        assert_eq!(tree.sum_attr_int("tokens.input"), 60);
        assert_eq!(tree.sum_attr_int("tokens.output"), 12);

        // No stop_reason → root status stays Ok.
        assert_eq!(tree.root.status, SpanStatus::Ok);
    }

    /// A Codex turn with no tool_calls projects to a tree with just a
    /// UserPrompt + Inference (no ToolUse children).
    #[test]
    fn codex_no_tools_emits_flat_inference() {
        let turn = codex_turn(Usage::default(), vec![]);
        let tree = build_codex_span_tree(CodexSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[],
        });
        let kinds: Vec<SpanKind> = tree.root.children.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![SpanKind::UserPrompt, SpanKind::Inference]);
        let inf = &tree.root.children[1];
        assert!(inf.children.is_empty(), "no tool_calls => no ToolUse children");
    }

    /// tool_use with `is_error == true` propagates the error to the
    /// root via the child_error path — same contract as the Claude
    /// builder.
    #[test]
    fn codex_tool_error_propagates_to_root() {
        let turn = codex_turn(Usage::default(), vec![tool_call("c1", "shell", Some(true))]);
        let tree = build_codex_span_tree(CodexSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[],
        });
        assert!(tree.root.status.is_error());
        match &tree.root.status {
            SpanStatus::Error { msg } => assert_eq!(msg, "child_error"),
            _ => unreachable!(),
        }
    }
}
