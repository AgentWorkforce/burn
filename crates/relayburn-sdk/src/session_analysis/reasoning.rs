//! Reasoning effort: spend per recorded effort level, where the effort
//! changed, and deep effort spent on routine work.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::document::{ActivityRow, Finding, FindingImpact, Section};
use super::findings::{finding, impact, FindingContext};
use super::sections::Spend;
use crate::analyze::findings::severity_from_usd;
use crate::analyze::{cost_for_turn, FindingPricingStatus, PricingTable, WasteSeverity};
use crate::reader::{ActivityCategory, Harness, SourceKind, TurnRecord};

/// Known effort levels, lowest first.
const LEVELS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];
/// Index in [`LEVELS`] from which effort counts as deep (`high`).
const DEEP: usize = 4;
/// Activities whose turns rarely need deep reasoning.
const ROUTINE: [ActivityCategory; 7] = [
    ActivityCategory::Git,
    ActivityCategory::BuildDeploy,
    ActivityCategory::Deps,
    ActivityCategory::Format,
    ActivityCategory::Exploration,
    ActivityCategory::Conversation,
    ActivityCategory::Delegation,
];
/// Deep-effort routine turns needed before they are a finding.
const ROUTINE_MIN_TURNS: usize = 3;
/// Share of the session's spend (cost, else tokens) those turns must carry.
const ROUTINE_MIN_SHARE: f64 = 0.25;
/// Effort changes named in the change finding.
const CHANGES_SHOWN: usize = 5;

