//! Burn's accounting of Codex `token_count` snapshots.
//!
//! Codex reports usage as a running total: every `token_count` event's
//! `info.total_token_usage` is the session's cumulative spend so far, in
//! OpenAI Responses terms (`input_tokens` includes `cached_input_tokens`,
//! `output_tokens` includes reasoning). A turn's spend is the difference
//! between the running total when it closes and when it opened.
//!
//! A counter that is present but not a non-negative integer is malformed,
//! and a running total that goes down is a regression. Neither is a zero or
//! negative spend: both are reported as [`CounterError`] so the turn can
//! say its usage is unknown.

use ai_hist::{TokenUsage, UsageSnapshot};
use serde_json::Value;

use crate::reader::types::Usage;

/// One cumulative snapshot, as the provider counted it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Counters {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning: u64,
}

/// Why a snapshot cannot be accounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CounterError {
    /// The counter is present but not a non-negative integer.
    Malformed { field: &'static str },
    /// A running total is lower than an earlier one.
    Regressed {
        field: &'static str,
        from: u64,
        to: u64,
    },
}

impl Counters {
    /// Input tokens not served from cache.
    fn uncached_input(&self) -> Result<u64, CounterError> {
        self.input
            .checked_sub(self.cached_input)
            .ok_or(CounterError::Malformed {
                field: "cached_input_tokens",
            })
    }

    /// The running totals as `(name, value)`, uncached input first.
    fn totals(&self) -> Result<[(&'static str, u64); 4], CounterError> {
        Ok([
            ("input_tokens", self.uncached_input()?),
            ("cached_input_tokens", self.cached_input),
            ("output_tokens", self.output),
            ("reasoning_output_tokens", self.reasoning),
        ])
    }
}

/// The cumulative counters in one `token_count` snapshot. `None` when the
/// snapshot carries no running total. A counter Codex leaves out is zero.
pub(super) fn read_snapshot(snapshot: &UsageSnapshot) -> Option<Result<Counters, CounterError>> {
    match &snapshot.total_token_usage {
        Some(total) => Some(counters(total)),
        // A total written as something other than an object counts nothing.
        None => snapshot
            .other
            .contains_key("total_token_usage")
            .then(|| Ok(Counters::default())),
    }
}

fn counters(total: &TokenUsage) -> Result<Counters, CounterError> {
    let field = |name: &'static str, value: &Option<Value>| match value {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or(CounterError::Malformed { field: name }),
    };
    Ok(Counters {
        input: field("input_tokens", &total.input_tokens)?,
        cached_input: field("cached_input_tokens", &total.cached_input_tokens)?,
        output: field("output_tokens", &total.output_tokens)?,
        reasoning: field("reasoning_output_tokens", &total.reasoning_output_tokens)?,
    })
}

/// The running total a fork child continues from: the replayed parent
/// snapshot on its `fork_replay_boundary`, when relayhistory found the
/// child's own counter `applied` it as the baseline.
pub(super) fn inherited_total(boundary: &Value) -> Option<Counters> {
    if boundary.get("inherited_baseline")?.as_str()? != "applied" {
        return None;
    }
    let info = boundary.get("inherited_snapshot")?.clone();
    read_snapshot(&serde_json::from_value(info).ok()?)?.ok()
}

