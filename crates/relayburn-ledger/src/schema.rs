//! Ledger line schema. Mirrors `packages/ledger/src/schema.ts`.
//!
//! Records are kept as opaque `serde_json::Value`s until the
//! `relayburn-reader` port (#242) defines strongly-typed `TurnRecord` and
//! friends. The hashing helpers below extract just the fields the index
//! sidecar needs — they MUST stay byte-compatible with the TS hashes so a
//! TS-written `ledger.idx` and a Rust-written one collide on the same lines.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub type Enrichment = BTreeMap<String, String>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageIdRange {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub to_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub to_ts: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct StampSelector {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub range: Option<MessageIdRange>,
}

/// One ledger line is `{v, kind, ...}`. We tag on `kind` the same way TS does.
/// Records are kept as `Value` so we can round-trip lines whose full schema
/// lives in `relayburn-reader`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LineKind {
    Turn { record: Value },
    Stamp(StampPayload),
    Compaction { record: Value },
    Relationship { record: Value },
    ToolResultEvent { record: Value },
    UserTurn { record: Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StampPayload {
    pub ts: String,
    pub selector: StampSelector,
    pub enrichment: Enrichment,
}

/// Top-level ledger line. `v` MUST be `1`; other versions are skipped by
/// readers (forward-compatible with future schema bumps).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerLine {
    pub v: u32,
    #[serde(flatten)]
    pub body: LineKind,
}

impl LedgerLine {
    pub fn turn(record: Value) -> Self {
        Self {
            v: 1,
            body: LineKind::Turn { record },
        }
    }

    pub fn stamp(ts: impl Into<String>, selector: StampSelector, enrichment: Enrichment) -> Self {
        Self {
            v: 1,
            body: LineKind::Stamp(StampPayload {
                ts: ts.into(),
                selector,
                enrichment,
            }),
        }
    }
}

// Convenience newtypes that return the variant payload, mirroring the TS
// `isTurnLine`/`isStampLine` guards.
#[derive(Debug, Clone)]
pub struct TurnLine<'a> {
    pub record: &'a Value,
}

#[derive(Debug, Clone)]
pub struct StampLine<'a> {
    pub ts: &'a str,
    pub selector: &'a StampSelector,
    pub enrichment: &'a Enrichment,
}

#[derive(Debug, Clone)]
pub struct CompactionLine<'a> {
    pub record: &'a Value,
}

#[derive(Debug, Clone)]
pub struct SessionRelationshipLine<'a> {
    pub record: &'a Value,
}

#[derive(Debug, Clone)]
pub struct ToolResultEventLine<'a> {
    pub record: &'a Value,
}

#[derive(Debug, Clone)]
pub struct UserTurnLine<'a> {
    pub record: &'a Value,
}

impl LedgerLine {
    pub fn as_turn(&self) -> Option<TurnLine<'_>> {
        if self.v != 1 {
            return None;
        }
        match &self.body {
            LineKind::Turn { record } => Some(TurnLine { record }),
            _ => None,
        }
    }

    pub fn as_stamp(&self) -> Option<StampLine<'_>> {
        if self.v != 1 {
            return None;
        }
        match &self.body {
            LineKind::Stamp(s) => Some(StampLine {
                ts: &s.ts,
                selector: &s.selector,
                enrichment: &s.enrichment,
            }),
            _ => None,
        }
    }

    pub fn as_compaction(&self) -> Option<CompactionLine<'_>> {
        if self.v != 1 {
            return None;
        }
        match &self.body {
            LineKind::Compaction { record } => Some(CompactionLine { record }),
            _ => None,
        }
    }

    pub fn as_relationship(&self) -> Option<SessionRelationshipLine<'_>> {
        if self.v != 1 {
            return None;
        }
        match &self.body {
            LineKind::Relationship { record } => Some(SessionRelationshipLine { record }),
            _ => None,
        }
    }

    pub fn as_tool_result_event(&self) -> Option<ToolResultEventLine<'_>> {
        if self.v != 1 {
            return None;
        }
        match &self.body {
            LineKind::ToolResultEvent { record } => Some(ToolResultEventLine { record }),
            _ => None,
        }
    }

    pub fn as_user_turn(&self) -> Option<UserTurnLine<'_>> {
        if self.v != 1 {
            return None;
        }
        match &self.body {
            LineKind::UserTurn { record } => Some(UserTurnLine { record }),
            _ => None,
        }
    }
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn opt_s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn sha256_truncated(input: &str) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let digest = h.finalize();
    let hex = hex::encode(digest);
    hex[..16].to_string()
}

