//! Per-type subagent rollups across sessions.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::subagent_tree::BuildSubagentTreeOptions;
use crate::analyze::cost::total_cost_for_turn;
use crate::analyze::util::percentile;
use crate::reader::TurnRecord;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentTypeStats {
    pub subagent_type: String,
    pub invocations: u64,
    pub turns: u64,
    pub total_cost: f64,
    pub median_cost: f64,
    pub p95_cost: f64,
    pub mean_cost: f64,
}

/// Aggregate subagent invocations across sessions by `subagentType`. An
/// invocation is the unique `(sessionId, agentId)` pair so the same agent id
/// re-used across sessions doesn't collide.
pub(crate) fn aggregate_subagent_type_stats(
    turns: &[TurnRecord],
    opts: &BuildSubagentTreeOptions<'_>,
) -> Vec<SubagentTypeStats> {
    #[derive(Default)]
    struct Inv {
        ty: String,
        turns: u64,
        cost: f64,
    }
    let mut by_invocation: IndexMap<String, Inv> = IndexMap::new();
    for t in turns {
        let Some(sub) = &t.subagent else { continue };
        let Some(agent_id) = &sub.agent_id else {
            continue;
        };
        let ty = sub
            .subagent_type
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string());
        let key = format!("{}:{}", t.session_id, agent_id);
        let inv = by_invocation.entry(key).or_insert_with(|| Inv {
            ty: ty.clone(),
            turns: 0,
            cost: 0.0,
        });
        if inv.ty == "(unknown)" && ty != "(unknown)" {
            inv.ty = ty;
        }
        inv.turns += 1;
        inv.cost += total_cost_for_turn(t, opts.pricing);
    }
    let mut by_type: IndexMap<String, Vec<f64>> = IndexMap::new();
    let mut totals_by_type: IndexMap<String, (u64, f64)> = IndexMap::new();
    for inv in by_invocation.values() {
        by_type.entry(inv.ty.clone()).or_default().push(inv.cost);
        let entry = totals_by_type.entry(inv.ty.clone()).or_insert((0, 0.0));
        entry.0 += inv.turns;
        entry.1 += inv.cost;
    }
    let mut out: Vec<SubagentTypeStats> = Vec::new();
    for (ty, mut costs) in by_type {
        let (turns, total) = *totals_by_type.get(&ty).unwrap();
        costs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let invocations = costs.len() as u64;
        out.push(SubagentTypeStats {
            subagent_type: ty,
            invocations,
            turns,
            total_cost: total,
            median_cost: percentile(&costs, 0.5),
            p95_cost: percentile(&costs, 0.95),
            mean_cost: if invocations > 0 {
                total / invocations as f64
            } else {
                0.0
            },
        });
    }
    out.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}
