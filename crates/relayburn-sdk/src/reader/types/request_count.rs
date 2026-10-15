//! `TurnRecord::request_count`: the request denominator aggregates divide by.

use std::borrow::Borrow;

use super::TurnRecord;

impl TurnRecord {
    /// Model API requests this record represents: its recorded count, else
    /// one.
    pub fn request_count(&self) -> u64 {
        self.request_count.unwrap_or(1)
    }

    /// Sum of [`Self::request_count`] across `turns`.
    pub fn total_requests<T: Borrow<TurnRecord>>(turns: &[T]) -> u64 {
        turns.iter().map(|t| t.borrow().request_count()).sum()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn turn(request_count: Option<u64>) -> TurnRecord {
        let mut value = json!({
            "v": 1, "source": "codex", "sessionId": "s", "messageId": "m",
            "turnIndex": 0, "ts": "2026-01-01T00:00:00.000Z", "model": "gpt-5.4",
            "usage": {"input": 0, "output": 0, "reasoning": 0, "cacheRead": 0,
                      "cacheCreate5m": 0, "cacheCreate1h": 0},
            "toolCalls": [],
        });
        if let Some(count) = request_count {
            value["requestCount"] = json!(count);
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_record_without_a_count_is_one_request() {
        let one = turn(None);
        assert_eq!(one.request_count, None);
        assert_eq!(one.request_count(), 1);
        assert!(!serde_json::to_string(&one)
            .unwrap()
            .contains("requestCount"));
    }

    #[test]
    fn a_recorded_count_round_trips_including_zero() {
        for count in [0, 1, 7] {
            let t = turn(Some(count));
            assert_eq!(t.request_count(), count);
            let back: TurnRecord =
                serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
            assert_eq!(back.request_count, Some(count));
        }
    }

    #[test]
    fn total_requests_sums_owned_and_borrowed_turns() {
        let owned = vec![turn(Some(4)), turn(Some(0)), turn(None)];
        assert_eq!(TurnRecord::total_requests(&owned), 5);
        let borrowed: Vec<&TurnRecord> = owned.iter().collect();
        assert_eq!(TurnRecord::total_requests(&borrowed[..2]), 4);
        assert_eq!(TurnRecord::total_requests::<TurnRecord>(&[]), 0);
    }
}
