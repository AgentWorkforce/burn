//! Subagent spend: a subagent type that carries a large share of the
//! session's tokens.

use indexmap::IndexMap;

use super::super::document::{Finding, FindingEvidence};
use super::super::sections::{turn_tokens, Spend};
use super::{finding, impact, FindingContext};
use crate::analyze::findings::severity_from_usd;
use crate::analyze::{PricingTable, WasteSeverity};
use crate::reader::TurnRecord;

/// A subagent type at or above this share of the session's tokens is a
/// finding.
const SUBAGENT_SHARE_FINDING: f64 = 0.25;

/// One finding per subagent type whose turns spent at least
/// [`SUBAGENT_SHARE_FINDING`] of the session's tokens, largest first.
pub(super) fn subagent_findings(
    turns: &[TurnRecord],
    cx: &FindingContext<'_>,
    pricing: &PricingTable,
) -> Vec<Finding> {
    let total: u64 = turns.iter().map(turn_tokens).sum();
    if total == 0 {
        return Vec::new();
    }
    let mut by_type: IndexMap<&str, (Spend, Vec<&str>, Vec<u64>)> = IndexMap::new();
    for turn in turns {
        let Some(sub) = turn.subagent.as_ref() else {
            continue;
        };
        let kind = sub.subagent_type.as_deref().unwrap_or("(unknown)");
        let (spend, agents, indexes) = by_type.entry(kind).or_default();
        spend.add(turn, pricing);
        if let Some(agent) = sub.agent_id.as_deref().filter(|a| !agents.contains(a)) {
            agents.push(agent);
        }
        indexes.push(turn.turn_index);
    }
    let mut rows: Vec<_> = by_type
        .into_iter()
        .filter(|(_, (spend, _, _))| spend.tokens as f64 >= total as f64 * SUBAGENT_SHARE_FINDING)
        .collect();
    rows.sort_by_key(|(_, (spend, _, _))| std::cmp::Reverse(spend.tokens));
    rows.into_iter()
        .map(|(kind, (spend, agents, indexes))| {
            let share = spend.tokens as f64 * 100.0 / total as f64;
            let cost = spend.cost_usd();
            finding(
                "subagent-spend",
                cost.map_or(WasteSeverity::Info, severity_from_usd),
                format!("{kind} subagents spent {share:.0}% of the session's tokens"),
                format!(
                    "{} {kind} subagent(s) ran {} turn(s) and spent {} of the session's {total} tokens.",
                    agents.len(),
                    spend.turns,
                    spend.tokens,
                ),
                FindingEvidence {
                    targets: vec![kind.to_string()],
                    ..cx.turns(indexes)
                },
                impact(spend.tokens, cost, cost.is_some()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::reader::{SourceKind, Subagent, Usage};

    fn turn(index: u64, kind: Option<&str>, input: u64) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "s".to_string(),
            session_path: None,
            message_id: format!("m{index}"),
            turn_index: index,
            ts: "2026-07-01T00:00:00.000Z".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            project: None,
            project_key: None,
            usage: Usage {
                input,
                ..Usage::default()
            },
            tool_calls: Vec::new(),
            files_touched: None,
            subagent: kind.map(|k| Subagent {
                is_sidechain: true,
                parent_tool_use_id: None,
                agent_id: Some(format!("agent-{index}")),
                parent_agent_id: Some("s".to_string()),
                subagent_type: Some(k.to_string()),
                description: None,
            }),
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
            reasoning: None,
        }
    }

    #[test]
    fn a_subagent_type_over_a_quarter_of_the_tokens_is_a_finding() {
        let turns = vec![
            turn(0, None, 400),
            turn(1, Some("Explore"), 500),
            turn(2, Some("Explore"), 50),
            turn(3, Some("Plan"), 50),
        ];
        let cx = FindingContext::new(&turns);
        let found = subagent_findings(&turns, &cx, &load_builtin_pricing());
        assert_eq!(found.len(), 1);
        let f = &found[0];
        assert_eq!(f.code, "subagent-spend");
        assert_eq!(
            f.title,
            "Explore subagents spent 55% of the session's tokens"
        );
        assert_eq!(f.evidence.targets, ["Explore"]);
        assert_eq!(f.evidence.turn_indexes, [1, 2]);
        assert_eq!(f.impact.tokens, Some(550));
        assert!(f.impact.cost_usd.is_some());
    }

    #[test]
    fn an_unpriced_subagent_type_has_unknown_cost() {
        let mut turns = vec![turn(0, None, 100), turn(1, Some("Explore"), 900)];
        turns[1].model = "no-such-model".to_string();
        let cx = FindingContext::new(&turns);
        let found = subagent_findings(&turns, &cx, &load_builtin_pricing());
        assert_eq!(found[0].impact.cost_usd, None);
        assert_eq!(found[0].severity, WasteSeverity::Info);
    }
}
