use super::*;

use std::collections::{BTreeSet, HashMap};

use crate::reader::{normalize_tool_name, CompactionEvent, ContentRecord, TurnRecord};

use crate::analyze::cost::{cost_for_usage, CostForUsageOptions};
use crate::analyze::findings::{CompactionLoss, CompactionLostWork};
use crate::analyze::pricing::PricingTable;

pub(super) fn detect_compaction_losses(
    events: &[CompactionEvent],
    turns: &[TurnRecord],
    pricing: &PricingTable,
    content_by_session: Option<&HashMap<String, Vec<ContentRecord>>>,
) -> Vec<CompactionLoss> {
    // turn_by_message_id over the full input for cache pricing lookup.
    let mut turn_by_message_id: HashMap<&str, &TurnRecord> = HashMap::new();
    for t in turns {
        turn_by_message_id.insert(t.message_id.as_str(), t);
    }

    // Group events by session in arrival order.
    let mut events_order: Vec<String> = Vec::new();
    let mut events_by_session: HashMap<String, Vec<&CompactionEvent>> = HashMap::new();
    for e in events {
        if !events_by_session.contains_key(&e.session_id) {
            events_order.push(e.session_id.clone());
        }
        events_by_session
            .entry(e.session_id.clone())
            .or_default()
            .push(e);
    }
    for list in events_by_session.values_mut() {
        list.sort_by(|a, b| a.ts.cmp(&b.ts));
    }

    // Sort turns by session, then turn_index.
    let mut turns_by_session: HashMap<String, Vec<&TurnRecord>> = HashMap::new();
    for t in turns {
        turns_by_session
            .entry(t.session_id.clone())
            .or_default()
            .push(t);
    }
    for list in turns_by_session.values_mut() {
        list.sort_by_key(|t| t.turn_index);
    }

    let mut prev_boundary_ts: HashMap<String, String> = HashMap::new();
    let mut out: Vec<CompactionLoss> = Vec::new();

    for sid in &events_order {
        let session_events = events_by_session.get(sid).unwrap();
        for e in session_events {
            let tokens = e.tokens_before_compact.unwrap_or(0);
            let mut cache_lost_cost = 0.0_f64;
            if tokens > 0 {
                if let Some(precid) = e.preceding_message_id.as_deref() {
                    if let Some(preceding) = turn_by_message_id.get(precid) {
                        let usage = crate::reader::Usage {
                            input: 0,
                            output: 0,
                            reasoning: 0,
                            cache_read: tokens,
                            cache_create_5m: 0,
                            cache_create_1h: 0,
                        };
                        if let Some(priced) = cost_for_usage(
                            &usage,
                            &preceding.model,
                            pricing,
                            CostForUsageOptions::default(),
                        ) {
                            cache_lost_cost = priced.total;
                        }
                    }
                }
            }
            let mut loss = CompactionLoss {
                session_id: e.session_id.clone(),
                ts: e.ts.clone(),
                preceding_message_id: e.preceding_message_id.clone(),
                tokens_before_compact: tokens,
                cache_lost_cost,
                lost_work: None,
            };
            // Gate on content-sidecar presence — `lost_work` is the "with
            // content" enrichment. Mirrors patterns.ts:1066-1074.
            if let Some(map) = content_by_session {
                if map.contains_key(&e.session_id) {
                    let session_turns = turns_by_session
                        .get(&e.session_id)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    let window_start = prev_boundary_ts.get(&e.session_id).cloned();
                    loss.lost_work = Some(summarize_compacted_window(
                        session_turns,
                        window_start.as_deref(),
                        &e.ts,
                    ));
                }
            }
            out.push(loss);
            prev_boundary_ts.insert(e.session_id.clone(), e.ts.clone());
        }
    }
    out
}

fn summarize_compacted_window(
    session_turns: &[&TurnRecord],
    window_start: Option<&str>,
    boundary_ts: &str,
) -> CompactionLostWork {
    let mut bash_count: u64 = 0;
    let mut edit_count: u64 = 0;
    let mut read_count: u64 = 0;
    let mut files: BTreeSet<String> = BTreeSet::new();
    for t in session_turns {
        if let Some(ws) = window_start {
            if t.ts.as_str() <= ws {
                continue;
            }
        }
        if t.ts.as_str() > boundary_ts {
            continue;
        }
        for call in &t.tool_calls {
            let name = normalize_tool_name(&call.name);
            if name == "Bash" {
                bash_count += 1;
            } else if is_edit_tool(name) {
                edit_count += 1;
                if let Some(target) = &call.target {
                    files.insert(target.clone());
                }
            } else if is_read_tool(name) {
                read_count += 1;
            }
        }
    }
    CompactionLostWork {
        files: files.into_iter().collect(),
        bash_count,
        edit_count,
        read_count,
    }
}
