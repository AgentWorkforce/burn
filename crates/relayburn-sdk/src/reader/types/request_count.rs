//! `TurnRecord::request_count` semantics: the serde default for rows that
//! predate the field, and the request denominator used by aggregates.

use std::borrow::Borrow;

use super::TurnRecord;

pub(super) const fn default_request_count() -> u64 {
    1
}

impl TurnRecord {
    /// Request denominator for aggregate metrics. Historical rows that omit
    /// the field deserialize as one; an explicit zero remains zero for a
    /// completed task that made no model request.
    pub fn effective_request_count(&self) -> u64 {
        self.request_count
    }

    /// Sum of [`Self::effective_request_count`] across `turns`.
    pub(crate) fn total_requests<T: Borrow<TurnRecord>>(turns: &[T]) -> u64 {
        turns
            .iter()
            .map(|t| t.borrow().effective_request_count())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::types::{SourceKind, Usage};

    #[test]
    fn historical_turn_defaults_to_one_request() {
        let mut value = serde_json::to_value(TurnRecord {
            v: 1,
            source: SourceKind::Codex,
            session_id: "s".into(),
            session_path: None,
            message_id: "m".into(),
            turn_index: 0,
            request_count: 1,
            ts: "2026-01-01T00:00:00.000Z".into(),
            model: "gpt-5.4".into(),
            project: None,
            project_key: None,
            usage: Usage::default(),
            tool_calls: Vec::new(),
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        })
        .unwrap();
        value.as_object_mut().unwrap().remove("requestCount");

        let historical: TurnRecord = serde_json::from_value(value).unwrap();
        assert_eq!(historical.request_count, 1);
        assert_eq!(historical.effective_request_count(), 1);
    }

    #[test]
    fn total_requests_sums_owned_and_borrowed_turns() {
        let turn = |request_count: u64| -> TurnRecord {
            serde_json::from_value(serde_json::json!({
                "v": 1,
                "source": "codex",
                "sessionId": "s",
                "messageId": "m",
                "turnIndex": 0,
                "requestCount": request_count,
                "ts": "2026-01-01T00:00:00.000Z",
                "model": "gpt-5.4",
                "usage": Usage::default(),
                "toolCalls": [],
            }))
            .unwrap()
        };
        let owned = vec![turn(4), turn(0), turn(3)];
        assert_eq!(TurnRecord::total_requests(&owned), 7);
        let borrowed: Vec<&TurnRecord> = owned.iter().collect();
        assert_eq!(TurnRecord::total_requests(&borrowed[..2]), 4);
        assert_eq!(TurnRecord::total_requests::<TurnRecord>(&[]), 0);
    }
}
