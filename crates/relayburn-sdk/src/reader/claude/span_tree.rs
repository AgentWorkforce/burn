//! Per-turn span tree builder for Claude Code transcripts.
//!
//! Pure projection over [`TurnRecord`] + paired [`ToolResultEventRecord`]
//! rows + optional [`SubagentTranscript`]s. Produces a [`TurnSpanTree`]
//! shaped per the schema documented in
//! [`crate::analyze::span_tree`] — no DB writes, no caching, no schema
//! migration. See AgentWorkforce/burn#430.
//!
//! # Hierarchy (Claude Code)
//!
//! ```text
//! Turn (root)
//! ├── UserPrompt
//! ├── Inference                    <- per #434 requestId
//! │   ├── ToolUse (name=Bash)
//! │   │   └── ToolResult           <- paired by tool_use_id
//! │   └── ToolUse (name=Task)      <- subagent dispatch
//! │       ├── ToolResult
//! │       └── Subagent             <- nested span tree from agent-<id>.jsonl
//! │           └── ...
//! └── ...
//! ```
//!
//! # Inference grouping
//!
//! When the caller hands us pre-built [`Inference`] aggregates (the
//! #434 path), each one becomes an `Inference` child of the root. Empty
//! input falls back to grouping the turn's own [`ToolCall`]s under a
//! synthetic single inference keyed by `message_id` so the tree shape
//! is uniform regardless of whether the caller wired the lookup.
//!
//! # Subagent stitching
//!
//! [`SubagentTranscript::paired_tool_use_id`] (filled by
//! [`crate::reader::pair_to_main`]) tells us which `ToolUse` a sidecar
//! belongs under. Unpaired transcripts are surfaced as **sibling**
//! `Subagent` nodes under the [`SpanKind::Turn`] root, carrying
//! `attributes["unattached"] = true`. The alternative — a separate
//! top-level `UnattachedGroup` — would force every consumer to special-
//! case "is this thing a turn or a bag of orphans?" at every entry
//! point. Keeping orphans as flagged children of the turn keeps the
//! traversal contract uniform: a tree is always one root, and the
//! `unattached` attribute is the discriminator. The agent-profiler
//! `UnattachedGroup` shape is recoverable from the same data: filter
//! the root's children to those with `kind == Subagent && unattached`.

use std::collections::HashMap;

use crate::analyze::span_tree::{AttrValue, SpanKind, SpanNode, SpanStatus, TurnSpanTree};
use crate::reader::claude::subagents::SubagentTranscript;
use crate::reader::inference::Inference;
use crate::reader::types::{
    StopReason, ToolCall, ToolResultEventRecord, ToolResultStatus, TurnRecord,
};
use crate::util::time::parse_iso_ms;

/// Inputs to the Claude span-tree builder. Grouped here (instead of as
/// a long positional argument list) so future inputs — content sidecar
/// reads, user-prompt blocks, compaction events — can be added without
/// touching every call site.
///
/// Lifetimes are tied to the caller's slice ownership; the builder
/// reads but does not retain references past `build`.
#[derive(Debug)]
pub struct ClaudeSpanTreeInputs<'a> {
    /// The turn this tree describes. Required.
    pub turn: &'a TurnRecord,
    /// Tool-result events for the same `(session_id, message_id)` as
    /// the turn. The builder pairs them to `tool_use_id`s via
    /// [`ToolResultEventRecord::tool_use_id`]. Empty slice is fine —
    /// turns without `tool_use` blocks won't have any events to pair.
    pub tool_result_events: &'a [ToolResultEventRecord],
    /// Inference aggregates the parser already collapsed by `requestId`
    /// (per #434). One [`Inference`] becomes one `Inference` span. When
    /// empty the builder falls back to a single synthetic inference
    /// keyed by the turn's `message_id`.
    pub inferences: &'a [Inference],
    /// Subagent transcripts discovered + paired via [`pair_to_main`].
    /// `paired_tool_use_id == Some(...)` nests the transcript under
    /// the matching `ToolUse`; `None` surfaces it as an `unattached`
    /// sibling under the `Turn` root.
    ///
    /// [`pair_to_main`]: crate::reader::pair_to_main
    pub subagents: &'a [SubagentTranscript],
}