/// What was spent between two running totals.
pub(super) fn spend(start: &Counters, end: &Counters) -> Result<Usage, CounterError> {
    let (before, after) = (start.totals()?, end.totals()?);
    let mut delta = [0u64; 4];
    for (slot, ((field, from), (_, to))) in delta.iter_mut().zip(before.into_iter().zip(after)) {
        *slot = to
            .checked_sub(from)
            .ok_or(CounterError::Regressed { field, from, to })?;
    }
    let [input, cache_read, output, reasoning] = delta;
    Ok(Usage {
        input,
        output,
        reasoning,
        cache_read,
        cache_create_5m: 0,
        cache_create_1h: 0,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// `read_snapshot` over a provider `info` object.
    fn read(info: Value) -> Option<Result<Counters, CounterError>> {
        read_snapshot(&serde_json::from_value::<UsageSnapshot>(info).unwrap())
    }

    fn snapshot(input: u64, cached: u64, output: u64, reasoning: u64) -> Counters {
        Counters {
            input,
            cached_input: cached,
            output,
            reasoning,
        }
    }

    #[test]
    fn reads_running_total() {
        let info = json!({"total_token_usage": {
            "input_tokens": 1000, "cached_input_tokens": 400,
            "output_tokens": 120, "reasoning_output_tokens": 30, "total_tokens": 1120
        }});
        assert_eq!(read(info), Some(Ok(snapshot(1000, 400, 120, 30))));
    }

    #[test]
    fn absent_counters_are_zero() {
        let info = json!({"total_token_usage": {"input_tokens": 100, "output_tokens": 50}});
        assert_eq!(read(info), Some(Ok(snapshot(100, 0, 50, 0))));
    }

    #[test]
    fn null_info_is_no_snapshot() {
        assert_eq!(read(json!({"last_token_usage": {}})), None);
    }

    #[test]
    fn a_non_object_total_counts_nothing() {
        assert_eq!(
            read(json!({"total_token_usage": "n/a"})),
            Some(Ok(Counters::default()))
        );
    }

    #[test]
    fn malformed_counters_are_errors() {
        for bad in [
            json!(-5),
            json!(1.5),
            json!("12"),
            json!(u64::MAX as f64 * 2.0),
        ] {
            let info = json!({"total_token_usage": {"input_tokens": 10, "output_tokens": bad}});
            assert_eq!(
                read(info),
                Some(Err(CounterError::Malformed {
                    field: "output_tokens"
                }))
            );
        }
    }

    #[test]
    fn an_applied_fork_baseline_is_the_inherited_total() {
        let info = json!({"total_token_usage": {"input_tokens": 1000, "output_tokens": 50}});
        let boundary =
            |verdict: Value| json!({"inherited_snapshot": info, "inherited_baseline": verdict});
        assert_eq!(
            inherited_total(&boundary(json!("applied"))),
            Some(snapshot(1000, 0, 50, 0))
        );
        for verdict in [json!("dropped"), json!("pending"), Value::Null] {
            assert_eq!(inherited_total(&boundary(verdict)), None);
        }
        assert_eq!(
            inherited_total(&json!({"inherited_baseline": "applied"})),
            None
        );
    }

    #[test]
    fn spend_differences_running_totals() {
        let usage = spend(
            &snapshot(3000, 1000, 200, 50),
            &snapshot(8000, 3500, 700, 100),
        )
        .unwrap();
        assert_eq!(
            (usage.input, usage.cache_read, usage.output, usage.reasoning),
            (2500, 2500, 500, 50)
        );
        assert_eq!((usage.cache_create_5m, usage.cache_create_1h), (0, 0));
    }

    #[test]
    fn spend_from_session_start() {
        let usage = spend(&Counters::default(), &snapshot(1000, 400, 120, 30)).unwrap();
        assert_eq!(
            (usage.input, usage.cache_read, usage.output, usage.reasoning),
            (600, 400, 120, 30)
        );
    }

    #[test]
    fn regressed_total_is_an_error_not_a_negative_spend() {
        assert_eq!(
            spend(&snapshot(500, 0, 90, 0), &snapshot(500, 0, 80, 0)),
            Err(CounterError::Regressed {
                field: "output_tokens",
                from: 90,
                to: 80
            })
        );
        // Cache growing faster than input shrinks the uncached total.
        assert_eq!(
            spend(&snapshot(100, 0, 0, 0), &snapshot(150, 80, 0, 0)),
            Err(CounterError::Regressed {
                field: "input_tokens",
                from: 100,
                to: 70
            })
        );
    }

    #[test]
    fn cache_above_input_is_malformed() {
        assert_eq!(
            spend(&Counters::default(), &snapshot(10, 20, 0, 0)),
            Err(CounterError::Malformed {
                field: "cached_input_tokens"
            })
        );
    }
}
