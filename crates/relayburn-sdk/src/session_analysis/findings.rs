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
    let (mut wastes, mut evidence) = detectors::detections(detections, &cx);
    for unpriced in unpriced_usage_findings(turns, inputs.pricing) {
        wastes.push(unpriced);
        evidence.push(FindingEvidence {
            models: tally_unpriced(turns, inputs.pricing).1,
            ..Default::default()
        });
    }
    mark_findings_with_unpriced_sessions(&mut wastes, turns, inputs.pricing);
    let priced = wastes
        .iter()
        .all(|w| w.pricing_status == FindingPricingStatus::Priced);
    let mut out: Vec<Finding> = wastes
        .into_iter()
        .zip(evidence)
        .map(|(waste, evidence)| from_waste(waste, evidence, &cx))
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

fn from_waste(waste: WasteFinding, evidence: FindingEvidence, cx: &FindingContext<'_>) -> Finding {
    let (why, suggestion) = guidance(&waste.kind);
    let tokens = waste
        .estimated_savings
        .tokens_per_session
        .or_else(|| cx.evidence_tokens(&evidence));
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
        .filter(|rec| rec.projected_savings.across_window_usd > 0.0)
        .map(|rec| {
            let usd = rec.projected_savings.per_session_usd;
            finding(
                "instruction-overhead",
                severity_from_usd(usd),
                format!("Trim \"{}\" in {}", rec.section.heading, rec.file),
                format!(
                    "Lines {}-{} of {} ({} tokens, {:.0}% of the file) stayed in the cached context for this session.",
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
                impact(rec.section.tokens, Some(usd), priced),
            )
        })
        .collect()
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

fn rank(a: &Finding, b: &Finding) -> std::cmp::Ordering {
    let severity = |f: &Finding| match f.severity {
        WasteSeverity::High => 0,
        WasteSeverity::Warn => 1,
        WasteSeverity::Info => 2,
    };
    severity(a)
        .cmp(&severity(b))
        .then_with(|| {
            let usd = |f: &Finding| f.impact.cost_usd.unwrap_or(-1.0);
            usd(b)
                .partial_cmp(&usd(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            b.impact
                .tokens
                .unwrap_or(0)
                .cmp(&a.impact.tokens.unwrap_or(0))
        })
}
