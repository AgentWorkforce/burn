//! Estimated tokens saved by replacement tools — Rust port of
//! `packages/analyze/src/replacement-savings.ts`.
//!
//! The reader back-populates `replacedTools` and `collapsedCalls` onto each
//! `ToolCall` when the upstream tool result carries a
//! `_meta.replaces` / `_meta.collapsedCalls` annotation. This module turns
//! those counterfactuals into a tokens-saved estimate using a static lookup
//! table keyed by the *replaced* tool name.

use std::collections::HashMap;

use indexmap::IndexMap;
use phf::phf_map;
use relayburn_reader::{ToolCall, TurnRecord};

/// Average tokens (input + output) one vanilla call of each tool consumes.
/// Numbers mirror `DEFAULT_REPLACED_TOOL_TOKEN_COST` in the TS implementation.
pub static DEFAULT_REPLACED_TOOL_TOKEN_COST: phf::Map<&'static str, u32> = phf_map! {
    "Bash" => 600u32,
    "BashOutput" => 400u32,
    "Edit" => 700u32,
    "Glob" => 250u32,
    "Grep" => 900u32,
    "KillShell" => 100u32,
    "LS" => 300u32,
    "MultiEdit" => 1100u32,
    "NotebookEdit" => 900u32,
    "Read" => 2200u32,
    "Task" => 5000u32,
    "TodoWrite" => 250u32,
    "WebFetch" => 3000u32,
    "WebSearch" => 2500u32,
    "Write" => 1600u32,
};

/// Fallback token-cost when a replaced tool isn't in the table.
pub const DEFAULT_FALLBACK_TOKEN_COST: u32 = 800;

