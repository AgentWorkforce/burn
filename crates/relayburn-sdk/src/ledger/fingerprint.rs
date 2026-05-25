//! Stable fingerprints for ledger records.
//!
//! Two layers of dedup, both shipped from day one against the events DB:
//!
//! 1. **Primary id fingerprint** (`*_id_fingerprint`) — the natural primary
//!    key of a record (`(source, sessionId, messageId)` for turns,
//!    `(source, sessionId, toolUseId, eventIndex)` for tool result events,
//!    etc.). Captured as the row's PRIMARY KEY so a re-ingest of the same
//!    upstream bytes is a UNIQUE-collision no-op.
//! 2. **Content fingerprint** (`turn_content_fingerprint`) — a hash over
//!    the externally-visible shape of a turn (timestamp, model, token
//!    counts, first-tool-args prefix). Catches "same logical turn under
//!    a different messageId" — re-emitted forks, parser bugfixes that
//!    change messageId derivation, the same turn observed under a
//!    different `source` label, etc. Lives in a regular indexed column
//!    on `turns`; without this, every such case double-counts in
//!    `burn summary`.
//!
//! Hashes are 16 hex chars (the leading 64 bits of SHA-256). The size
//! is shared with the TS implementation so inter-tree cross-checks stay
//! easy; the bound is conservative — birthday collisions at 2^32 turns
//! are still under one in a billion.

use crate::reader::{
    CompactionEvent, SessionRelationshipRecord, ToolResultEventRecord, TurnRecord, UserTurnRecord,
};
use sha2::{Digest, Sha256};

const FINGERPRINT_LEN: usize = 16;

fn short_sha256(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let hex = hex::encode(digest);
    hex[..FINGERPRINT_LEN].to_string()
}

/// `sha256("source|sessionId|messageId")[..16]`.
#[cfg(test)]
pub fn turn_id_fingerprint(t: &TurnRecord) -> String {
    short_sha256(&format!(
        "{}|{}|{}",
        t.source.wire_str(),
        t.session_id,
        t.message_id
    ))
}

/// `sha256("source|sessionId|ts")[..16]` — compactions are unique per
/// session/timestamp.
pub fn compaction_id_fingerprint(e: &CompactionEvent) -> String {
    short_sha256(&format!(
        "{}|{}|{}",
        e.source.wire_str(),
        e.session_id,
        e.ts
    ))
}

/// `sha256("source|sessionId|relationshipType|relatedSessionId|agentId|parentToolUseId")[..16]`.
pub fn relationship_id_fingerprint(r: &SessionRelationshipRecord) -> String {
    let parts = [
        r.source.wire_str(),
        r.session_id.as_str(),
        r.relationship_type.wire_str(),
        r.related_session_id.as_deref().unwrap_or(""),
        r.agent_id.as_deref().unwrap_or(""),
        r.parent_tool_use_id.as_deref().unwrap_or(""),
    ];
    short_sha256(&parts.join("|"))
}

/// `sha256("source|sessionId|toolUseId|eventIndex")[..16]`.
pub fn tool_result_event_id_fingerprint(r: &ToolResultEventRecord) -> String {
    short_sha256(&format!(
        "{}|{}|{}|{}",
        r.source.wire_str(),
        r.session_id,
        r.tool_use_id,
        r.event_index
    ))
}

/// `sha256("source|sessionId|userUuid")[..16]`.
pub fn user_turn_id_fingerprint(r: &UserTurnRecord) -> String {
    short_sha256(&format!(
        "{}|{}|{}",
        r.source.wire_str(),
        r.session_id,
        r.user_uuid
    ))
}

/// Layer-2 dedup hash for a turn. Includes the externally-visible cost
/// shape (timestamp, model, total tokens, cache split) plus a 4-char
/// prefix of the first tool call's args hash. Two turns under different
/// messageIds whose shape exactly matches collapse to one row.
pub fn turn_content_fingerprint(t: &TurnRecord) -> String {
    let first_tool_prefix = match t.tool_calls.first() {
        Some(call) if !call.args_hash.is_empty() => {
            let n = call.args_hash.len().min(4);
            call.args_hash[..n].to_string()
        }
        _ => String::new(),
    };
    let composite = format!(
        "{}|{}|{}|{}|{}|{}",
        t.ts,
        t.model,
        t.usage.input + t.usage.output,
        t.usage.cache_read,
        t.usage.cache_create_5m + t.usage.cache_create_1h,
        first_tool_prefix,
    );
    short_sha256(&composite)
}

/// Hash a content blob's body so the content store can dedup
/// byte-equivalent records across re-ingest passes without storing the
/// payload twice.
pub fn content_blob_fingerprint(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    hex::encode(digest)[..FINGERPRINT_LEN].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{SourceKind, ToolCall, Usage};

    fn turn(message_id: &str, ts: &str) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "sess1".into(),
            session_path: None,
            message_id: message_id.into(),
            turn_index: 0,
            ts: ts.into(),
            model: "claude-sonnet-4-6".into(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 10,
                output: 5,
                reasoning: 0,
                cache_read: 100,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                target: None,
                args_hash: "abcdef".into(),
                is_error: None,
                edit_pre_hash: None,
                edit_post_hash: None,
                skill_name: None,
                replaced_tools: None,
                collapsed_calls: None,
            }],
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
    fn turn_id_fingerprint_is_stable_per_message_id() {
        let a = turn("m1", "2025-01-01T00:00:00Z");
        let b = turn("m1", "2025-01-01T00:00:01Z"); // different ts
        assert_eq!(turn_id_fingerprint(&a), turn_id_fingerprint(&b));
    }

    #[test]
    fn turn_id_fingerprint_changes_with_message_id() {
        let a = turn("m1", "2025-01-01T00:00:00Z");
        let b = turn("m2", "2025-01-01T00:00:00Z");
        assert_ne!(turn_id_fingerprint(&a), turn_id_fingerprint(&b));
    }

    #[test]
    fn content_fingerprint_collapses_renamed_message_ids() {
        let a = turn("m1", "2025-01-01T00:00:00Z");
        let b = turn("m2", "2025-01-01T00:00:00Z");
        // Layer-2: same shape, different messageId — fingerprint matches.
        assert_eq!(turn_content_fingerprint(&a), turn_content_fingerprint(&b));
    }

    #[test]
    fn content_fingerprint_distinguishes_different_models() {
        let a = turn("m1", "2025-01-01T00:00:00Z");
        let mut b = turn("m1", "2025-01-01T00:00:00Z");
        b.model = "claude-opus-4-7".into();
        assert_ne!(turn_content_fingerprint(&a), turn_content_fingerprint(&b));
    }

    #[test]
    fn fingerprint_is_16_hex_chars() {
        let a = turn("m1", "2025-01-01T00:00:00Z");
        let fp = turn_id_fingerprint(&a);
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