/// Project a [`TurnSpanTree`] for the given Claude turn.
///
/// The returned tree is deterministic for a fixed input — children are
/// emitted in causal (parent → child) order matching the source row
/// order. Attribute maps are sorted alphabetically via [`BTreeMap`] so
/// serialized output is byte-stable.
///
/// See [`ClaudeSpanTreeInputs`] for the input contract and the module
/// doc for the hierarchy / status / attribute schema.
pub fn build_claude_span_tree(inputs: ClaudeSpanTreeInputs<'_>) -> TurnSpanTree {
    let turn = inputs.turn;

    // Root: one `Turn` span carrying the turn-level scalars + outcome.
    let mut root = SpanNode::new(SpanKind::Turn, "turn");
    let start_ms = parse_iso_ms(&turn.ts).unwrap_or(0);
    root.start_ms = start_ms;
    // End_ms gets overwritten below by the max end_ms of any child once
    // inferences materialize. Until then mirror start_ms — instant-like.
    root.end_ms = start_ms;
    root.set_attr("model", AttrValue::str(turn.model.clone()));
    if let Some(reason) = turn.stop_reason {
        root.set_attr("stop_reason", AttrValue::str(reason.wire_str()));
    }
    // Token attributes live on `Inference` spans only — never on the
    // root or on `ToolUse` / `ToolResult` children. This is the
    // "scalars are a projection of the tree" contract: a depth-first
    // sum of `tokens.*` across the tree equals the underlying
    // `TurnRecord::usage` value (modulo the multi-row request merge
    // already done by `build_inferences`). Putting tokens on more than
    // one level would double-count under `sum_attr_int`.
    //
    // Consumers that want the root-level total in one read should call
    // `TurnSpanTree::sum_attr_int("tokens.input")` etc. — same DFS the
    // tests assert on.

    // UserPrompt placeholder (we don't yet plumb the user text body).
    // Keeping the node in the shape lets downstream consumers
    // (context-delta #432) traverse `Turn -> UserPrompt -> ...`
    // uniformly even when the prompt text isn't materialized.
    let mut user_prompt = SpanNode::new(SpanKind::UserPrompt, "user-prompt");
    user_prompt.start_ms = start_ms;
    user_prompt.end_ms = start_ms;
    root.children.push(user_prompt);

    // Pair tool_result_events by tool_use_id for fast lookup. First-write
    // wins — when multiple events share the same id (progress + final
    // status), the earliest event_index records the result; the final
    // status from later events is reconciled into a single child.
    let tr_by_id = index_tool_results(inputs.tool_result_events);

    // Pair subagents by tool_use_id similarly. Pre-grouped so we can
    // mutate the unpaired list as we consume them.
    let mut paired_subagents: HashMap<String, Vec<&SubagentTranscript>> = HashMap::new();
    let mut unpaired_subagents: Vec<&SubagentTranscript> = Vec::new();
    for sa in inputs.subagents {
        match sa.paired_tool_use_id.as_deref() {
            Some(id) if !id.is_empty() => {
                paired_subagents.entry(id.to_string()).or_default().push(sa)
            }
            _ => unpaired_subagents.push(sa),
        }
    }

    // Build Inference children. Each `Inference` carries its own
    // `tool_uses` list, but the tool_use block metadata (target file,
    // is_error flag, etc.) lives on the turn's `tool_calls`. Index the
    // turn's calls by id so we can hydrate the `ToolUse` span with the
    // richer per-call attributes without re-parsing.
    let toolcall_by_id: HashMap<&str, &ToolCall> =
        turn.tool_calls.iter().map(|c| (c.id.as_str(), c)).collect();

    let inference_pairs = effective_inferences(turn, inputs.inferences);

    let mut last_inference_end: i64 = root.end_ms;
    for inf in inference_pairs.iter() {
        let inference_node =
            build_inference_node(inf, turn, &toolcall_by_id, &tr_by_id, &mut paired_subagents);
        last_inference_end = last_inference_end.max(inference_node.end_ms);
        root.children.push(inference_node);
    }

    // Unpaired subagents — sibling nodes under the root with the
    // `unattached` flag. See module doc for the orphan-semantics choice.
    //
    // Anything still in `paired_subagents` after the inference walk
    // had a `paired_tool_use_id` that didn't match any `ToolUse` we
    // saw on this turn (out-of-sync inference view, ingest race, etc).
    // Surface those as `unattached` siblings too rather than dropping
    // them — losing a subagent transcript silently is the worst
    // failure mode here.
    let mut dangling_paired: Vec<&SubagentTranscript> =
        paired_subagents.into_values().flatten().collect();
    dangling_paired.sort_by(|a, b| a.source_path.cmp(&b.source_path));

    for sa in unpaired_subagents.into_iter().chain(dangling_paired) {
        root.children.push(build_subagent_node(sa, true));
    }

    if last_inference_end > root.end_ms {
        root.end_ms = last_inference_end;
    }

    // Propagate error status: if any child is errored, mark the root.
    if root.status == SpanStatus::Ok {
        let has_child_error = root.children.iter().any(|c| c.status.is_error());
        if has_child_error {
            root.set_error("child_error");
        }
    }
    // Stop reason overrides child-error propagation — the root's outcome
    // is the most specific signal we have for the turn as a whole.
    apply_stop_reason_status(&mut root, turn.stop_reason);

    TurnSpanTree {
        session_id: turn.session_id.clone(),
        turn_id: turn.message_id.clone(),
        // `turn_index` is u64 on disk to accommodate replay corpora; OTel /
        // agent-profiler wire format uses u32. Lossy cast is intentional
        // and saturating — a session with > u32::MAX turns isn't a real
        // shape we need to support, and saturation keeps the wire form
        // well-defined.
        turn_number: u32::try_from(turn.turn_index).unwrap_or(u32::MAX),
        root,
    }
}

