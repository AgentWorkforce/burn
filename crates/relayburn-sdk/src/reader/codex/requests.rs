//! Codex request-level accounting.
//!
//! Codex reports usage as cumulative `token_count` snapshots, and one logical
//! task (`task_started` .. `task_complete`) spans many model requests. Every
//! snapshot whose totals advance past the prior snapshot marks one request;
//! [`RequestTally`] turns each advance into a native [`Inference`] row keyed
//! by a stable per-task request index.

use serde_json::Value;

use crate::reader::inference::{Inference, InferenceKeySource, InferenceKind, ToolUseRef};
use crate::reader::types::{SourceKind, ToolCall, Usage};
use crate::util::time::parse_iso_ms;

use super::{CumulativeUsage, OpenTurn};

impl CumulativeUsage {
    /// Reads a Codex `info.total_token_usage` object. Codex counts cached
    /// input inside `input_tokens`, so fresh input is the difference.
    pub(in crate::reader::codex) fn from_total_token_usage(total: &Value) -> Self {
        let field = |name: &str| total.get(name).and_then(Value::as_i64).unwrap_or(0);
        let cached = field("cached_input_tokens");
        Self {
            input: field("input_tokens") - cached,
            output: field("output_tokens"),
            cache_read: cached,
            reasoning: field("reasoning_output_tokens"),
        }
    }

    fn advanced_from(&self, prior: &Self) -> bool {
        self.input > prior.input
            || self.output > prior.output
            || self.cache_read > prior.cache_read
            || self.reasoning > prior.reasoning
    }

    /// Per-request usage between two snapshots; a counter that moved
    /// backwards contributes zero rather than wrapping.
    fn delta_since(&self, prior: &Self) -> Usage {
        Usage {
            input: (self.input - prior.input).max(0) as u64,
            output: (self.output - prior.output).max(0) as u64,
            reasoning: (self.reasoning - prior.reasoning).max(0) as u64,
            cache_read: (self.cache_read - prior.cache_read).max(0) as u64,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }
}

impl OpenTurn {
    /// Folds a new cumulative usage snapshot into this open task.
    pub(in crate::reader::codex) fn observe_usage(
        &mut self,
        session_id: &str,
        prior: &CumulativeUsage,
        current: &CumulativeUsage,
        ts: &str,
    ) {
        self.usage_observed = true;
        let turn = RequestTurn {
            session_id,
            turn_id: &self.turn_id,
            model: &self.model,
        };
        self.requests
            .observe(&turn, &self.tool_calls, prior, current, ts);
    }
}

/// Logical-turn identity shared by every request row of one Codex task.
struct RequestTurn<'a> {
    session_id: &'a str,
    turn_id: &'a str,
    model: &'a str,
}

/// Requests observed so far inside one open Codex task.
#[derive(Debug, Clone, Default)]
pub(in crate::reader::codex) struct RequestTally {
    pub(in crate::reader::codex) count: u64,
    pub(in crate::reader::codex) inferences: Vec<Inference>,
    /// Tool calls before this index belong to already-recorded requests.
    tool_call_index: usize,
}

impl RequestTally {
    /// Records one request when `current` advances past `prior`. The request
    /// claims every tool call issued since the previous recorded request.
    fn observe(
        &mut self,
        turn: &RequestTurn<'_>,
        tool_calls: &[ToolCall],
        prior: &CumulativeUsage,
        current: &CumulativeUsage,
        ts: &str,
    ) {
        if !current.advanced_from(prior) {
            return;
        }
        let tool_uses = tool_calls[self.tool_call_index..]
            .iter()
            .map(|call| ToolUseRef {
                id: call.id.clone(),
                name: call.name.clone(),
            })
            .collect::<Vec<_>>();
        self.tool_call_index = tool_calls.len();
        let usage = current.delta_since(prior);
        let kind = request_kind(&usage, !tool_uses.is_empty());
        self.count += 1;
        let ts_ms = parse_iso_ms(ts).unwrap_or(0);
        self.inferences.push(Inference {
            v: 1,
            source: SourceKind::Codex,
            session_id: turn.session_id.to_string(),
            request_id: format!("{}:request:{:06}", turn.turn_id, self.count),
            request_id_source: InferenceKeySource::RowSynthetic,
            turn_id: turn.turn_id.to_string(),
            model: turn.model.to_string(),
            usage,
            kind,
            tool_uses,
            start_ts: ts.to_string(),
            end_ts: ts.to_string(),
            start_ms: ts_ms,
            end_ms: ts_ms,
        });
    }
}

