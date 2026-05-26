//! Append-only writer paths. Each verb runs its inserts inside a single
//! transaction; SQLite WAL serializes concurrent writers without any
//! user-space lock.
//!
//! All inserts use `INSERT OR IGNORE` against the table's primary key —
//! re-ingesting the same upstream bytes is a no-op (layer-1 dedup). For
//! turns we additionally consult the `content_fingerprint` index before
//! inserting so a re-emitted-under-a-new-messageId turn collapses
//! against the layer-2 fingerprint as well.

use rusqlite::{params, Connection, OptionalExtension};

use crate::reader::{
    CompactionEvent, ContentRecord, Inference, SessionRelationshipRecord, ToolResultEventRecord,
    TurnRecord, UserTurnRecord,
};

use crate::ledger::error::{LedgerError, Result};
use crate::ledger::fingerprint::{
    compaction_id_fingerprint, content_blob_fingerprint, relationship_id_fingerprint,
    tool_result_event_id_fingerprint, turn_content_fingerprint, user_turn_id_fingerprint,
};
use crate::ledger::paths::is_valid_session_id;
use crate::ledger::stamp::Stamp;

/// Sortable monotonic application-clock token used to stamp `written_at`
/// and content `created_at`. Format is `ts:{secs:020}.{nanos:09}` — NOT
/// ISO-8601. Ordering (lex == chronological) is the only contract; we
/// don't pay for a calendar-formatting dep just for stamps. The widths
/// matter — anything narrower flips the lex sort. See `format_cutoff_ts`
/// in `relayburn-cli/src/commands/state.rs` for the matching cutoff
/// helper.
fn now_lex_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let secs = nanos / 1_000_000_000;
    let nanos_part = nanos % 1_000_000_000;
    format!("ts:{:020}.{:09}", secs, nanos_part)
}

pub(crate) fn append_turns(conn: &mut Connection, turns: &[TurnRecord]) -> Result<usize> {
    if turns.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut appended = 0usize;
    {
        let mut content_lookup =
            tx.prepare("SELECT 1 FROM turns WHERE content_fingerprint = ? LIMIT 1")?;
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO turns
                 (source, session_id, message_id, ts, project, project_key,
                  record_json, content_fingerprint, stop_reason, subagent_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )?;
        for t in turns {
            let fingerprint = turn_content_fingerprint(t);
            let already: Option<i64> = content_lookup
                .query_row(params![&fingerprint], |r| r.get(0))
                .optional()?;
            if already.is_some() {
                continue;
            }
            let json = serde_json::to_string(t)?;
            // Denormalize `stop_reason` so summary aggregations don't have to
            // re-deserialize `record_json`. NULL for Codex (no field) and
            // pre-3.0 imports.
            let stop_reason_str = t.stop_reason.as_ref().map(|s| s.wire_str());
            // Denormalize `subagent.agent_id` into the v4 `subagent_id`
            // column so queries can count / filter subagent rows
            // structurally without deserializing `record_json`. NULL
            // when the row isn't a subagent or the parser couldn't
            // resolve the agent id (e.g. sidechain marker without a
            // chain-walk match). See AgentWorkforce/burn#435.
            let subagent_id = t.subagent.as_ref().and_then(|s| s.agent_id.as_deref());
            let changed = insert.execute(params![
                t.source.wire_str(),
                t.session_id,
                t.message_id,
                t.ts,
                t.project,
                t.project_key,
                json,
                fingerprint,
                stop_reason_str,
                subagent_id,
            ])?;
            if changed > 0 {
                appended += 1;
            }
        }
    }
    tx.commit()?;
    Ok(appended)
}

pub(crate) fn append_compactions(
    conn: &mut Connection,
    events: &[CompactionEvent],
) -> Result<usize> {
    if events.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut appended = 0usize;
    {
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO compactions
                 (id_fingerprint, source, session_id, ts, record_json)
             VALUES (?, ?, ?, ?, ?)",
        )?;
        for e in events {
            let id = compaction_id_fingerprint(e);
            let json = serde_json::to_string(e)?;
            let changed =
                insert.execute(params![id, e.source.wire_str(), e.session_id, e.ts, json])?;
            if changed > 0 {
                appended += 1;
            }
        }
    }
    tx.commit()?;
    Ok(appended)
}

pub(crate) fn append_relationships(
    conn: &mut Connection,
    records: &[SessionRelationshipRecord],
) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut appended = 0usize;
    {
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO relationships
                 (id_fingerprint, source, session_id, related_session_id,
                  relationship_type, ts, record_json)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        for r in records {
            let id = relationship_id_fingerprint(r);
            let json = serde_json::to_string(r)?;
            let changed = insert.execute(params![
                id,
                r.source.wire_str(),
                r.session_id,
                r.related_session_id,
                r.relationship_type.wire_str(),
                r.ts,
                json,
            ])?;
            if changed > 0 {
                appended += 1;
            }
        }
    }
    tx.commit()?;
    Ok(appended)
}

pub(crate) fn append_tool_result_events(
    conn: &mut Connection,
    records: &[ToolResultEventRecord],
) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut appended = 0usize;
    {
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO tool_result_events
                 (id_fingerprint, source, session_id, tool_use_id, event_index, ts,
                  record_json, output_bytes, output_truncated)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )?;
        for r in records {
            let id = tool_result_event_id_fingerprint(r);
            let json = serde_json::to_string(r)?;
            let truncated_int: Option<i64> = r.output_truncated.map(|b| if b { 1 } else { 0 });
            let changed = insert.execute(params![
                id,
                r.source.wire_str(),
                r.session_id,
                r.tool_use_id,
                r.event_index as i64,
                r.ts,
                json,
                r.output_bytes.map(|n| n as i64),
                truncated_int,
            ])?;
            if changed > 0 {
                appended += 1;
            }
        }
    }
    tx.commit()?;
    Ok(appended)
}