/// Read-side projection of the turn's `Usage`, normalized for span
/// attributes. The `Usage` struct splits cache writes by TTL window
/// (5m / 1h) but downstream consumers want a single `tokens.cache_write`
/// counter — see the issue's locked attribute schema in `analyze/span_tree.rs`.
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

/// Group `ToolResultEventRecord`s by `tool_use_id`. Each id may have
/// multiple events (progress + final status); the slice is preserved in
/// insertion order so a presenter can render the timeline.
fn index_tool_results(
    events: &[ToolResultEventRecord],
) -> HashMap<String, Vec<&ToolResultEventRecord>> {
    let mut out: HashMap<String, Vec<&ToolResultEventRecord>> = HashMap::new();
    for ev in events {
        out.entry(ev.tool_use_id.clone()).or_default().push(ev);
    }
    out
}

/// Decide which inference list drives the tree.
///
/// When the caller passes pre-built [`Inference`] aggregates we use
/// them verbatim — that's the #434 path. When the slice is empty we
/// synthesize a single inference covering the whole turn so the tree
/// shape stays uniform.
///
/// The function returns owned `Inference`s when synthesis happens so
/// the caller's borrow of `turn` is the only lifetime to track.
fn effective_inferences<'a>(turn: &TurnRecord, supplied: &'a [Inference]) -> Vec<InferenceRef<'a>> {
    if !supplied.is_empty() {
        return supplied.iter().map(InferenceRef::Borrowed).collect();
    }
    vec![InferenceRef::Synthetic(Box::new(synthesize_inference(
        turn,
    )))]
}

/// Either a caller-supplied Inference (when #434 was wired) or a
/// fabricated one (when it wasn't). Both share the same shape for the
/// builder, so we paper over them via a small enum rather than
/// `Cow<Inference>` (which would require `Inference: ToOwned`).
enum InferenceRef<'a> {
    Borrowed(&'a Inference),
    Synthetic(Box<Inference>),
}

impl<'a> InferenceRef<'a> {
    fn as_ref(&self) -> &Inference {
        match self {
            InferenceRef::Borrowed(b) => b,
            InferenceRef::Synthetic(s) => s.as_ref(),
        }
    }
}

impl<'a> AsRef<Inference> for InferenceRef<'a> {
    fn as_ref(&self) -> &Inference {
        InferenceRef::as_ref(self)
    }
}