fn request_kind(usage: &Usage, has_tool_uses: bool) -> InferenceKind {
    if !has_tool_uses {
        if usage.reasoning > 0 && usage.output == 0 {
            InferenceKind::Reasoning
        } else {
            InferenceKind::Message
        }
    } else if usage.reasoning > 0 {
        InferenceKind::Mixed
    } else {
        InferenceKind::ToolUse
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn snapshot(input: i64, output: i64, cache_read: i64, reasoning: i64) -> CumulativeUsage {
        CumulativeUsage {
            input,
            output,
            cache_read,
            reasoning,
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: format!("tool_{id}"),
            target: None,
            args_hash: String::new(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    const TURN: RequestTurn<'static> = RequestTurn {
        session_id: "sess",
        turn_id: "turn",
        model: "gpt-5.4",
    };

    #[test]
    fn total_token_usage_splits_cached_input() {
        let total = json!({
            "input_tokens": 100,
            "cached_input_tokens": 30,
            "output_tokens": 7,
            "reasoning_output_tokens": 3,
        });
        assert_eq!(
            CumulativeUsage::from_total_token_usage(&total),
            snapshot(70, 7, 30, 3)
        );
        assert_eq!(
            CumulativeUsage::from_total_token_usage(&json!({})),
            CumulativeUsage::default()
        );
    }

    #[test]
    fn any_single_counter_advancing_counts_as_progress() {
        let prior = snapshot(10, 10, 10, 10);
        assert!(!prior.advanced_from(&prior));
        assert!(snapshot(11, 10, 10, 10).advanced_from(&prior));
        assert!(snapshot(10, 11, 10, 10).advanced_from(&prior));
        assert!(snapshot(10, 10, 11, 10).advanced_from(&prior));
        assert!(snapshot(10, 10, 10, 11).advanced_from(&prior));
        assert!(!snapshot(9, 9, 9, 9).advanced_from(&prior));
    }

    #[test]
    fn delta_clamps_regressed_counters_to_zero() {
        let usage = snapshot(15, 8, 3, 9).delta_since(&snapshot(10, 10, 1, 4));
        assert_eq!(
            usage,
            Usage {
                input: 5,
                output: 0,
                reasoning: 5,
                cache_read: 2,
                cache_create_5m: 0,
                cache_create_1h: 0,
            }
        );
    }

    #[test]
    fn request_kind_covers_each_shape() {
        let with = |output, reasoning| Usage {
            output,
            reasoning,
            ..Usage::default()
        };
        assert_eq!(request_kind(&with(0, 2), false), InferenceKind::Reasoning);
        assert_eq!(request_kind(&with(1, 2), false), InferenceKind::Message);
        assert_eq!(request_kind(&with(0, 0), false), InferenceKind::Message);
        assert_eq!(request_kind(&with(1, 2), true), InferenceKind::Mixed);
        assert_eq!(request_kind(&with(1, 0), true), InferenceKind::ToolUse);
    }

    #[test]
    fn stalled_snapshot_records_nothing() {
        let mut tally = RequestTally::default();
        let same = snapshot(5, 5, 5, 5);
        tally.observe(&TURN, &[call("a")], &same, &same, "2026-01-01T00:00:00Z");
        assert_eq!(tally.count, 0);
        assert!(tally.inferences.is_empty());
        assert_eq!(tally.tool_call_index, 0);
    }

    #[test]
    fn each_advance_records_a_keyed_request_with_new_tool_calls() {
        let mut tally = RequestTally::default();
        let calls = [call("a"), call("b")];
        let s0 = CumulativeUsage::default();
        let s1 = snapshot(10, 2, 4, 0);
        let s2 = snapshot(16, 5, 4, 1);
        tally.observe(&TURN, &calls[..1], &s0, &s1, "2026-01-01T00:00:01Z");
        tally.observe(&TURN, &calls, &s1, &s2, "2026-01-01T00:00:02.500Z");

        assert_eq!(tally.count, 2);
        assert_eq!(tally.tool_call_index, 2);
        let [first, second] = tally.inferences.as_slice() else {
            panic!("expected two inferences, got {:?}", tally.inferences);
        };
        assert_eq!(first.request_id, "turn:request:000001");
        assert_eq!(second.request_id, "turn:request:000002");
        assert_eq!(first.request_id_source, InferenceKeySource::RowSynthetic);
        assert_eq!(first.source, SourceKind::Codex);
        assert_eq!(first.v, 1);
        assert_eq!(first.session_id, "sess");
        assert_eq!(first.turn_id, "turn");
        assert_eq!(first.model, "gpt-5.4");
        assert_eq!(first.usage, s1.delta_since(&s0));
        assert_eq!(second.usage, s2.delta_since(&s1));
        assert_eq!(first.kind, InferenceKind::ToolUse);
        assert_eq!(second.kind, InferenceKind::Mixed);
        let ids = |inf: &Inference| {
            inf.tool_uses
                .iter()
                .map(|t| t.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(first), ["a"]);
        assert_eq!(ids(second), ["b"]);
        assert_eq!(second.tool_uses[0].name, "tool_b");
        assert_eq!(second.start_ts, "2026-01-01T00:00:02.500Z");
        assert_eq!(second.end_ts, "2026-01-01T00:00:02.500Z");
        assert_eq!(second.start_ms, 1_767_225_602_500);
        assert_eq!(second.end_ms, 1_767_225_602_500);
    }

    #[test]
    fn unparseable_timestamp_falls_back_to_zero_ms() {
        let mut tally = RequestTally::default();
        let advanced = snapshot(1, 0, 0, 0);
        tally.observe(&TURN, &[], &CumulativeUsage::default(), &advanced, "bad");
        assert_eq!(tally.inferences[0].start_ms, 0);
        assert_eq!(tally.inferences[0].end_ms, 0);
        assert_eq!(tally.inferences[0].kind, InferenceKind::Message);
    }
}
