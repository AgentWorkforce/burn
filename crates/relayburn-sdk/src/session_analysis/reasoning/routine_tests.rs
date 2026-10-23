//! When deep-effort routine turns are a finding, and the share they report.

use serde_json::json;

use super::super::findings::FindingContext;
use super::*;
use crate::analyze::pricing::load_builtin_pricing;

/// A Codex turn (reasoning is part of its output).
fn turn(index: u64, model: &str, effort: &str, input: u64, output: u64) -> TurnRecord {
    serde_json::from_value(json!({
        "v": 1, "source": "codex", "sessionId": "s1", "messageId": format!("t{index}"),
        "turnIndex": index, "ts": "2026-04-20T00:00:00.000Z", "model": model,
        "usage": {"input": input, "output": output, "reasoning": output / 2, "cacheRead": 0,
                  "cacheCreate5m": 0, "cacheCreate1h": 0},
        "toolCalls": [], "activity": "git",
        "reasoning": {"effort": effort}
    }))
    .unwrap()
}

/// Three deep routine turns of `deep` (input, output) each, plus one
/// low-effort turn of `rest`.
fn session(model: &str, deep: (u64, u64), rest: (u64, u64)) -> Vec<TurnRecord> {
    let mut turns: Vec<TurnRecord> = (0..3)
        .map(|i| turn(i, model, "high", deep.0, deep.1))
        .collect();
    turns.push(turn(3, model, "low", rest.0, rest.1));
    turns
}

fn routine(turns: &[TurnRecord], pricing: &PricingTable) -> Option<Finding> {
    routine_finding(turns, pricing, &FindingContext::new(turns))
}

#[test]
fn an_unpriced_share_is_measured_in_tokens_from_a_quarter_up() {
    let pricing = load_builtin_pricing();
    // 3 x 100 of 1,200 tokens: exactly a quarter.
    let quarter = session("unpriced-house-model", (60, 40), (540, 360));
    let found = routine(&quarter, &pricing).expect("a quarter of the tokens is a finding");
    assert!(
        found.explanation.starts_with(
            "3 of 4 turns ran at high effort on git work and carried 25% of the session's tokens;"
        ),
        "{}",
        found.explanation
    );
    let under = session("unpriced-house-model", (60, 40), (600, 400));
    assert!(routine(&under, &pricing).is_none());
}

#[test]
fn a_priced_share_is_measured_in_cost() {
    let pricing = load_builtin_pricing();
    // Output-heavy deep turns: 13% of the tokens, most of the cost.
    let turns = session("gpt-5.4", (0, 100), (2000, 0));
    let found = routine(&turns, &pricing).expect("a cost share over a quarter");
    assert!(
        found.explanation.contains("% of the session's cost;"),
        "{}",
        found.explanation
    );
    let whole: f64 = turns
        .iter()
        .map(|t| crate::analyze::cost_for_turn(t, &pricing).unwrap().total)
        .sum();
    let part: f64 = turns[..3]
        .iter()
        .map(|t| crate::analyze::cost_for_turn(t, &pricing).unwrap().total)
        .sum();
    let share = format!("carried {:.0}% of the session's cost", part / whole * 100.0);
    assert!(found.explanation.contains(&share), "{}", found.explanation);
}

#[test]
fn a_free_model_falls_back_to_the_token_share() {
    let mut pricing = load_builtin_pricing();
    let mut free = pricing.get("gpt-5.4").unwrap().clone();
    free.input = 0.0;
    free.output = 0.0;
    free.cache_read = 0.0;
    free.cache_write = 0.0;
    free.cache_write_1h = 0.0;
    free.reasoning = None;
    free.context_tiers.clear();
    pricing.insert("free-model".to_string(), free);
    let turns = session("free-model", (0, 100), (2000, 0));
    assert!(
        routine(&turns, &pricing).is_none(),
        "13% of the tokens of a session that cost nothing is no finding"
    );
}
