//! Per-turn rollups: identity, fidelity, activity, stop reasons, quality.

use std::collections::HashMap;

use indexmap::IndexMap;

use super::document::{
    ActivityBreakdown, ActivityRow, FidelityReport, QualityReport, Section, SessionIdentity,
    ToolRow,
};
use super::Inputs;
use crate::analyze::{
    compute_quality, cost_for_turn, summarize_fidelity, summarize_replacement_savings,
    ComputeQualityOptions, PricingTable,
};
use crate::query_verbs::{fidelity_summary_to_value, turn_passes_hotspots_coverage};
use crate::reader::{resolve_project, ActivityCategory, TurnRecord};
use crate::util::time::format_iso_ms;
use crate::{SessionTokenMetrics, StopReasonCounts};

/// Tokens and USD of a set of turns; USD is unknown once any turn is
/// unpriced.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Spend {
    pub turns: u64,
    pub tokens: u64,
    usd: f64,
    unpriced: bool,
}

impl Spend {
    pub(super) fn add(&mut self, turn: &TurnRecord, pricing: &PricingTable) {
        self.turns += 1;
        self.tokens += turn_tokens(turn);
        match cost_for_turn(turn, pricing) {
            Some(cost) => self.usd += cost.total,
            None => self.unpriced = true,
        }
    }

    pub(super) fn cost_usd(&self) -> Option<f64> {
        (!self.unpriced).then_some(self.usd)
    }

    pub(super) fn rank(&self) -> (bool, f64, u64) {
        (self.unpriced, self.usd, self.tokens)
    }
}

/// Billable tokens of one turn, counted as the session metrics count them.
pub(super) fn turn_tokens(turn: &TurnRecord) -> u64 {
    let mut metrics = SessionTokenMetrics::default();
    metrics.add_turn(turn);
    metrics.total_tokens
}

pub(super) fn identity(inputs: &Inputs<'_>) -> SessionIdentity {
    let session = &inputs.evidence.session;
    let turns = &inputs.records.turns;
    let mut models: Vec<String> = Vec::new();
    for turn in turns {
        if !turn.model.is_empty() && !models.contains(&turn.model) {
            models.push(turn.model.clone());
        }
    }
    SessionIdentity {
        harness: inputs.harness,
        session_id: session.session_id.clone(),
        transcript_path: session
            .raw_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        cwd: session.cwd.clone(),
        project: session
            .cwd
            .as_deref()
            .map(|cwd| resolve_project(cwd).project),
        git_branch: session.git_branch.clone(),
        agent_version: session.agent_version.clone(),
        models,
        first_activity: session.first_activity_ms.map(format_iso_ms),
        last_activity: session.last_activity_ms.map(format_iso_ms),
        turn_count: turns.len() as u64,
        user_turn_count: inputs.records.user_turns.len() as u64,
        tool_call_count: turns.iter().map(|t| t.tool_calls.len() as u64).sum(),
        compaction_count: inputs.records.compactions.len() as u64,
    }
}

pub(super) fn fidelity(inputs: &Inputs<'_>) -> FidelityReport {
    let turns = &inputs.records.turns;
    FidelityReport {
        summary: fidelity_summary_to_value(&summarize_fidelity(turns)),
        attributable_turns: turns
            .iter()
            .filter(|t| turn_passes_hotspots_coverage(t))
            .count() as u64,
        evidence_kinds: inputs
            .evidence
            .coverage
            .iter()
            .filter_map(|kind| {
                serde_json::to_value(kind)
                    .ok()?
                    .as_str()
                    .map(str::to_string)
            })
            .collect(),
    }
}

pub(super) fn activity(inputs: &Inputs<'_>) -> Section<ActivityBreakdown> {
    let turns = &inputs.records.turns;
    if turns.is_empty() {
        return Section::unavailable("the session has no assistant turns");
    }
    let mut categories: IndexMap<Option<ActivityCategory>, Spend> = IndexMap::new();
    let mut tools: IndexMap<String, (u64, u64, Spend)> = IndexMap::new();
    for turn in turns {
        categories
            .entry(turn.activity)
            .or_default()
            .add(turn, inputs.pricing);
        let mut seen: Vec<&str> = Vec::new();
        for call in &turn.tool_calls {
            let row = tools.entry(call.name.clone()).or_default();
            row.0 += 1;
            row.1 += u64::from(call.is_error == Some(true));
            if !seen.contains(&call.name.as_str()) {
                seen.push(&call.name);
                row.2.add(turn, inputs.pricing);
            }
        }
    }
    let mut categories: Vec<(Option<ActivityCategory>, Spend)> = categories.into_iter().collect();
    categories.sort_by(|a, b| {
        b.1.rank()
            .partial_cmp(&a.1.rank())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut tools: Vec<(String, (u64, u64, Spend))> = tools.into_iter().collect();
    tools.sort_by(|a, b| {
        (b.1)
            .2
            .rank()
            .partial_cmp(&(a.1).2.rank())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let savings = summarize_replacement_savings(turns, None);
    Section::available(ActivityBreakdown {
        categories: categories
            .into_iter()
            .map(|(category, spend)| ActivityRow {
                category,
                turns: spend.turns,
                tokens: spend.tokens,
                cost_usd: spend.cost_usd(),
            })
            .collect(),
        tools: tools
            .into_iter()
            .map(|(tool, (calls, errors, spend))| ToolRow {
                tool,
                calls,
                errors,
                turns: spend.turns,
                tokens: spend.tokens,
                cost_usd: spend.cost_usd(),
            })
            .collect(),
        replacement_savings: (savings.calls > 0).then_some(savings),
    })
}

pub(super) fn stop_reasons(inputs: &Inputs<'_>) -> Section<StopReasonCounts> {
    let turns = &inputs.records.turns;
    let counts = StopReasonCounts::from_turns(turns);
    if turns.is_empty() {
        return Section::unavailable("the session has no assistant turns");
    }
    if counts.none == turns.len() as u64 {
        return Section::unavailable(format!(
            "{} transcripts record no stop reason for these turns",
            inputs.harness
        ));
    }
    Section::available(counts)
}

pub(super) fn quality(inputs: &Inputs<'_>) -> Section<QualityReport> {
    let turns = &inputs.records.turns;
    if turns.is_empty() {
        return Section::unavailable("the session has no assistant turns");
    }
    let content_by_session: HashMap<String, Vec<crate::reader::ContentRecord>> = HashMap::from([(
        inputs.evidence.session.session_id.clone(),
        inputs.records.content.clone(),
    )]);
    let result = compute_quality(
        turns,
        &ComputeQualityOptions {
            content_by_session: Some(&content_by_session),
            now_ms: None,
        },
    );
    Section::available(QualityReport {
        outcome: result.outcomes.into_iter().next(),
        one_shot: result.one_shot.into_iter().next(),
    })
}
