//! The analysis `findings` list: every detector's output with its evidence,
//! explanation, impact and suggested change.

use std::collections::HashMap;

use super::document::{
    ContextReport, Finding, FindingEvidence, FindingImpact, OverheadReport, Section,
};
use super::explain::guidance;
use super::sections::{turn_tokens, Spend};
use super::Inputs;
use crate::analyze::findings::severity_from_usd;
use crate::analyze::{
    mark_findings_with_unpriced_sessions, tally_unpriced, unpriced_usage_findings,
    FindingPricingStatus, InterveningStep, WasteFinding, WasteSeverity,
};
use crate::query_verbs::HotspotDetections;
use crate::reader::{StopReason, TurnRecord};

mod detectors;

use detectors::Detail;

/// Context growth at or above this many tokens in one step is a finding.
const CONTEXT_GROWTH_FINDING_TOKENS: i64 = 20_000;
/// Context-growth findings kept, largest first.
const CONTEXT_GROWTH_FINDINGS: usize = 3;
/// Turn ids listed per finding.
const EVIDENCE_TURNS: usize = 50;

/// Turn lookups shared by every finding builder.
pub(super) struct FindingContext<'a> {
    turns: HashMap<u64, &'a TurnRecord>,
}

impl<'a> FindingContext<'a> {
    fn new(turns: &'a [TurnRecord]) -> Self {
        Self {
            turns: turns.iter().map(|t| (t.turn_index, t)).collect(),
        }
    }

    /// Tokens of the turns `evidence` names; `None` when it names none.
    fn evidence_tokens(&self, evidence: &FindingEvidence) -> Option<u64> {
        let tokens = evidence
            .turn_indexes
            .iter()
            .filter_map(|i| self.turns.get(i))
            .map(|t| turn_tokens(t));
        (!evidence.turn_indexes.is_empty()).then(|| tokens.sum())
    }

    /// Evidence naming the turns at `indexes`.
    pub(super) fn turns(&self, indexes: impl IntoIterator<Item = u64>) -> FindingEvidence {
        let turn_indexes: Vec<u64> = indexes.into_iter().take(EVIDENCE_TURNS).collect();
        FindingEvidence {
            turn_ids: turn_indexes
                .iter()
                .filter_map(|i| self.turns.get(i).map(|t| t.message_id.clone()))
                .collect(),
            turn_indexes,
            ..Default::default()
        }
    }
}

/// Every finding for the session, most severe and most expensive first.
pub(super) fn findings(
    inputs: &Inputs<'_>,
    detections: &HotspotDetections,
    overhead: &Section<OverheadReport>,
    context: &Section<ContextReport>,
    attribution_refusal: Option<&str>,
) -> Vec<Finding> {
    let turns = &inputs.records.turns;
    let cx = FindingContext::new(turns);
    let (mut wastes, mut details) = detectors::detections(detections, &cx);
    for unpriced in unpriced_usage_findings(turns, inputs.pricing) {
        wastes.push(unpriced);
        details.push(Detail::from(FindingEvidence {
            models: tally_unpriced(turns, inputs.pricing).1,
            ..Default::default()
        }));
    }
    // Every detection belongs to this session, including installed-surface
    // rows keyed by their file, so unpriced marking reaches all of them.
    // Marking also stands the session's whole unpriced volume in for a
    // missing token estimate; each finding keeps its own estimate instead.
    let own_tokens: Vec<Option<u64>> = wastes
        .iter_mut()
        .map(|w| {
            w.session_id = inputs.evidence.session.session_id.clone();
            w.estimated_savings.tokens_per_session
        })
        .collect();
    mark_findings_with_unpriced_sessions(&mut wastes, turns, inputs.pricing);
    let priced = wastes
        .iter()
        .all(|w| w.pricing_status == FindingPricingStatus::Priced);
    let mut out: Vec<Finding> = wastes
        .into_iter()
        .zip(details)
        .zip(own_tokens)
        .map(|((waste, detail), tokens)| from_waste(waste, detail, tokens, &cx))
        .collect();
    if let Some(report) = overhead.data() {
        out.extend(overhead_findings(report, priced));
    }
    if let Some(report) = context.data() {
        out.extend(context_findings(report, priced));
    }
    out.extend(stop_findings(turns, &cx, inputs.pricing));
    out.extend(fidelity_findings(turns, &cx, attribution_refusal));
    out.sort_by(rank);
    out
}

