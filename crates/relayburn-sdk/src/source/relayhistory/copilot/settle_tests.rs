//! A shutdown interval settles so that every output token sits on exactly
//! one turn, under a model the shutdown names.

use std::collections::HashMap;

use super::{leave_unnamed_output_to_aggregates, settle};
use crate::reader::types::{TurnRecord, Usage};

fn message(model: &str, output: u64) -> TurnRecord {
    let mut turn: TurnRecord = serde_json::from_value(serde_json::json!({
        "v": 1, "source": "copilot-cli", "sessionId": "s", "messageId": "m",
        "turnIndex": 0, "ts": "2026-09-18T21:30:00.000Z", "model": model,
        "usage": {"input": 0, "output": 0, "reasoning": 0, "cacheRead": 0,
                  "cacheCreate5m": 0, "cacheCreate1h": 0},
        "toolCalls": [],
    }))
    .unwrap();
    turn.usage.output = output;
    turn
}

fn grew(output: u64) -> Usage {
    Usage {
        input: 1,
        output,
        ..Usage::default()
    }
}

/// Output billed per model across the message turns and the aggregates.
fn billed(messages: &[TurnRecord], aggregates: &[(String, Usage)]) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    for turn in messages {
        *out.entry(turn.model.clone()).or_default() += turn.usage.output;
    }
    for (model, usage) in aggregates {
        *out.entry(model.clone()).or_default() += usage.output;
    }
    out.retain(|_, n| *n > 0);
    out
}

/// The interval's message turns as ingest bills them, and its aggregates.
fn settled(
    mut messages: Vec<TurnRecord>,
    growths: &[(&str, u64)],
) -> (Vec<TurnRecord>, Vec<(String, Usage)>) {
    messages
        .iter_mut()
        .for_each(leave_unnamed_output_to_aggregates);
    let growths = growths
        .iter()
        .map(|(model, output)| (model.to_string(), grew(*output)))
        .collect();
    let refs: Vec<&TurnRecord> = messages.iter().collect();
    let aggregates = settle(&refs, growths);
    (messages, aggregates)
}

/// Two models with 6 and 4 output tokens left, and unnamed messages of 4
/// then 6: neither message can be placed by size alone, so the aggregates
/// carry all 10 and the messages bill none.
#[test]
fn unnamed_messages_under_several_models_leave_their_output_to_the_aggregates() {
    let (messages, aggregates) = settled(
        vec![message("", 4), message("", 6)],
        &[("gpt-5.4", 6), ("kimi-k3", 4)],
    );
    let want = HashMap::from([("gpt-5.4".to_string(), 6), ("kimi-k3".to_string(), 4)]);
    assert_eq!(billed(&messages, &aggregates), want);
    assert!(messages.iter().all(|t| t.usage.output == 0));
}

/// Under one model too, an unnamed message bills nothing and that model's
/// aggregate carries its output.
#[test]
fn an_unnamed_message_under_one_model_leaves_its_output_to_that_models_aggregate() {
    let (messages, aggregates) = settled(vec![message("", 4)], &[("gpt-5.4", 10)]);
    assert_eq!(
        (messages[0].model.as_str(), messages[0].usage.output),
        ("", 0)
    );
    assert_eq!(aggregates, [("gpt-5.4".to_string(), grew(10))]);
}

/// Before any shutdown names a model, an unnamed message still bills
/// nothing: it bills the same once a shutdown lands.
#[test]
fn an_unnamed_message_bills_nothing_before_a_shutdown() {
    let (messages, aggregates) = settled(vec![message("", 4)], &[]);
    assert_eq!(messages[0].usage.output, 0);
    assert!(aggregates.is_empty());
}

/// xorshift: a fixed, dependency-free sequence of cases.
struct Cases(u64);

impl Cases {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// For any interval whose message turns bill no more than the shutdown's
/// growth: per model, billed output equals the growth; no output is left
/// on a turn without a model; no aggregate exceeds its growth.
#[test]
fn every_interval_bills_each_models_growth_exactly_once() {
    let models = ["claude-sonnet-4.6", "gpt-5.4", "kimi-k3"];
    let mut cases = Cases(0x9e37_79b9_7f4a_7c15);
    for _ in 0..2_000 {
        let count = 1 + cases.below(models.len() as u64) as usize;
        let named = &models[..count];
        let mut left: Vec<u64> = named.iter().map(|_| cases.below(500)).collect();
        let growth: Vec<(&str, u64)> = named.iter().copied().zip(left.clone()).collect();
        let mut messages = Vec::new();
        for _ in 0..cases.below(8) {
            let pick = cases.below(count as u64) as usize;
            let output = cases.below(left[pick] + 1);
            left[pick] -= output;
            let model = if cases.below(3) == 0 { "" } else { named[pick] };
            messages.push(message(model, output));
        }
        let (messages, aggregates) = settled(messages, &growth);
        let want: HashMap<String, u64> = growth
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(m, n)| (m.to_string(), *n))
            .collect();
        assert_eq!(billed(&messages, &aggregates), want, "{growth:?}");
        assert!(messages
            .iter()
            .all(|t| !t.model.is_empty() || t.usage.output == 0));
        for (model, usage) in &aggregates {
            let grown = growth.iter().find(|(m, _)| m == model).unwrap().1;
            assert!(usage.output <= grown, "{model}");
        }
    }
}
