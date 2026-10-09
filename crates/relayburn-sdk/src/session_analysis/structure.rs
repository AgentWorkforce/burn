//! Cross-turn structure: cost attribution, instruction overhead, subagent
//! tree, inference flow, and context growth.

use std::collections::BTreeSet;

use super::document::{ContextReport, FlowSummary, OverheadReport, Section};
use super::Inputs;
use crate::analyze::{
    deltas_for_session, flow_graph_from_trees, ContextDeltaOpts, FlowEdgeKind, FlowNodeKind,
    FlowOpts, SubagentTreeNode, TurnSpanTree,
};
use crate::query_verbs::{
    build_session_span_trees, hotspots_attribution, load_overhead_files, overhead_report,
    overhead_trim_report, subagent_tree_for_session, HotspotSideRecords, SessionDetail, TrimShape,
};
use crate::reader::{build_inferences, TurnRecord};
use crate::source::{child_ids, subagent_transcripts};
use crate::{HotspotsAttributionResult, HotspotsGroupBy, HotspotsResult};

/// Context-growth rows kept in [`ContextReport::largest_growth`].
const CONTEXT_GROWTH_ROWS: u32 = 10;

pub(super) fn side_records<'a>(inputs: &'a Inputs<'_>) -> HotspotSideRecords<'a> {
    HotspotSideRecords {
        user_turns: &inputs.records.user_turns,
        tool_result_events: &inputs.records.tool_result_events,
        session_detail: Some(SessionDetail {
            content: &inputs.records.content,
            compactions: &inputs.records.compactions,
        }),
    }
}

pub(super) fn hotspots(inputs: &Inputs<'_>) -> Section<HotspotsAttributionResult> {
    let turns = &inputs.records.turns;
    if turns.is_empty() {
        return Section::unavailable("the session has no assistant turns");
    }
    let result = hotspots_attribution(
        turns,
        &side_records(inputs),
        inputs.pricing,
        Some(HotspotsGroupBy::Attribution),
    );
    match result {
        HotspotsResult::Attribution(result) => match result.refusal_reason.clone() {
            Some(reason) => Section::unavailable(reason),
            None => Section::available(*result),
        },
        _ => Section::unavailable("attribution produced no attribution result"),
    }
}

pub(super) fn overhead(inputs: &Inputs<'_>) -> Section<OverheadReport> {
    let dir = match inputs.project_dir() {
        Ok(dir) => dir,
        Err(reason) => return Section::unavailable(reason),
    };
    let files = match load_overhead_files(dir, None) {
        Ok(files) if files.is_empty() => {
            return Section::unavailable(format!(
                "no project instruction files (CLAUDE.md, AGENTS.md, …) under {}",
                dir.display()
            ))
        }
        Ok(files) => files,
        Err(error) => return Section::unavailable(format!("read instruction files: {error:#}")),
    };
    let turns = &inputs.records.turns;
    let trim = overhead_trim_report(
        dir,
        &files,
        turns,
        inputs.pricing,
        TrimShape {
            top: None,
            include_diff: false,
            since_label: "this session",
        },
    );
    match trim {
        Ok(trim) => Section::available(OverheadReport {
            project_dir: dir.to_string_lossy().into_owned(),
            attribution: overhead_report(dir, &files, turns, inputs.pricing),
            trim,
        }),
        Err(error) => Section::unavailable(format!("trim recommendations: {error:#}")),
    }
}

pub(super) fn subagents(inputs: &Inputs<'_>) -> Section<SubagentTreeNode> {
    let session_id = &inputs.evidence.session.session_id;
    subagent_tree_for_session(
        &inputs.records.turns,
        &inputs.records.relationships,
        inputs.pricing,
        session_id,
    )
    .map(Section::available)
    .unwrap_or_else(|| Section::unavailable("the session has no assistant turns"))
}

/// One span tree per turn of the session's own conversation, with its
/// delegated subagents paired to the tool uses that spawned them.
pub(super) fn span_trees(inputs: &Inputs<'_>) -> Vec<TurnSpanTree> {
    let delegated = child_ids(inputs.children);
    let turns: Vec<TurnRecord> = inputs
        .records
        .turns
        .iter()
        .filter(|t| {
            !t.subagent
                .as_ref()
                .and_then(|s| s.agent_id.as_deref())
                .is_some_and(|id| delegated.contains(id))
        })
        .cloned()
        .collect();
    let subagents = subagent_transcripts(&turns, inputs.children);
    build_session_span_trees(
        &turns,
        build_inferences(&turns, &inputs.records.request_id_lookup),
        inputs.records.tool_result_events.clone(),
        &subagents,
    )
}

pub(super) fn flow(inputs: &Inputs<'_>, trees: &[TurnSpanTree]) -> Section<FlowSummary> {
    if trees.is_empty() {
        return Section::unavailable("the session has no assistant turns");
    }
    let graph = flow_graph_from_trees(
        &inputs.evidence.session.session_id,
        trees,
        FlowOpts { max_turns: Some(0) },
    );
    let count = |kind: FlowNodeKind| graph.nodes.iter().filter(|n| n.kind == kind).count() as u64;
    let rails: BTreeSet<u32> = graph.nodes.iter().map(|n| n.rail).collect();
    Section::available(FlowSummary {
        turns: trees.len() as u64,
        inferences: count(FlowNodeKind::Inference),
        tool_uses: count(FlowNodeKind::ToolUse),
        subagents: count(FlowNodeKind::Subagent),
        skills: count(FlowNodeKind::Skill),
        rails: rails.len() as u64,
        edges: graph
            .edges
            .iter()
            .filter(|e| e.kind != FlowEdgeKind::Unattached)
            .count() as u64,
    })
}

pub(super) fn context(inputs: &Inputs<'_>, trees: &[TurnSpanTree]) -> Section<ContextReport> {
    let turns = &inputs.records.turns;
    let peak = turns.iter().max_by_key(|t| context_tokens(t));
    let Some(peak) = peak else {
        return Section::unavailable("the session has no assistant turns");
    };
    let largest_growth = deltas_for_session(
        trees,
        &inputs.records.compactions,
        inputs.pricing,
        &ContextDeltaOpts {
            top: Some(CONTEXT_GROWTH_ROWS),
            ..Default::default()
        },
    );
    Section::available(ContextReport {
        peak_context_tokens: context_tokens(peak),
        peak_turn_id: Some(peak.message_id.clone()),
        compactions: inputs.records.compactions.clone(),
        largest_growth,
    })
}

/// Size of the context window one turn sent.
fn context_tokens(turn: &crate::reader::TurnRecord) -> u64 {
    let u = &turn.usage;
    u.input + u.cache_read + u.cache_create_5m + u.cache_create_1h
}