/// `INSERT OR REPLACE` per-API-call inferences. Re-ingest of the same
/// session intentionally replaces existing rows: the inference is pure
/// derived state (no fingerprint dedup, no first-party fields), and a
/// re-parse may legitimately produce different `end_ts` / `usage` values
/// if the JSONL grew between runs. The composite PK
/// `(source, session_id, request_id)` is the natural identity. See issue
/// #434.
pub(crate) fn append_inferences(conn: &mut Connection, records: &[Inference]) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut appended = 0usize;
    {
        let mut insert = tx.prepare(
            "INSERT OR REPLACE INTO inferences
                 (source, session_id, request_id, request_id_source, turn_id,
                  model, kind, start_ts, end_ts, record_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )?;
        for r in records {
            let json = serde_json::to_string(r)?;
            let changed = insert.execute(params![
                r.source.wire_str(),
                r.session_id,
                r.request_id,
                r.request_id_source.wire_str(),
                r.turn_id,
                r.model,
                r.kind.wire_str(),
                r.start_ts,
                r.end_ts,
                json,
            ])?;
            if changed > 0 {
                appended += 1;
            }
        }
    }
    tx.commit()?;
    Ok(appended)
}

pub(crate) fn append_user_turns(
    conn: &mut Connection,
    records: &[UserTurnRecord],
) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut appended = 0usize;
    {
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO user_turns
                 (id_fingerprint, source, session_id, user_uuid, ts, record_json)
             VALUES (?, ?, ?, ?, ?, ?)",
        )?;
        for r in records {
            let id = user_turn_id_fingerprint(r);
            let json = serde_json::to_string(r)?;
            let changed = insert.execute(params![
                id,
                r.source.wire_str(),
                r.session_id,
                r.user_uuid,
                r.ts,
                json,
            ])?;
            if changed > 0 {
                appended += 1;
            }
        }
    }
    tx.commit()?;
    Ok(appended)
}

pub(crate) fn append_stamp(conn: &mut Connection, stamp: &Stamp) -> Result<()> {
    let selector_json = serde_json::to_string(&stamp.selector)?;
    let enrichment_json = serde_json::to_string(&stamp.enrichment)?;
    // Synthesize a spawn-env relationship row when the stamp carries a
    // `parentAgentId` enrichment — mirrors the TS adapter so downstream
    // queries see the relationship even when the source log itself
    // didn't carry it.
    let synthesized = synthesize_relationship(stamp);

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let written_at = now_lex_token();
    {
        tx.prepare(
            "INSERT INTO stamps (source, session_id, ts, selector_json, enrichment_json, written_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )?
        .execute(params![
            "",
            stamp.selector.session_id,
            stamp.ts,
            selector_json,
            enrichment_json,
            written_at,
        ])?;
        if let Some(rel) = synthesized {
            let id = relationship_id_fingerprint(&rel);
            let json = serde_json::to_string(&rel)?;
            tx.prepare(
                "INSERT OR IGNORE INTO relationships
                     (id_fingerprint, source, session_id, related_session_id,
                      relationship_type, ts, record_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )?
            .execute(params![
                id,
                rel.source.wire_str(),
                rel.session_id,
                rel.related_session_id,
                rel.relationship_type.wire_str(),
                rel.ts,
                json,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn append_content(conn: &mut Connection, records: &[ContentRecord]) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut appended = 0usize;
    {
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO content
                 (source, session_id, message_id, content_hash, body, byte_length, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        let now = now_lex_token();
        for r in records {
            if !is_valid_session_id(&r.session_id) {
                return Err(LedgerError::InvalidSessionId(r.session_id.clone()));
            }
            let body = serde_json::to_string(r)?;
            let body_bytes = body.as_bytes();
            let hash = content_blob_fingerprint(body_bytes);
            let changed = insert.execute(params![
                r.source.wire_str(),
                r.session_id,
                r.message_id,
                hash,
                body,
                body_bytes.len() as i64,
                now,
            ])?;
            if changed > 0 {
                appended += 1;
            }
        }
    }
    tx.commit()?;
    Ok(appended)
}

/// If `stamp` carries a `parentAgentId` enrichment, synthesize the
/// implied subagent relationship row. Returns `None` for stamps that
/// don't target a session or don't carry the enrichment key.
pub(crate) fn synthesize_relationship(stamp: &Stamp) -> Option<SessionRelationshipRecord> {
    use crate::reader::{RelationshipSourceKind, RelationshipType};
    let session_id = stamp.selector.session_id.clone()?;
    if session_id.is_empty() {
        return None;
    }
    let parent = stamp.enrichment.get("parentAgentId")?;
    if parent.is_empty() {
        return None;
    }
    Some(SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::SpawnEnv,
        session_id,
        related_session_id: Some(parent.clone()),
        relationship_type: RelationshipType::Subagent,
        ts: Some(stamp.ts.clone()),
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: stamp.enrichment.get("agentId").cloned(),
        subagent_type: None,
        description: None,
    })
}

pub(crate) fn debug_now() -> String {
    now_lex_token()
}