/// Stable id for a turn record. Mirrors `turnIdHash` in the TS sidecar.
pub fn turn_id_hash(record: &Value) -> String {
    let key = format!(
        "{}|{}|{}",
        s(record, "source"),
        s(record, "sessionId"),
        s(record, "messageId"),
    );
    sha256_truncated(&key)
}

/// Stable id for a compaction event. Matches `compactionIdHash` (TS).
pub fn compaction_id_hash(record: &Value) -> String {
    let key = format!(
        "{}|{}|{}",
        s(record, "source"),
        s(record, "sessionId"),
        s(record, "ts"),
    );
    sha256_truncated(&key)
}

/// Stable id for a session-relationship record.
///
/// Mirrors `relationshipIdHash` (TS): `(source, sessionId, relationshipType,
/// relatedSessionId, agentId, parentToolUseId)`. Missing fields collapse to
/// the empty string the same way `?? ''` does in the TS code.
pub fn relationship_id_hash(record: &Value) -> String {
    let key = [
        s(record, "source"),
        s(record, "sessionId"),
        s(record, "relationshipType"),
        opt_s(record, "relatedSessionId"),
        opt_s(record, "agentId"),
        opt_s(record, "parentToolUseId"),
    ]
    .join("|");
    sha256_truncated(&key)
}

/// Stable id for a `ToolResultEventRecord`. Matches the TS hash.
pub fn tool_result_event_id_hash(record: &Value) -> String {
    let event_index = record
        .get("eventIndex")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let key = format!(
        "{}|{}|{}|{}",
        s(record, "source"),
        s(record, "sessionId"),
        s(record, "toolUseId"),
        event_index,
    );
    sha256_truncated(&key)
}

/// Stable id for a `UserTurnRecord`. Matches the TS hash.
pub fn user_turn_id_hash(record: &Value) -> String {
    let key = format!(
        "{}|{}|{}",
        s(record, "source"),
        s(record, "sessionId"),
        s(record, "userUuid"),
    );
    sha256_truncated(&key)
}

/// Per-turn content fingerprint. Mirrors `turnContentFingerprint` (TS).
///
/// Composite key:
/// `ts | model | (input+output) | cacheRead | (cacheCreate5m+cacheCreate1h)
///  | firstToolArgsHash[..4]`.
pub fn turn_content_fingerprint(record: &Value) -> String {
    let usage = record.get("usage").cloned().unwrap_or(Value::Null);
    let input = usage.get("input").and_then(Value::as_i64).unwrap_or(0);
    let output = usage.get("output").and_then(Value::as_i64).unwrap_or(0);
    let cache_read = usage.get("cacheRead").and_then(Value::as_i64).unwrap_or(0);
    let cache_5m = usage
        .get("cacheCreate5m")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_1h = usage
        .get("cacheCreate1h")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let first_tool_prefix = record
        .get("toolCalls")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|tc| tc.get("argsHash"))
        .and_then(Value::as_str)
        .map(|h| {
            let take = h.len().min(4);
            h[..take].to_string()
        })
        .unwrap_or_default();

    let composite = format!(
        "{}|{}|{}|{}|{}|{}",
        s(record, "ts"),
        s(record, "model"),
        input + output,
        cache_read,
        cache_5m + cache_1h,
        first_tool_prefix,
    );
    sha256_truncated(&composite)
}