/// Session spend per reasoning effort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningBreakdown {
    /// One row per effort, lowest effort first; turns that recorded no
    /// effort last.
    pub levels: Vec<ReasoningEffortRow>,
    /// Turns whose effort differs from the previous turn that recorded one.
    pub changes: Vec<ReasoningEffortChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortRow {
    /// The effort as the harness recorded it; `None` for turns that
    /// recorded none.
    pub effort: Option<String>,
    pub turns: u64,
    pub tokens: u64,
    pub reasoning_tokens: u64,
    /// `None` when any contributing turn's model is unpriced.
    pub cost_usd: Option<f64>,
    /// What the reasoning tokens alone cost; `None` when unpriced.
    pub reasoning_cost_usd: Option<f64>,
    /// The same turns per classified activity, most expensive first.
    pub activities: Vec<ActivityRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortChange {
    pub turn_index: u64,
    pub turn_id: String,
    pub from: String,
    pub to: String,
}

/// The effort a turn recorded.
fn effort_of(turn: &TurnRecord) -> Option<&str> {
    turn.reasoning.as_ref()?.effort.as_deref()
}

fn is_deep(effort: &str) -> bool {
    LEVELS
        .iter()
        .position(|level| *level == effort)
        .is_some_and(|rank| rank >= DEEP)
}

/// Sort key: known levels in order, then unknown levels, then unrecorded.
fn order(effort: Option<&str>) -> (u8, usize) {
    match effort {
        Some(effort) => match LEVELS.iter().position(|level| *level == effort) {
            Some(rank) => (0, rank),
            None => (1, 0),
        },
        None => (2, 0),
    }
}

/// Reasoning tokens and what they cost.
#[derive(Debug, Default, Clone, Copy)]
struct ReasoningSpend {
    tokens: u64,
    usd: f64,
    unpriced: bool,
}

impl ReasoningSpend {
    fn add(&mut self, turn: &TurnRecord, pricing: &PricingTable) {
        self.tokens += turn.usage.reasoning;
        match reasoning_cost(turn, pricing) {
            Some(cost) => self.usd += cost,
            None => self.unpriced = true,
        }
    }

    fn cost_usd(&self) -> Option<f64> {
        (!self.unpriced).then_some(self.usd)
    }
}

/// The turn's price less the price of the same turn without its reasoning
/// tokens (which Codex counts inside its output tokens).
fn reasoning_cost(turn: &TurnRecord, pricing: &PricingTable) -> Option<f64> {
    let full = cost_for_turn(turn, pricing)?.total;
    let reasoning = turn.usage.reasoning;
    if reasoning == 0 {
        return Some(0.0);
    }
    let mut without = turn.clone();
    without.usage.reasoning = 0;
    if turn.source == SourceKind::Codex {
        without.usage.output = without.usage.output.saturating_sub(reasoning);
    }
    Some(full - cost_for_turn(&without, pricing)?.total)
}

#[derive(Default)]
struct Level {
    spend: Spend,
    reasoning: ReasoningSpend,
    activities: IndexMap<Option<ActivityCategory>, Spend>,
}

impl Level {
    fn add(&mut self, turn: &TurnRecord, pricing: &PricingTable) {
        self.spend.add(turn, pricing);
        self.reasoning.add(turn, pricing);
        self.activities
            .entry(turn.activity)
            .or_default()
            .add(turn, pricing);
    }

    fn row(self, effort: Option<&str>) -> ReasoningEffortRow {
        let mut activities: Vec<(Option<ActivityCategory>, Spend)> =
            self.activities.into_iter().collect();
        activities.sort_by(|a, b| {
            b.1.rank()
                .partial_cmp(&a.1.rank())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ReasoningEffortRow {
            effort: effort.map(str::to_string),
            turns: self.spend.turns,
            tokens: self.spend.tokens,
            reasoning_tokens: self.reasoning.tokens,
            cost_usd: self.spend.cost_usd(),
            reasoning_cost_usd: self.reasoning.cost_usd(),
            activities: activities
                .into_iter()
                .map(|(category, spend)| ActivityRow {
                    category,
                    turns: spend.turns,
                    tokens: spend.tokens,
                    cost_usd: spend.cost_usd(),
                })
                .collect(),
        }
    }
}

/// Turns, tokens and cost per reasoning effort the turns recorded, lowest
/// effort first and turns without one last; empty when no turn recorded
/// an effort.
pub fn reasoning_effort_rows(
    turns: &[TurnRecord],
    pricing: &PricingTable,
) -> Vec<ReasoningEffortRow> {
    if turns.iter().all(|t| effort_of(t).is_none()) {
        return Vec::new();
    }
    let mut levels: IndexMap<Option<&str>, Level> = IndexMap::new();
    for turn in turns {
        levels
            .entry(effort_of(turn))
            .or_default()
            .add(turn, pricing);
    }
    let mut levels: Vec<ReasoningEffortRow> = levels
        .into_iter()
        .map(|(effort, level)| level.row(effort))
        .collect();
    levels.sort_by_key(|row| order(row.effort.as_deref()));
    levels
}

/// The reasoning section, or why the session has none.
pub(super) fn breakdown(
    harness: Harness,
    turns: &[TurnRecord],
    pricing: &PricingTable,
) -> Section<ReasoningBreakdown> {
    if turns.is_empty() {
        return Section::unavailable("the session has no assistant turns");
    }
    if turns.iter().all(|t| effort_of(t).is_none()) {
        return Section::unavailable(format!(
            "no turn of this {harness} session records a reasoning effort"
        ));
    }
    Section::available(ReasoningBreakdown {
        levels: reasoning_effort_rows(turns, pricing),
        changes: changes(turns),
    })
}

fn changes(turns: &[TurnRecord]) -> Vec<ReasoningEffortChange> {
    let mut out = Vec::new();
    let mut last: Option<&str> = None;
    for turn in turns {
        let Some(effort) = effort_of(turn) else {
            continue;
        };
        if let Some(previous) = last.filter(|previous| *previous != effort) {
            out.push(ReasoningEffortChange {
                turn_index: turn.turn_index,
                turn_id: turn.message_id.clone(),
                from: previous.to_string(),
                to: effort.to_string(),
            });
        }
        last = Some(effort);
    }
    out
}

/// Findings over the reasoning section.
pub(super) fn findings(
    turns: &[TurnRecord],
    report: &ReasoningBreakdown,
    pricing: &PricingTable,
    cx: &FindingContext<'_>,
) -> Vec<Finding> {
    let mut out: Vec<Finding> = routine_finding(turns, pricing, cx).into_iter().collect();
    out.extend(change_finding(report, cx));
    out
}

fn label(activity: ActivityCategory) -> String {
    serde_json::to_value(activity)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn distinct<T: PartialEq>(values: impl Iterator<Item = T>) -> Vec<T> {
    let mut out = Vec::new();
    for value in values {
        if !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

fn usd(value: f64) -> String {
    format!("${value:.4}")
}

/// `n` with thousands separators.
fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Deep-effort turns on routine activities that carry a large share of the
/// session's spend.
fn routine_finding(
    turns: &[TurnRecord],
    pricing: &PricingTable,
    cx: &FindingContext<'_>,
) -> Option<Finding> {
    let deep: Vec<&TurnRecord> = turns
        .iter()
        .filter(|t| effort_of(t).is_some_and(is_deep))
        .filter(|t| t.activity.is_some_and(|a| ROUTINE.contains(&a)))
        .collect();
    if deep.len() < ROUTINE_MIN_TURNS {
        return None;
    }
    let (mut session, mut spend, mut reasoning) = (
        Spend::default(),
        Spend::default(),
        ReasoningSpend::default(),
    );
    turns.iter().for_each(|t| session.add(t, pricing));
    for turn in &deep {
        spend.add(turn, pricing);
        reasoning.add(turn, pricing);
    }
    let (share, basis) = match (spend.cost_usd(), session.cost_usd()) {
        (Some(part), Some(whole)) if whole > 0.0 => (part / whole, "cost"),
        _ => (spend.tokens as f64 / session.tokens.max(1) as f64, "tokens"),
    };
    if share < ROUTINE_MIN_SHARE || reasoning.tokens == 0 {
        return None;
    }
    let efforts = distinct(deep.iter().filter_map(|t| effort_of(t))).join("/");
    let activities = distinct(deep.iter().filter_map(|t| t.activity.map(label)));
    let cost = reasoning.cost_usd();
    let reasoning_cost = cost.map(|c| format!(" ({})", usd(c))).unwrap_or_default();
    let mut evidence = cx.turns(deep.iter().map(|t| t.turn_index));
    evidence.targets = activities.clone();
    Some(finding(
        "high-effort-routine-work",
        cost.map_or(WasteSeverity::Info, severity_from_usd),
        format!("{} routine turn(s) ran at {efforts} reasoning effort", deep.len()),
        format!(
            "{} of {} turns ran at {efforts} effort on {} work and carried {:.0}% of the session's {basis}; they spent {} reasoning tokens{reasoning_cost}, the most lower effort could save on them.",
            deep.len(),
            turns.len(),
            activities.join(", "),
            share * 100.0,
            count(reasoning.tokens),
        ),
        evidence,
        impact(reasoning.tokens, cost, cost.is_some()),
    ))
}

/// Effort changed mid-session: where, and what a turn cost at each level.
fn change_finding(report: &ReasoningBreakdown, cx: &FindingContext<'_>) -> Option<Finding> {
    let first = report.changes.first()?;
    let shown: Vec<String> = report
        .changes
        .iter()
        .take(CHANGES_SHOWN)
        .map(|c| format!("{} to {} at turn {}", c.from, c.to, c.turn_index))
        .collect();
    let per_level: Vec<String> = report
        .levels
        .iter()
        .filter_map(|row| Some(per_turn(row.effort.as_deref()?, row)))
        .collect();
    let priced = report.levels.iter().all(|row| row.cost_usd.is_some());
    Some(finding(
        "reasoning-effort-change",
        WasteSeverity::Info,
        format!(
            "Reasoning effort changed {} time(s), first {} to {}",
            report.changes.len(),
            first.from,
            first.to
        ),
        format!(
            "Effort went from {}. Per turn: {}.",
            shown.join(", "),
            per_level.join("; ")
        ),
        cx.turns(report.changes.iter().map(|c| c.turn_index)),
        FindingImpact {
            tokens: None,
            cost_usd: None,
            pricing: if priced {
                FindingPricingStatus::Priced
            } else {
                FindingPricingStatus::Unpriced
            },
        },
    ))
}

fn per_turn(effort: &str, row: &ReasoningEffortRow) -> String {
    let turns = row.turns.max(1);
    let cost = row
        .cost_usd
        .map(|c| format!(", {}", usd(c / turns as f64)))
        .unwrap_or_default();
    format!(
        "{effort} averaged {} tokens ({} reasoning{cost}) over {} turn(s)",
        count(row.tokens / turns),
        count(row.reasoning_tokens / turns),
        row.turns
    )
}

#[cfg(test)]
mod tests;
