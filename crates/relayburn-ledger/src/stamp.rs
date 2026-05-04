//! Stamp records — first-party metadata that survives `burn state rebuild`.
//!
//! A stamp annotates one or more turns with key/value enrichment data
//! (`{"role": "fix-bug"}`, `{"parentAgentId": "..."}`, …). Stamps are
//! written by the `burn stamp` CLI verb and by harness wrappers that
//! observe spawn-env signals; the analytics layer folds them onto turns
//! at query time so existing turn rows don't need to be rewritten.
//!
//! Selectors target either a specific session, a specific message, or a
//! ts/messageId range. Empty selectors are rejected by [`Stamp::new`] —
//! a stamp with no targeting clause would silently apply to every turn,
//! which is never what callers want.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use relayburn_reader::TurnRecord;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ts: Option<String>,
}

impl MessageRange {
    fn is_empty(&self) -> bool {
        self.from_message_id.is_none()
            && self.to_message_id.is_none()
            && self.from_ts.is_none()
            && self.to_ts.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StampSelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<MessageRange>,
}

impl StampSelector {
    pub fn is_empty(&self) -> bool {
        self.session_id.is_none()
            && self.message_id.is_none()
            && self.range.as_ref().map(|r| r.is_empty()).unwrap_or(true)
    }
}

pub type Enrichment = BTreeMap<String, String>;

/// A selector + enrichment payload. `ts` is the wall-clock timestamp the
/// caller observed; `written_at` is set by the writer at append time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stamp {
    pub ts: String,
    pub selector: StampSelector,
    pub enrichment: Enrichment,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StampError {
    #[error("stamp selector must target sessionId, messageId, or a range")]
    EmptySelector,
}

impl Stamp {
    /// Build a stamp, rejecting empty selectors. An empty selector would
    /// match every turn — we'd rather fail loud than silently apply a
    /// label across the whole ledger.
    pub fn new(
        ts: impl Into<String>,
        selector: StampSelector,
        enrichment: Enrichment,
    ) -> Result<Self, StampError> {
        if selector.is_empty() {
            return Err(StampError::EmptySelector);
        }
        Ok(Self {
            ts: ts.into(),
            selector,
            enrichment,
        })
    }
}

/// True iff `stamp` should fold its enrichment onto `turn`.
///
/// Mirrors the TS `stampMatches` so cross-tree readers fold stamps
/// identically. Range bounds are inclusive on `ts` (string compare —
/// works because all our timestamps are ISO-8601 with fixed width).
pub fn stamp_matches(stamp: &Stamp, turn: &TurnRecord) -> bool {
    let s = &stamp.selector;
    if let Some(ref sid) = s.session_id {
        if sid != &turn.session_id {
            return false;
        }
    }
    if let Some(ref mid) = s.message_id {
        if mid != &turn.message_id {
            return false;
        }
    }
    if let Some(ref range) = s.range {
        if let Some(ref from) = range.from_ts {
            if &turn.ts < from {
                return false;
            }
        }
        if let Some(ref to) = range.to_ts {
            if &turn.ts > to {
                return false;
            }
        }
    }
    !s.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use relayburn_reader::{SourceKind, Usage};

    fn make_turn(session: &str, message: &str, ts: &str) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session.into(),
            session_path: None,
            message_id: message.into(),
            turn_index: 0,
            ts: ts.into(),
            model: "m".into(),
            project: None,
            project_key: None,
            usage: Usage::default(),
            tool_calls: vec![],
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    #[test]
    fn empty_selector_rejected() {
        let err = Stamp::new(
            "2025-01-01T00:00:00Z".to_string(),
            StampSelector::default(),
            Enrichment::new(),
        )
        .unwrap_err();
        assert_eq!(err, StampError::EmptySelector);
    }

    #[test]
    fn session_selector_matches_only_that_session() {
        let s = Stamp::new(
            "2025-01-01T00:00:00Z",
            StampSelector {
                session_id: Some("a".into()),
                ..Default::default()
            },
            Enrichment::new(),
        )
        .unwrap();
        assert!(stamp_matches(&s, &make_turn("a", "m1", "2025-01-01T00:00:00Z")));
        assert!(!stamp_matches(&s, &make_turn("b", "m1", "2025-01-01T00:00:00Z")));
    }

    #[test]
    fn range_bounds_are_inclusive() {
        let s = Stamp::new(
            "2025-01-01T00:00:00Z",
            StampSelector {
                session_id: Some("a".into()),
                range: Some(MessageRange {
                    from_ts: Some("2025-01-01T00:00:00Z".into()),
                    to_ts: Some("2025-01-02T00:00:00Z".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Enrichment::new(),
        )
        .unwrap();
        assert!(stamp_matches(&s, &make_turn("a", "m", "2025-01-01T00:00:00Z")));
        assert!(stamp_matches(&s, &make_turn("a", "m", "2025-01-02T00:00:00Z")));
        assert!(!stamp_matches(
            &s,
            &make_turn("a", "m", "2024-12-31T23:59:59Z")
        ));
        assert!(!stamp_matches(
            &s,
            &make_turn("a", "m", "2025-01-02T00:00:01Z")
        ));
    }
}