/// Equivalent of `stampMatches` (TS). True iff the stamp's selector touches
/// the turn AND the selector specified at least one of `sessionId`,
/// `messageId`, or `range`.
pub fn stamp_matches(stamp: &StampLine<'_>, turn: &Value) -> bool {
    let sel = stamp.selector;
    if let Some(ref sid) = sel.session_id {
        if sid != s(turn, "sessionId") {
            return false;
        }
    }
    if let Some(ref mid) = sel.message_id {
        if mid != s(turn, "messageId") {
            return false;
        }
    }
    if let Some(ref r) = sel.range {
        if let Some(ref from_ts) = r.from_ts {
            if s(turn, "ts") < from_ts.as_str() {
                return false;
            }
        }
        if let Some(ref to_ts) = r.to_ts {
            if s(turn, "ts") > to_ts.as_str() {
                return false;
            }
        }
    }
    sel.session_id.is_some() || sel.message_id.is_some() || sel.range.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn turn_id_hash_matches_known_inputs() {
        // Keep this in sync with the TS test vector below; both should produce
        // the same 16-char prefix.
        let r = json!({
            "source": "claude",
            "sessionId": "abc",
            "messageId": "msg1",
            "ts": "2026-01-01T00:00:00Z",
            "model": "claude-sonnet-4-6",
        });
        let expected = sha256_truncated("claude|abc|msg1");
        assert_eq!(turn_id_hash(&r), expected);
    }

    #[test]
    fn fingerprint_handles_missing_fields() {
        let r = json!({"ts": "t", "model": "m"});
        // Should not panic, all numeric fields default to 0.
        let h = turn_content_fingerprint(&r);
        assert_eq!(h.len(), 16);
    }

    #[test]
    fn round_trips_turn_line() {
        let line = LedgerLine::turn(json!({"source": "claude", "sessionId": "s", "messageId": "m"}));
        let s = serde_json::to_string(&line).unwrap();
        let back: LedgerLine = serde_json::from_str(&s).unwrap();
        let t = back.as_turn().expect("turn");
        assert_eq!(t.record.get("messageId").unwrap(), "m");
    }

    #[test]
    fn round_trips_stamp_line() {
        let mut enr = Enrichment::new();
        enr.insert("workflowId".into(), "wf-1".into());
        let line = LedgerLine::stamp(
            "2026-05-03T00:00:00Z",
            StampSelector {
                session_id: Some("sess".into()),
                ..Default::default()
            },
            enr,
        );
        let s = serde_json::to_string(&line).unwrap();
        let back: LedgerLine = serde_json::from_str(&s).unwrap();
        let st = back.as_stamp().expect("stamp");
        assert_eq!(st.ts, "2026-05-03T00:00:00Z");
        assert_eq!(st.enrichment.get("workflowId").unwrap(), "wf-1");
    }

    #[test]
    fn stamp_matches_requires_some_selector() {
        let stamp_payload = StampPayload {
            ts: "t".into(),
            selector: StampSelector::default(),
            enrichment: Enrichment::new(),
        };
        let stamp = StampLine {
            ts: &stamp_payload.ts,
            selector: &stamp_payload.selector,
            enrichment: &stamp_payload.enrichment,
        };
        let turn = json!({"sessionId": "x", "messageId": "y", "ts": "t"});
        assert!(!stamp_matches(&stamp, &turn));
    }

    #[test]
    fn stamp_matches_session_filter() {
        let p = StampPayload {
            ts: "t".into(),
            selector: StampSelector {
                session_id: Some("sess-a".into()),
                ..Default::default()
            },
            enrichment: Enrichment::new(),
        };
        let stamp = StampLine {
            ts: &p.ts,
            selector: &p.selector,
            enrichment: &p.enrichment,
        };
        let matching = json!({"sessionId": "sess-a", "messageId": "m", "ts": "t"});
        let other = json!({"sessionId": "sess-b", "messageId": "m", "ts": "t"});
        assert!(stamp_matches(&stamp, &matching));
        assert!(!stamp_matches(&stamp, &other));
    }
}