fn synthesize_inference(turn: &TurnRecord) -> Inference {
    // Synthetic inference for the empty-supplied case. We don't have a
    // real requestId, so we mirror what `build_inferences` would emit
    // with an empty lookup: key = message_id, source = MessageId.
    let start_ms = parse_iso_ms(&turn.ts).unwrap_or(0);
    Inference {
        v: 1,
        source: turn.source,
        session_id: turn.session_id.clone(),
        request_id: turn.message_id.clone(),
        request_id_source: crate::reader::inference::InferenceKeySource::MessageId,
        turn_id: turn.message_id.clone(),
        model: turn.model.clone(),
        usage: turn.usage.clone(),
        kind: if turn.tool_calls.is_empty() {
            crate::reader::inference::InferenceKind::Message
        } else {
            crate::reader::inference::InferenceKind::ToolUse
        },
        tool_uses: turn
            .tool_calls
            .iter()
            .map(|c| crate::reader::inference::ToolUseRef {
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
    inf_ref: &InferenceRef<'_>,
    turn: &TurnRecord,
    toolcall_by_id: &HashMap<&str, &ToolCall>,
    tr_by_id: &HashMap<String, Vec<&ToolResultEventRecord>>,
    paired_subagents: &mut HashMap<String, Vec<&SubagentTranscript>>,
) -> SpanNode {
    let inf = inf_ref.as_ref();
    let name = if !inf.model.is_empty() {
        inf.model.clone()
    } else {
        turn.model.clone()
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
        let mut tool_node = build_tool_use_node(tu.id.as_str(), tu.name.as_str(), toolcall);
        // Tool_use spans inherit their inference's wall-clock range — Claude
        // doesn't record per-tool-use start/end on the assistant row.
        tool_node.start_ms = node.start_ms;
        tool_node.end_ms = node.end_ms;

        // Pair the tool_result event(s), if any.
        if let Some(events) = tr_by_id.get(tu.id.as_str()) {
            if let Some(result_node) = build_tool_result_node(events) {
                if tool_node.end_ms < result_node.end_ms {
                    tool_node.end_ms = result_node.end_ms;
                }
                // Propagate the result's error up to the tool_use parent.
                // `ToolCall::is_error` is the assistant-row hint; the
                // tool_result event carries the runtime outcome. The
                // parent inference rollup below only consults
                // `tool_node.status`, so without this an errored
                // tool_result on a tool_use whose `is_error` flag was
                // unset would silently report as success.
                if result_node.status.is_error() {
                    tool_node.set_error("tool_error");
                }
                tool_node.children.push(result_node);
            }
        }

        // Nest paired subagent transcripts under the tool_use.
        if let Some(subs) = paired_subagents.remove(tu.id.as_str()) {
            for sa in subs {
                tool_node.children.push(build_subagent_node(sa, false));
            }
        }

        // Bubble tool_error up.
        if tool_node.status.is_error() && node.status == SpanStatus::Ok {
            node.set_error("child_error");
        }

        // Propagate the widened tool_use end (from a later tool_result
        // event) back up to the inference span. Otherwise turns with a
        // tool_result timestamped after the assistant row would report
        // truncated durations once the root rolls up child end_ms.
        if node.end_ms < tool_node.end_ms {
            node.end_ms = tool_node.end_ms;
        }

        node.children.push(tool_node);
    }

    node
}

fn build_tool_use_node(tool_use_id: &str, tool_name: &str, call: Option<&ToolCall>) -> SpanNode {
    let mut node = SpanNode::new(SpanKind::ToolUse, tool_name);
    node.set_attr("tool_use_id", AttrValue::str(tool_use_id));
    if let Some(c) = call {
        if c.is_error.unwrap_or(false) {
            node.set_error("tool_error");
        }
    }
    node
}

fn build_tool_result_node(events: &[&ToolResultEventRecord]) -> Option<SpanNode> {
    if events.is_empty() {
        return None;
    }
    // Final status: prefer the latest non-Running event (Completed /
    // Errored / Cancelled). Otherwise fall back to the last seen.
    let final_event = events
        .iter()
        .rev()
        .find(|e| !matches!(e.status, ToolResultStatus::Running))
        .copied()
        .unwrap_or(*events.last().unwrap());

    let mut node = SpanNode::new(SpanKind::ToolResult, "tool-result");
    node.set_attr(
        "tool_use_id",
        AttrValue::str(final_event.tool_use_id.clone()),
    );
    if let Some(ts) = final_event.ts.as_deref() {
        let ms = parse_iso_ms(ts).unwrap_or(0);
        node.start_ms = ms;
        node.end_ms = ms;
    }
    if let Some(bytes) = final_event.output_bytes {
        node.set_attr("output_bytes", AttrValue::Int(bytes as i64));
    }
    // Propagate `output_truncated` so downstream consumers (context-delta
    // attribution, hotspots-by-bytes presenters) can flag tool outputs
    // that ingest decided to cap. Without this, large outputs appear as
    // fully representative even when only the head was retained.
    if let Some(truncated) = final_event.output_truncated {
        node.set_attr("output_truncated", AttrValue::Bool(truncated));
    }
    if final_event.is_error.unwrap_or(false) {
        node.set_error("tool_error");
    }
    // Surface status as an attribute too — useful for downstream
    // presenters that want to display "cancelled" / "errored" without
    // having to interpret `SpanStatus`.
    node.set_attr(
        "status",
        AttrValue::str(format!("{:?}", final_event.status).to_ascii_lowercase()),
    );
    Some(node)
}

fn build_subagent_node(sa: &SubagentTranscript, unattached: bool) -> SpanNode {
    let name = sa
        .agent_type
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "subagent".to_string());
    let mut node = SpanNode::new(SpanKind::Subagent, name);
    node.set_attr("agent_id", AttrValue::str(sa.agent_id.clone()));
    if let Some(at) = sa.agent_type.as_deref() {
        if !at.is_empty() {
            node.set_attr("agent_type", AttrValue::str(at));
        }
    }
    if let Some(desc) = sa.description.as_deref() {
        if !desc.is_empty() {
            node.set_attr("description", AttrValue::str(desc));
        }
    }
    if let Some(tu) = sa.paired_tool_use_id.as_deref() {
        if !tu.is_empty() {
            node.set_attr("tool_use_id", AttrValue::str(tu));
        }
    }
    if unattached {
        node.set_attr("unattached", AttrValue::Bool(true));
    }
    // We do NOT recursively build a span tree from `sa.records` here:
    // the parser hands us raw `Value` rows, and re-running the parse
    // pipeline against an in-memory sidecar is the ingest path's job,
    // not the span-tree builder's. Downstream consumers can call
    // `build_claude_span_tree` against the materialized child turn(s)
    // once they're in the ledger and stitch the subtree client-side.
    node
}

/// Apply `StopReason`-derived status to the root. Refusal / MaxTokens
/// → `Error { msg: ... }`. Other stop reasons leave the root's status
/// alone (child-error propagation may still have set it).
fn apply_stop_reason_status(root: &mut SpanNode, reason: Option<StopReason>) {
    match reason {
        Some(StopReason::Refusal) => root.set_error("refusal"),
        Some(StopReason::MaxTokens) => root.set_error("max_tokens"),
        _ => return,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::inference::{Inference, InferenceKeySource, InferenceKind, ToolUseRef};
    use crate::reader::types::{
        SourceKind, ToolCall, ToolResultEventRecord, ToolResultEventSource, ToolResultStatus, Usage,
    };

    fn make_turn(usage: Usage, calls: Vec<ToolCall>) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "sess-1".into(),
            session_path: None,
            message_id: "msg-1".into(),
            turn_index: 7,
            ts: "2026-04-20T00:00:01.000Z".into(),
            model: "claude-sonnet-4-6".into(),
            project: None,
            project_key: None,
            usage,
            tool_calls: calls,
            files_touched: None,
            subagent: None,
            stop_reason: Some(StopReason::EndTurn),
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn make_inference(req_id: &str, usage: Usage, tool_uses: Vec<ToolUseRef>) -> Inference {
        Inference {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "sess-1".into(),
            request_id: req_id.into(),
            request_id_source: InferenceKeySource::RequestId,
            turn_id: "msg-1".into(),
            model: "claude-sonnet-4-6".into(),
            usage,
            kind: InferenceKind::ToolUse,
            tool_uses,
            start_ts: "2026-04-20T00:00:01.000Z".into(),
            end_ts: "2026-04-20T00:00:02.000Z".into(),
            start_ms: 1_776_643_201_000,
            end_ms: 1_776_643_202_000,
        }
    }

    fn make_tool_call(id: &str, name: &str, is_error: Option<bool>) -> ToolCall {
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

    fn make_tr_event(
        tool_use_id: &str,
        status: ToolResultStatus,
        is_error: Option<bool>,
    ) -> ToolResultEventRecord {
        ToolResultEventRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "sess-1".into(),
            message_id: Some("msg-1".into()),
            tool_use_id: tool_use_id.into(),
            call_index: None,
            event_index: 0,
            ts: Some("2026-04-20T00:00:03.000Z".into()),
            status,
            event_source: ToolResultEventSource::ToolResult,
            content_length: Some(42),
            output_bytes: Some(42),
            output_truncated: None,
            content_hash: None,
            is_error,
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    /// Acceptance: single-inference turn projects to a well-formed
    /// tree, and the depth-first scalar sums on the root match the
    /// underlying `TurnRecord::usage` field within rounding tolerance.
    ///
    /// The root carries the turn-level scalars (we do not duplicate
    /// them onto descendants), so the per-key sum equals the source.
    #[test]
    fn single_inference_turn_projects_well_formed_tree() {
        let usage = Usage {
            input: 100,
            output: 25,
            reasoning: 5,
            cache_read: 200,
            cache_create_5m: 10,
            cache_create_1h: 2,
        };
        let calls = vec![make_tool_call("toolu_1", "Bash", None)];
        let turn = make_turn(usage.clone(), calls);
        let inf = make_inference(
            "req-1",
            usage.clone(),
            vec![ToolUseRef {
                id: "toolu_1".into(),
                name: "Bash".into(),
            }],
        );
        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[inf],
            subagents: &[],
        });

        assert_eq!(tree.session_id, "sess-1");
        assert_eq!(tree.turn_id, "msg-1");
        assert_eq!(tree.turn_number, 7);
        assert_eq!(tree.root.kind, SpanKind::Turn);

        // Root + UserPrompt + Inference -> ToolUse.
        let child_kinds: Vec<SpanKind> = tree.root.children.iter().map(|c| c.kind).collect();
        assert_eq!(child_kinds, vec![SpanKind::UserPrompt, SpanKind::Inference]);
        let inf_node = &tree.root.children[1];
        assert_eq!(inf_node.children.len(), 1);
        assert_eq!(inf_node.children[0].kind, SpanKind::ToolUse);
        assert_eq!(inf_node.children[0].name, "Bash");

        // Scalars on the root match TurnRecord::usage exactly.
        assert_eq!(tree.sum_attr_int("tokens.input"), 100);
        assert_eq!(tree.sum_attr_int("tokens.output"), 25);
        assert_eq!(tree.sum_attr_int("tokens.reasoning"), 5);
        assert_eq!(tree.sum_attr_int("tokens.cache_read"), 200);
        // cache_write = create_5m + create_1h.
        assert_eq!(tree.sum_attr_int("tokens.cache_write"), 12);

        // request_id attribute lives on the Inference span.
        match inf_node.attributes.get("request_id") {
            Some(AttrValue::String(s)) => assert_eq!(s, "req-1"),
            other => panic!("expected request_id string attribute, got {other:?}"),
        }

        // Root status is Ok for an EndTurn turn.
        assert_eq!(tree.root.status, SpanStatus::Ok);
    }

    /// Acceptance: multi-inference turn — multiple requestIds in one
    /// turn must produce multiple `Inference` children under the root.
    #[test]
    fn multi_inference_turn_emits_multiple_inference_children() {
        let usage = Usage::default();
        let turn = make_turn(
            usage.clone(),
            vec![
                make_tool_call("toolu_a", "Bash", None),
                make_tool_call("toolu_b", "Read", None),
            ],
        );
        let inf1 = make_inference(
            "req-1",
            Usage {
                input: 50,
                output: 10,
                ..Usage::default()
            },
            vec![ToolUseRef {
                id: "toolu_a".into(),
                name: "Bash".into(),
            }],
        );
        let inf2 = make_inference(
            "req-2",
            Usage {
                input: 80,
                output: 20,
                ..Usage::default()
            },
            vec![ToolUseRef {
                id: "toolu_b".into(),
                name: "Read".into(),
            }],
        );

        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[inf1, inf2],
            subagents: &[],
        });

        let inference_children: Vec<&SpanNode> = tree
            .root
            .children
            .iter()
            .filter(|c| c.kind == SpanKind::Inference)
            .collect();
        assert_eq!(inference_children.len(), 2, "two inferences expected");

        // request_id distinguishes them.
        let req_ids: Vec<&str> = inference_children
            .iter()
            .filter_map(|n| match n.attributes.get("request_id") {
                Some(AttrValue::String(s)) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(req_ids, vec!["req-1", "req-2"]);

        // Each inference has its own ToolUse child wired to the right
        // tool_use_id.
        assert_eq!(inference_children[0].children[0].name, "Bash");
        assert_eq!(inference_children[1].children[0].name, "Read");
    }

    /// Acceptance: tool_use with paired tool_result event → ToolResult
    /// nested under ToolUse, carrying the byte size and status.
    #[test]
    fn tool_use_with_paired_result_nests_tool_result() {
        let turn = make_turn(
            Usage::default(),
            vec![make_tool_call("toolu_1", "Bash", None)],
        );
        let inf = make_inference(
            "req-1",
            Usage::default(),
            vec![ToolUseRef {
                id: "toolu_1".into(),
                name: "Bash".into(),
            }],
        );
        let evt = make_tr_event("toolu_1", ToolResultStatus::Completed, Some(false));
        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[evt],
            inferences: &[inf],
            subagents: &[],
        });

        let inf_node = tree
            .root
            .children
            .iter()
            .find(|c| c.kind == SpanKind::Inference)
            .unwrap();
        let tool_use = &inf_node.children[0];
        assert_eq!(tool_use.kind, SpanKind::ToolUse);
        assert_eq!(tool_use.children.len(), 1);
        let result = &tool_use.children[0];
        assert_eq!(result.kind, SpanKind::ToolResult);
        match result.attributes.get("output_bytes") {
            Some(AttrValue::Int(n)) => assert_eq!(*n, 42),
            other => panic!("expected output_bytes int, got {other:?}"),
        }
        // tool_use_id round-trips onto the result span.
        match result.attributes.get("tool_use_id") {
            Some(AttrValue::String(s)) => assert_eq!(s, "toolu_1"),
            other => panic!("expected tool_use_id, got {other:?}"),
        }
        assert_eq!(result.status, SpanStatus::Ok);
    }

    /// Acceptance: tool_use whose `is_error == true` propagates an
    /// error status all the way to the root.
    #[test]
    fn tool_use_is_error_propagates_to_root() {
        let turn = make_turn(
            Usage::default(),
            vec![make_tool_call("toolu_1", "Bash", Some(true))],
        );
        let inf = make_inference(
            "req-1",
            Usage::default(),
            vec![ToolUseRef {
                id: "toolu_1".into(),
                name: "Bash".into(),
            }],
        );
        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[inf],
            subagents: &[],
        });

        let inf_node = &tree.root.children[1];
        assert!(inf_node.status.is_error());
        let tool_use = &inf_node.children[0];
        assert!(tool_use.status.is_error());
        // EndTurn doesn't override the child_error propagation.
        assert!(tree.root.status.is_error());
        match &tree.root.status {
            SpanStatus::Error { msg } => assert_eq!(msg, "child_error"),
            _ => unreachable!(),
        }
    }

    /// Acceptance: MaxTokens stop_reason → root status
    /// `Error { msg: "max_tokens" }`.
    #[test]
    fn max_tokens_stop_reason_marks_root_error() {
        let mut turn = make_turn(Usage::default(), vec![]);
        turn.stop_reason = Some(StopReason::MaxTokens);
        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[],
            subagents: &[],
        });
        match &tree.root.status {
            SpanStatus::Error { msg } => assert_eq!(msg, "max_tokens"),
            other => panic!("expected max_tokens error, got {other:?}"),
        }
    }

    /// Acceptance: Refusal stop_reason → root status
    /// `Error { msg: "refusal" }`.
    #[test]
    fn refusal_stop_reason_marks_root_error() {
        let mut turn = make_turn(Usage::default(), vec![]);
        turn.stop_reason = Some(StopReason::Refusal);
        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[],
            subagents: &[],
        });
        match &tree.root.status {
            SpanStatus::Error { msg } => assert_eq!(msg, "refusal"),
            other => panic!("expected refusal error, got {other:?}"),
        }
    }

    /// Acceptance: unpaired subagent → orphan handling per the chosen
    /// semantics. We surface it as a SIBLING of the inferences under
    /// the Turn root, carrying `attributes["unattached"] = true`.
    #[test]
    fn unpaired_subagent_surfaces_as_unattached_sibling() {
        let turn = make_turn(Usage::default(), vec![]);
        let orphan = SubagentTranscript {
            agent_id: "orphan-1".into(),
            agent_type: Some("slash-skill".into()),
            description: Some("ad-hoc".into()),
            meta_tool_use_id: None,
            records: vec![],
            paired_tool_use_id: None,
            source_path: std::path::PathBuf::from("/tmp/agent-orphan-1.jsonl"),
        };
        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[],
            subagents: &[orphan],
        });

        let subagent_nodes: Vec<&SpanNode> = tree
            .root
            .children
            .iter()
            .filter(|c| c.kind == SpanKind::Subagent)
            .collect();
        assert_eq!(subagent_nodes.len(), 1);
        let node = subagent_nodes[0];
        match node.attributes.get("unattached") {
            Some(AttrValue::Bool(true)) => {}
            other => panic!("expected unattached=true, got {other:?}"),
        }
        match node.attributes.get("agent_id") {
            Some(AttrValue::String(s)) => assert_eq!(s, "orphan-1"),
            _ => panic!("expected agent_id"),
        }
        // Span name uses agent_type when present.
        assert_eq!(node.name, "slash-skill");
    }

    /// Acceptance follow-on: a paired subagent nests under the
    /// matching ToolUse (NOT under the turn root). Complements the
    /// orphan case above.
    #[test]
    fn paired_subagent_nests_under_tool_use() {
        let turn = make_turn(
            Usage::default(),
            vec![make_tool_call("toolu_task", "Task", None)],
        );
        let inf = make_inference(
            "req-1",
            Usage::default(),
            vec![ToolUseRef {
                id: "toolu_task".into(),
                name: "Task".into(),
            }],
        );
        let sa = SubagentTranscript {
            agent_id: "agent-x".into(),
            agent_type: Some("general-purpose".into()),
            description: None,
            meta_tool_use_id: None,
            records: vec![],
            paired_tool_use_id: Some("toolu_task".into()),
            source_path: std::path::PathBuf::from("/tmp/agent-x.jsonl"),
        };
        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[inf],
            subagents: &[sa],
        });

        // Subagent must NOT be a top-level child of the root.
        let top_level_subs: Vec<&SpanNode> = tree
            .root
            .children
            .iter()
            .filter(|c| c.kind == SpanKind::Subagent)
            .collect();
        assert!(
            top_level_subs.is_empty(),
            "paired subagents must nest under their ToolUse, not the root"
        );

        // It IS under the Task ToolUse.
        let inf_node = tree
            .root
            .children
            .iter()
            .find(|c| c.kind == SpanKind::Inference)
            .unwrap();
        let tool_use = &inf_node.children[0];
        assert_eq!(tool_use.name, "Task");
        let nested: Vec<&SpanNode> = tool_use
            .children
            .iter()
            .filter(|c| c.kind == SpanKind::Subagent)
            .collect();
        assert_eq!(nested.len(), 1);
        // Paired subagent does NOT carry the unattached flag.
        assert!(!nested[0].attributes.contains_key("unattached"));
    }

    /// Empty-inferences input: builder synthesizes one inference from
    /// the turn itself so the tree shape stays uniform.
    #[test]
    fn empty_inferences_synthesizes_single_inference() {
        let turn = make_turn(
            Usage {
                input: 10,
                output: 5,
                ..Usage::default()
            },
            vec![make_tool_call("toolu_1", "Read", None)],
        );
        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[],
            subagents: &[],
        });
        let inf_children: Vec<&SpanNode> = tree
            .root
            .children
            .iter()
            .filter(|c| c.kind == SpanKind::Inference)
            .collect();
        assert_eq!(inf_children.len(), 1);
        // Synthetic inference carries the message_id as request_id.
        match inf_children[0].attributes.get("request_id") {
            Some(AttrValue::String(s)) => assert_eq!(s, "msg-1"),
            _ => panic!("expected request_id"),
        }
        // ToolUse child for the lone tool call.
        assert_eq!(inf_children[0].children.len(), 1);
        assert_eq!(inf_children[0].children[0].name, "Read");
    }

    /// Regression: a `ToolResult` whose timestamp lands after the
    /// assistant row must widen the parent `Inference` (and the turn
    /// root) `end_ms`. Without the propagation up the chain, the root
    /// rolls up the inference's stale end and the turn reports a
    /// truncated duration.
    #[test]
    fn tool_result_after_assistant_row_widens_inference_and_root_end_ms() {
        let turn = make_turn(
            Usage::default(),
            vec![make_tool_call("toolu_1", "Bash", None)],
        );
        // Inference ends at t=00:00:02, ToolResult is at t=00:00:03.
        let inf = make_inference(
            "req-1",
            Usage::default(),
            vec![ToolUseRef {
                id: "toolu_1".into(),
                name: "Bash".into(),
            }],
        );
        let evt = make_tr_event("toolu_1", ToolResultStatus::Completed, Some(false));

        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[evt],
            inferences: &[inf],
            subagents: &[],
        });

        let inf_node = tree
            .root
            .children
            .iter()
            .find(|c| c.kind == SpanKind::Inference)
            .expect("inference child");
        let tool_use = &inf_node.children[0];
        assert_eq!(tool_use.kind, SpanKind::ToolUse);
        let result = &tool_use.children[0];
        assert_eq!(result.kind, SpanKind::ToolResult);
        // ToolResult.end_ms is the t=00:00:03 timestamp.
        assert_eq!(result.end_ms, 1_776_643_203_000);
        // ToolUse end widens to the ToolResult end.
        assert_eq!(tool_use.end_ms, result.end_ms);
        // Inference end_ms must be propagated up — was 1_776_643_202_000
        // before, must widen to match the tool_result.
        assert_eq!(inf_node.end_ms, result.end_ms);
        // Turn root rolls up the inference end.
        assert_eq!(tree.root.end_ms, result.end_ms);
    }

    /// turn_index saturation: a future record with a > u32::MAX turn
    /// index must serialize as u32::MAX, not panic.
    #[test]
    fn turn_index_saturates_at_u32_max() {
        let mut turn = make_turn(Usage::default(), vec![]);
        turn.turn_index = (u32::MAX as u64) + 5;
        let tree = build_claude_span_tree(ClaudeSpanTreeInputs {
            turn: &turn,
            tool_result_events: &[],
            inferences: &[],
            subagents: &[],
        });
        assert_eq!(tree.turn_number, u32::MAX);
    }
}
