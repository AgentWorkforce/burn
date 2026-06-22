use super::*;

use std::collections::HashSet;

use crate::reader::{ToolResultEventRecord, ToolResultEventSource, ToolResultStatus, TurnRecord};

use crate::analyze::findings::{CancellationRun, FailureRun, FailureRunErrorSignature, RetryLoop};
use crate::analyze::pricing::PricingTable;

pub(super) struct GraphStatusPatterns {
    pub(super) retry_loops: Vec<RetryLoop>,
    pub(super) failure_runs: Vec<FailureRun>,
    pub(super) cancelled_runs: Vec<CancellationRun>,
}

pub(super) fn detect_graph_status_patterns_for_session<'a>(
    session_id: &str,
    turns: &[&'a TurnRecord],
    events: &[&'a ToolResultEventRecord],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> GraphStatusPatterns {
    let terminal_refs = build_terminal_event_refs(session_id, turns, events);
    GraphStatusPatterns {
        retry_loops: detect_graph_retry_loops_for_session(
            session_id,
            &terminal_refs,
            pricing,
            content_index,
        ),
        failure_runs: detect_graph_failure_runs_for_session(
            session_id,
            &terminal_refs,
            pricing,
            content_index,
        ),
        cancelled_runs: detect_graph_cancellation_runs_for_session(
            session_id,
            &terminal_refs,
            pricing,
        ),
    }
}

fn detect_graph_retry_loops_for_session<'a>(
    session_id: &str,
    refs: &[ToolResultEventRef<'a>],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> Vec<RetryLoop> {
    detect_streaks(
        refs.iter().cloned(),
        |head, r| {
            let is_errored = matches!(r.event.status, ToolResultStatus::Errored);
            if !is_errored || r.call.is_none() || r.args_hash.is_none() {
                StreakOp::Break
            } else if let Some(head) = head {
                if head.tool == r.tool && head.args_hash == r.args_hash {
                    StreakOp::Extend
                } else {
                    StreakOp::Rotate
                }
            } else {
                StreakOp::Extend
            }
        },
        |streak| {
            if streak.len() < MIN_RETRY_LEN {
                return None;
            }
            let first = streak.first().unwrap();
            let last = streak.last().unwrap();
            let contributing = dedup_defined_turns(streak);
            let mut loop_ = RetryLoop {
                session_id: session_id.to_string(),
                tool: first.tool.clone(),
                target: first.target.clone(),
                args_hash: first.args_hash.clone().unwrap_or_default(),
                attempts: streak.len() as u64,
                start_turn_index: first.turn_index,
                end_turn_index: last.turn_index,
                cost: sum_cost_for_turns(&contributing, pricing),
                error_signature: None,
                event_source: Some(coalesce_event_source(streak)),
            };
            let call_refs = event_refs_to_tool_call_refs(streak);
            if let Some(sig) = retry_loop_signature(&call_refs, content_index) {
                loop_.error_signature = Some(sig);
            }
            Some(loop_)
        },
    )
}

fn detect_graph_failure_runs_for_session<'a>(
    session_id: &str,
    refs: &[ToolResultEventRef<'a>],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> Vec<FailureRun> {
    detect_streaks(
        refs.iter().cloned(),
        |_head, r| {
            if matches!(r.event.status, ToolResultStatus::Errored) {
                StreakOp::Extend
            } else {
                StreakOp::Break
            }
        },
        |streak| {
            if streak.len() < MIN_FAILURE_RUN_LEN {
                return None;
            }
            let mut keys: HashSet<String> = HashSet::new();
            for r in streak.iter() {
                keys.insert(status_pattern_key(r));
            }
            let has_non_tool_result = streak
                .iter()
                .any(|r| !matches!(r.event.event_source, ToolResultEventSource::ToolResult));
            // A same-(tool,args) tool_result run is a retry loop. Non-tool_result
            // terminal events (notably subagent notifications) remain failure
            // runs — they represent child invocations ending badly, not a parent
            // retry loop. Mirrors patterns.ts:706-710.
            if keys.len() < 2 && !has_non_tool_result {
                return None;
            }
            let first = streak.first().unwrap();
            let last = streak.last().unwrap();
            // First-seen unique tool order.
            let mut tools: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for r in streak.iter() {
                if seen.insert(r.tool.clone()) {
                    tools.push(r.tool.clone());
                }
            }
            let contributing = dedup_defined_turns(streak);
            let mut run = FailureRun {
                session_id: session_id.to_string(),
                length: streak.len() as u64,
                start_turn_index: first.turn_index,
                end_turn_index: last.turn_index,
                tools_involved: tools,
                cost: sum_cost_for_turns(&contributing, pricing),
                error_signatures: None,
                event_source: Some(coalesce_event_source(streak)),
            };
            let call_refs = event_refs_to_tool_call_refs(streak);
            let sigs = failure_run_signatures(&call_refs, content_index);
            if !sigs.is_empty() {
                run.error_signatures = Some(sigs);
            }
            Some(run)
        },
    )
}

fn detect_graph_cancellation_runs_for_session<'a>(
    session_id: &str,
    refs: &[ToolResultEventRef<'a>],
    pricing: &PricingTable,
) -> Vec<CancellationRun> {
    detect_streaks(
        refs.iter().cloned(),
        |_head, r| {
            if matches!(r.event.status, ToolResultStatus::Cancelled) {
                StreakOp::Extend
            } else {
                StreakOp::Break
            }
        },
        |streak| {
            if streak.is_empty() {
                return None;
            }
            let first = streak.first().unwrap();
            let last = streak.last().unwrap();
            let mut tools: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for r in streak.iter() {
                if seen.insert(r.tool.clone()) {
                    tools.push(r.tool.clone());
                }
            }
            let contributing = dedup_defined_turns(streak);
            Some(CancellationRun {
                session_id: session_id.to_string(),
                length: streak.len() as u64,
                start_turn_index: first.turn_index,
                end_turn_index: last.turn_index,
                tools_involved: tools,
                cost: sum_cost_for_turns(&contributing, pricing),
                event_source: coalesce_event_source(streak),
            })
        },
    )
}

fn status_pattern_key(r: &ToolResultEventRef<'_>) -> String {
    let args = r
        .args_hash
        .clone()
        .unwrap_or_else(|| r.event.tool_use_id.clone());
    format!("{}|{}", r.tool, args)
}

pub(crate) fn detect_retry_loops_for_session<'a>(
    session_id: &str,
    turns: &'a [&'a TurnRecord],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> Vec<RetryLoop> {
    detect_streaks(
        flatten_tool_calls(turns),
        |head, r| {
            if r.call.is_error != Some(true) {
                StreakOp::Break
            } else if let Some(head) = head {
                if head.call.name == r.call.name && head.call.args_hash == r.call.args_hash {
                    StreakOp::Extend
                } else {
                    StreakOp::Rotate
                }
            } else {
                StreakOp::Extend
            }
        },
        |streak| {
            if streak.len() < MIN_RETRY_LEN {
                return None;
            }
            let first = streak.first().unwrap();
            let last = streak.last().unwrap();
            let turns_in_streak: Vec<&TurnRecord> = streak.iter().map(|r| r.turn).collect();
            let contributing = dedup_turns(turns_in_streak);
            let mut loop_ = RetryLoop {
                session_id: session_id.to_string(),
                tool: first.call.name.clone(),
                target: first.call.target.clone(),
                args_hash: first.call.args_hash.clone(),
                attempts: streak.len() as u64,
                start_turn_index: first.turn.turn_index,
                end_turn_index: last.turn.turn_index,
                cost: sum_cost_for_turns(&contributing, pricing),
                error_signature: None,
                event_source: None,
            };
            if let Some(sig) = retry_loop_signature(streak, content_index) {
                loop_.error_signature = Some(sig);
            }
            Some(loop_)
        },
    )
}