#[derive(Debug, Clone, Default)]
pub struct ReplacementSavingsOptions {
    /// Per-tool token-cost override. Provided values are merged on top of the
    /// builtin defaults.
    pub cost_per_call: Option<HashMap<String, u32>>,
    /// Override the fallback used when a replaced tool name is unknown.
    pub fallback_cost_per_call: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallSavings {
    pub collapsed_calls: u64,
    pub replaced_tools: Vec<String>,
    pub estimated_tokens_saved: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolSavingsAggregate {
    pub calls: u64,
    pub collapsed_calls: u64,
    pub estimated_tokens_saved: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplacementSavingsSummary {
    pub calls: u64,
    pub collapsed_calls: u64,
    pub estimated_tokens_saved: u64,
    /// Per-replacement-tool aggregate keyed by `ToolCall.name`. `IndexMap` to
    /// preserve first-seen insertion order, matching the TS `Map` semantics.
    pub by_tool: IndexMap<String, ToolSavingsAggregate>,
}

fn lookup_cost(
    name: &str,
    overrides: Option<&HashMap<String, u32>>,
    fallback: u32,
) -> u32 {
    if let Some(o) = overrides {
        if let Some(v) = o.get(name) {
            return *v;
        }
    }
    DEFAULT_REPLACED_TOOL_TOKEN_COST
        .get(name)
        .copied()
        .unwrap_or(fallback)
}

fn resolve_fallback(opts: Option<&ReplacementSavingsOptions>) -> u32 {
    opts.and_then(|o| o.fallback_cost_per_call)
        .unwrap_or(DEFAULT_FALLBACK_TOKEN_COST)
}

fn average_replaced_cost(
    replaced: &[String],
    overrides: Option<&HashMap<String, u32>>,
    fallback: u32,
) -> f64 {
    if replaced.is_empty() {
        return fallback as f64;
    }
    let mut total: u64 = 0;
    for name in replaced {
        total += lookup_cost(name, overrides, fallback) as u64;
    }
    total as f64 / replaced.len() as f64
}

/// Mirrors the TS `Math.round`: half-away-from-zero rounding to the nearest
/// integer. (`f64::round` matches that for non-negative values, which is the
/// only regime here since both inputs are non-negative.)
fn round_to_u64(x: f64) -> u64 {
    if x <= 0.0 {
        return 0;
    }
    x.round() as u64
}

/// Per-call savings estimate. Returns `None` for calls without any
/// counterfactual annotation so callers can skip them in aggregates.
pub fn estimate_savings_for_tool_call(
    call: &ToolCall,
    options: Option<&ReplacementSavingsOptions>,
) -> Option<ToolCallSavings> {
    let collapsed = call.collapsed_calls.unwrap_or(0);
    let replaced: &[String] = call
        .replaced_tools
        .as_deref()
        .unwrap_or(&[]);
    if collapsed == 0 && replaced.is_empty() {
        return None;
    }
    let fallback = resolve_fallback(options);
    let overrides = options.and_then(|o| o.cost_per_call.as_ref());
    let avg = average_replaced_cost(replaced, overrides, fallback);
    // When `collapsedCalls` is missing but `replaces` is present, treat the
    // call as having replaced one of each named tool. Conservative floor.
    let calls = if collapsed > 0 { collapsed } else { replaced.len() as u64 };
    Some(ToolCallSavings {
        collapsed_calls: calls,
        replaced_tools: replaced.to_vec(),
        estimated_tokens_saved: round_to_u64(calls as f64 * avg),
    })
}

pub fn summarize_replacement_savings(
    turns: &[TurnRecord],
    options: Option<&ReplacementSavingsOptions>,
) -> ReplacementSavingsSummary {
    let mut summary = ReplacementSavingsSummary::default();
    for turn in turns {
        for tc in &turn.tool_calls {
            let Some(est) = estimate_savings_for_tool_call(tc, options) else {
                continue;
            };
            summary.calls += 1;
            summary.collapsed_calls += est.collapsed_calls;
            summary.estimated_tokens_saved += est.estimated_tokens_saved;
            let agg = summary
                .by_tool
                .entry(tc.name.clone())
                .or_default();
            agg.calls += 1;
            agg.collapsed_calls += est.collapsed_calls;
            agg.estimated_tokens_saved += est.estimated_tokens_saved;
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use relayburn_reader::{SourceKind, Usage};

    fn empty_usage() -> Usage {
        Usage {
            input: 0,
            output: 0,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    fn turn(tool_calls: Vec<ToolCall>) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "s".to_string(),
            session_path: None,
            message_id: "m".to_string(),
            turn_index: 0,
            ts: "2026-04-20T00:00:00.000Z".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            project: None,
            project_key: None,
            usage: empty_usage(),
            tool_calls,
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: format!("{name}-1"),
            name: name.to_string(),
            target: None,
            args_hash: "h".to_string(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn call_with(name: &str, replaced: Option<Vec<&str>>, collapsed: Option<u64>) -> ToolCall {
        ToolCall {
            id: format!("{name}-1"),
            name: name.to_string(),
            target: None,
            args_hash: "h".to_string(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: replaced.map(|v| v.into_iter().map(String::from).collect()),
            collapsed_calls: collapsed,
        }
    }

    #[test]
    fn returns_none_for_tool_calls_without_annotation() {
        assert!(estimate_savings_for_tool_call(&call("Bash"), None).is_none());
    }

    #[test]
    fn estimates_using_average_per_call_cost_across_replaced_tools() {
        let tc = call_with("relaywash__Search", Some(vec!["Glob", "Grep", "Read"]), Some(9));
        let est = estimate_savings_for_tool_call(&tc, None).expect("est");
        let avg = (DEFAULT_REPLACED_TOOL_TOKEN_COST.get("Glob").unwrap()
            + DEFAULT_REPLACED_TOOL_TOKEN_COST.get("Grep").unwrap()
            + DEFAULT_REPLACED_TOOL_TOKEN_COST.get("Read").unwrap()) as f64
            / 3.0;
        assert_eq!(est.collapsed_calls, 9);
        assert_eq!(est.replaced_tools, vec!["Glob", "Grep", "Read"]);
        assert_eq!(est.estimated_tokens_saved, (9.0 * avg).round() as u64);
    }

    #[test]
    fn falls_back_to_per_call_default_when_unknown_replaced_tool() {
        let tc = call_with("relaywash__Custom", Some(vec!["UnknownTool"]), Some(2));
        let est = estimate_savings_for_tool_call(&tc, None).expect("est");
        assert_eq!(est.estimated_tokens_saved, 2 * 800);
    }

    #[test]
    fn treats_replaces_without_collapsed_calls_as_one_per_listed_name() {
        let tc = call_with("relaywash__Search", Some(vec!["Read", "Grep"]), None);
        let est = estimate_savings_for_tool_call(&tc, None).expect("est");
        assert_eq!(est.collapsed_calls, 2);
    }

    #[test]
    fn aggregates_savings_across_many_turns_and_tool_names() {
        let turns = vec![
            turn(vec![
                call_with("relaywash__Search", Some(vec!["Glob", "Grep", "Read"]), Some(9)),
                call("Bash"),
            ]),
            turn(vec![call_with(
                "relaywash__Search",
                Some(vec!["Read"]),
                Some(4),
            )]),
        ];
        let summary = summarize_replacement_savings(&turns, None);
        assert_eq!(summary.calls, 2);
        assert_eq!(summary.collapsed_calls, 13);
        assert!(summary.estimated_tokens_saved > 0);
        let search = summary.by_tool.get("relaywash__Search").expect("agg");
        assert_eq!(search.calls, 2);
        assert_eq!(search.collapsed_calls, 13);
    }

    #[test]
    fn empty_summary_when_no_turn_carries_annotation() {
        let summary = summarize_replacement_savings(&[turn(vec![call("Bash")])], None);
        assert_eq!(summary.calls, 0);
        assert_eq!(summary.collapsed_calls, 0);
        assert_eq!(summary.estimated_tokens_saved, 0);
        assert!(summary.by_tool.is_empty());
    }
}