fn from_waste(
    waste: WasteFinding,
    detail: Detail,
    own_tokens: Option<u64>,
    cx: &FindingContext<'_>,
) -> Finding {
    let (why, suggestion) = guidance(&waste.kind);
    let suggestion = detail.suggestion.unwrap_or(suggestion);
    let evidence = detail.evidence;
    let tokens = own_tokens.or_else(|| cx.evidence_tokens(&evidence));
    let cost_usd = match waste.pricing_status {
        FindingPricingStatus::Priced => waste.estimated_savings.usd_per_session,
        FindingPricingStatus::Unpriced => None,
    };
    Finding {
        explanation: join(&waste.detail, why),
        suggestion: suggestion.to_string(),
        code: waste.kind,
        severity: waste.severity,
        title: waste.title,
        evidence,
        impact: FindingImpact {
            tokens,
            cost_usd,
            pricing: waste.pricing_status,
        },
        actions: waste.actions,
    }
}

/// A finding built here rather than by a detector adapter.
fn finding(
    code: &str,
    severity: WasteSeverity,
    title: String,
    what: String,
    evidence: FindingEvidence,
    impact: FindingImpact,
) -> Finding {
    let (why, suggestion) = guidance(code);
    Finding {
        code: code.to_string(),
        severity,
        title,
        explanation: join(&what, why),
        evidence,
        impact,
        suggestion: suggestion.to_string(),
        actions: Vec::new(),
    }
}

fn impact(tokens: u64, usd: Option<f64>, priced: bool) -> FindingImpact {
    FindingImpact {
        tokens: Some(tokens),
        cost_usd: usd.filter(|_| priced),
        pricing: if priced {
            FindingPricingStatus::Priced
        } else {
            FindingPricingStatus::Unpriced
        },
    }
}

fn join(what: &str, why: &str) -> String {
    match (what.is_empty(), why.is_empty()) {
        (_, true) => what.to_string(),
        (true, false) => why.to_string(),
        (false, false) => format!("{what} {why}"),
    }
}

fn overhead_findings(report: &OverheadReport, priced: bool) -> Vec<Finding> {
    report
        .trim
        .recommendations
        .iter()
        .filter_map(|rec| {
            let rides = riding_turns(report, &rec.file);
            (rides > 0).then(|| {
                let usd = rec.projected_savings.per_session_usd;
                finding(
                    "instruction-overhead",
                    severity_from_usd(usd),
                    format!("Trim \"{}\" in {}", rec.section.heading, rec.file),
                    format!(
                        "Lines {}-{} of {} ({} tokens, {:.0}% of the file) were re-read from cache on {rides} turn(s) of this session.",
                        rec.section.start_line,
                        rec.section.end_line,
                        rec.file,
                        rec.section.tokens,
                        rec.projected_savings.token_share * 100.0,
                    ),
                    FindingEvidence {
                        files: vec![rec.file.clone()],
                        targets: vec![rec.section.heading.clone()],
                        ..Default::default()
                    },
                    impact(rec.section.tokens * rides, Some(usd), priced),
                )
            })
        })
        .collect()
}

/// Turns of the session that carried the instruction file `file` (a
/// project-relative path) in their cached context.
fn riding_turns(report: &OverheadReport, file: &str) -> u64 {
    report
        .attribution
        .per_file
        .iter()
        .filter(|entry| entry.path.replace('\\', "/").ends_with(file))
        .flat_map(|entry| &entry.attribution.session_costs)
        .map(|session| session.riding_turns)
        .max()
        .unwrap_or(0)
}

fn context_findings(report: &ContextReport, priced: bool) -> Vec<Finding> {
    report
        .largest_growth
        .iter()
        .filter(|d| d.delta_tokens >= CONTEXT_GROWTH_FINDING_TOKENS)
        .take(CONTEXT_GROWTH_FINDINGS)
        .map(|delta| {
            let mut tools: Vec<String> = Vec::new();
            for step in &delta.intervening {
                if let InterveningStep::ToolResult { tool_name, .. } = step {
                    if !tools.contains(tool_name) {
                        tools.push(tool_name.clone());
                    }
                }
            }
            let evidence = FindingEvidence {
                turn_ids: vec![delta.turn_id.clone()],
                tools: tools.clone(),
                ..Default::default()
            };
            let cause = if tools.is_empty() {
                String::new()
            } else {
                format!(", added by {} output", tools.join(", "))
            };
            finding(
                "context-growth",
                severity_from_usd(delta.attributed_cost_usd),
                format!("Context grew by {} tokens in one step", delta.delta_tokens),
                format!(
                    "Before inference {} of turn {} the context went from {} to {} tokens{cause}.",
                    delta.inference_idx,
                    delta.turn_id,
                    delta.prior_context_tokens,
                    delta.current_context_tokens,
                ),
                evidence,
                impact(
                    delta.delta_tokens as u64,
                    Some(delta.attributed_cost_usd),
                    priced,
                ),
            )
        })
        .collect()
}