fn retry_loop_signature(
    streak: &[ToolCallRef<'_>],
    content_index: Option<&ContentIndex>,
) -> Option<String> {
    let idx = content_index?;
    let mut first_sig: Option<String> = None;
    let mut diverged = false;
    for r in streak {
        let result = idx.tool_results.get(&r.call.id);
        let sig = extract_error_signature(result);
        let Some(sig) = sig else { continue };
        match &first_sig {
            None => first_sig = Some(sig),
            Some(existing) => {
                if existing != &sig {
                    diverged = true;
                    break;
                }
            }
        }
    }
    let first = first_sig?;
    if diverged {
        Some(format!("{first} (signatures diverged)"))
    } else {
        Some(first)
    }
}

pub(crate) fn detect_failure_runs_for_session<'a>(
    session_id: &str,
    turns: &'a [&'a TurnRecord],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> Vec<FailureRun> {
    detect_streaks(
        flatten_tool_calls(turns),
        |_head, r| {
            if r.call.is_error == Some(true) {
                StreakOp::Extend
            } else {
                StreakOp::Break
            }
        },
        |streak| {
            if streak.len() < MIN_FAILURE_RUN_LEN {
                return None;
            }
            let mut keys: HashSet<String> = HashSet::new();
            for r in streak.iter() {
                keys.insert(format!("{}|{}", r.call.name, r.call.args_hash));
            }
            // Same-(tool,args) run is a retry loop, not a failure run. See
            // patterns.ts:868-872.
            if keys.len() < 2 {
                return None;
            }
            let first = streak.first().unwrap();
            let last = streak.last().unwrap();
            let mut tools: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for r in streak.iter() {
                if seen.insert(r.call.name.clone()) {
                    tools.push(r.call.name.clone());
                }
            }
            let turns_in_streak: Vec<&TurnRecord> = streak.iter().map(|r| r.turn).collect();
            let contributing = dedup_turns(turns_in_streak);
            let mut run = FailureRun {
                session_id: session_id.to_string(),
                length: streak.len() as u64,
                start_turn_index: first.turn.turn_index,
                end_turn_index: last.turn.turn_index,
                tools_involved: tools,
                cost: sum_cost_for_turns(&contributing, pricing),
                error_signatures: None,
                event_source: None,
            };
            let sigs = failure_run_signatures(streak, content_index);
            if !sigs.is_empty() {
                run.error_signatures = Some(sigs);
            }
            Some(run)
        },
    )
}

fn failure_run_signatures(
    streak: &[ToolCallRef<'_>],
    content_index: Option<&ContentIndex>,
) -> Vec<FailureRunErrorSignature> {
    let Some(idx) = content_index else {
        return Vec::new();
    };
    let mut out: Vec<FailureRunErrorSignature> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for r in streak {
        if seen.contains(&r.call.name) {
            continue;
        }
        let result = idx.tool_results.get(&r.call.id);
        let Some(sig) = extract_error_signature(result) else {
            continue;
        };
        out.push(FailureRunErrorSignature {
            tool: r.call.name.clone(),
            first_line: sig,
        });
        seen.insert(r.call.name.clone());
    }
    out
}
