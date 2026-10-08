//! Per-turn charging of tool results: the initial cost a turn pays for the
//! results it received, and the persistence cost of results still riding in
//! its cache read.

use super::ToolAttribution;
use crate::analyze::cost::{EffectiveModelRate, PER_MILLION};
use crate::reader::TurnRecord;

/// Charge `pending` tool results to `turn`, which paid for them as new
/// content. Tokens are attributed whether or not the model is priced; cost
/// only when `rate` is known.
pub(super) fn pay_initial(
    attributions: &mut [ToolAttribution],
    pending: &[usize],
    turn: &TurnRecord,
    rate: Option<&EffectiveModelRate>,
    sized: bool,
) {
    let create = (turn.usage.cache_create_5m + turn.usage.cache_create_1h) as f64;
    let new_content = turn.usage.input as f64 + create;
    if new_content <= 0.0 {
        return;
    }
    if !sized {
        // Even-split: with no per-result sizes, divide this turn's
        // (input + cacheCreate) evenly across the prior emit's tool calls.
        let k = pending.len() as f64;
        let cost = rate.map_or(0.0, |rate| {
            (turn.usage.input as f64 / PER_MILLION) * rate.input
                + (create / PER_MILLION) * rate.cache_write
        }) / k;
        for &i in pending {
            attributions[i].initial_tokens = new_content / k;
            attributions[i].initial_cost = cost;
            attributions[i].total_cost += cost;
        }
        return;
    }
    let sibling_total: f64 = pending
        .iter()
        .map(|&i| attributions[i].result_tokens as f64)
        .sum();
    if sibling_total <= 0.0 {
        return;
    }
    let input_share = turn.usage.input as f64 / new_content;
    let per_token_price = rate.map_or(0.0, |rate| {
        input_share * rate.input + (1.0 - input_share) * rate.cache_write
    });
    // Cap at what turn N+1 actually paid for new content — otherwise
    // multiple tool_results entering on the same turn could over-attribute
    // past the actual paid total.
    let cap = sibling_total.min(new_content);
    for &i in pending {
        let tokens = (attributions[i].result_tokens as f64 / sibling_total) * cap;
        let cost = (tokens / PER_MILLION) * per_token_price;
        attributions[i].initial_cost = cost;
        attributions[i].initial_tokens = tokens;
        attributions[i].total_cost += cost;
    }
}

/// Allocate `turn`'s cacheRead across the still-cached `riding` results by
/// size, so the sum never exceeds the actual cacheRead tokens. A result
/// drops out once the cacheRead falls below its size.
pub(super) fn pay_persistence(
    attributions: &mut [ToolAttribution],
    riding: &[usize],
    turn: &TurnRecord,
    rate: Option<&EffectiveModelRate>,
) {
    let still_cached: Vec<usize> = riding
        .iter()
        .copied()
        .filter(|&i| {
            let rt = attributions[i].result_tokens;
            rt > 0 && turn.usage.cache_read >= rt
        })
        .collect();
    let active_total: f64 = still_cached
        .iter()
        .map(|&i| attributions[i].result_tokens as f64)
        .sum();
    if active_total <= 0.0 {
        return;
    }
    let allocatable = (turn.usage.cache_read as f64).min(active_total);
    for &i in &still_cached {
        let tokens = (attributions[i].result_tokens as f64 / active_total) * allocatable;
        let cost = rate.map_or(0.0, |rate| (tokens / PER_MILLION) * rate.cache_read);
        attributions[i].persistence_tokens += tokens;
        attributions[i].persistence_cost += cost;
        attributions[i].total_cost += cost;
        attributions[i].riding_turns += 1;
    }
}