fn stop_findings(
    turns: &[TurnRecord],
    cx: &FindingContext<'_>,
    pricing: &crate::analyze::PricingTable,
) -> Vec<Finding> {
    let pick = |reason: StopReason| -> Vec<&TurnRecord> {
        turns
            .iter()
            .filter(|t| t.stop_reason == Some(reason))
            .collect()
    };
    let mut out = Vec::new();
    for (reason, code, severity, label) in [
        (
            StopReason::MaxTokens,
            "max-tokens-stop",
            WasteSeverity::Warn,
            "hit the output token limit",
        ),
        (
            StopReason::Refusal,
            "refusal",
            WasteSeverity::Info,
            "ended in a refusal",
        ),
    ] {
        let hit = pick(reason);
        if hit.is_empty() {
            continue;
        }
        let mut spend = Spend::default();
        for turn in &hit {
            spend.add(turn, pricing);
        }
        out.push(finding(
            code,
            severity,
            format!("{} turn(s) {label}", hit.len()),
            format!("{} turn(s) {label}.", hit.len()),
            cx.turns(hit.iter().map(|t| t.turn_index)),
            impact(spend.tokens, spend.cost_usd(), spend.cost_usd().is_some()),
        ));
    }
    out
}

fn fidelity_findings(
    turns: &[TurnRecord],
    cx: &FindingContext<'_>,
    attribution_refusal: Option<&str>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    let unrecorded: Vec<&TurnRecord> = turns
        .iter()
        .filter(|t| {
            t.fidelity
                .as_ref()
                .is_some_and(|f| !(f.coverage.has_input_tokens && f.coverage.has_output_tokens))
        })
        .collect();
    if !unrecorded.is_empty() {
        out.push(finding(
            "usage-unrecorded",
            WasteSeverity::Warn,
            format!("{} turn(s) carry no token usage", unrecorded.len()),
            format!(
                "{} of {} turns have no recorded input/output token counts.",
                unrecorded.len(),
                turns.len()
            ),
            cx.turns(unrecorded.iter().map(|t| t.turn_index)),
            FindingImpact {
                tokens: None,
                cost_usd: None,
                pricing: FindingPricingStatus::Unpriced,
            },
        ));
    }
    if let Some(reason) = attribution_refusal {
        out.push(finding(
            "attribution-unavailable",
            WasteSeverity::Info,
            "Cost attribution unavailable".to_string(),
            format!("Hotspot attribution was refused: {reason}."),
            FindingEvidence::default(),
            FindingImpact {
                tokens: None,
                cost_usd: None,
                pricing: FindingPricingStatus::Priced,
            },
        ));
    }
    out
}

/// Severity first, then estimated impact: priced cost, else tokens. A
/// finding without a known impact sorts after every finding with one.
pub(super) fn rank(a: &Finding, b: &Finding) -> std::cmp::Ordering {
    let ((tier_a, size_a), (tier_b, size_b)) = (impact_rank(&a.impact), impact_rank(&b.impact));
    b.severity
        .cmp(&a.severity)
        .then(tier_b.cmp(&tier_a))
        .then(size_b.total_cmp(&size_a))
}

/// `(tier, magnitude)`: tier 2 for a nonzero priced cost, 1 for a nonzero
/// token count, 0 for no known impact.
fn impact_rank(impact: &FindingImpact) -> (u8, f64) {
    let cost = impact
        .cost_usd
        .filter(|c| *c > 0.0 && impact.pricing == FindingPricingStatus::Priced);
    match (cost, impact.tokens.filter(|t| *t > 0)) {
        (Some(cost), _) => (2, cost),
        (None, Some(tokens)) => (1, tokens as f64),
        (None, None) => (0, 0.0),
    }
}
