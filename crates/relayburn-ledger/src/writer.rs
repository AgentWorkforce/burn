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

use relayburn_reader::{
    CompactionEvent, ContentRecord, SessionRelationshipRecord, ToolResultEventRecord, TurnRecord,
    UserTurnRecord,
};

use crate::error::{LedgerError, Result};
use crate::fingerprint::{
    compaction_id_fingerprint, content_blob_fingerprint, relationship_id_fingerprint,
    tool_result_event_id_fingerprint, turn_content_fingerprint, user_turn_id_fingerprint,
};
use crate::paths::is_valid_session_id;
use crate::stamp::Stamp;

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Application-clock string for `written_at`. We don't need calendar
    // formatting here — ordering is the only contract — and rolling our
    // own ISO formatter avoids a chrono / time dep just for stamps.
    let secs = nanos / 1_000_000_000;
    let nanos_part = nanos % 1_000_000_000;
    format!("ts:{:020}.{:09}", secs, nanos_part)
}

fn source_str<T: serde::Serialize>(v: &T) -> Result<String> {
    let value = serde_json::to_value(v)?;
    Ok(match value {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
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
                 (source, session_id, message_id, ts, project, project_key, record_json, content_fingerprint)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )?;
        for t in turns {
            let fingerprint = turn_content_fingerprint(t);
            let already: Option<i64> = content_lookup
                .query_row(params![&fingerprint], |r| r.get(0))
                .optional()?;
            if already.is_some() {
                continue;
            }
            let source = source_str(&t.source)?;
            let json = serde_json::to_string(t)?;
            let changed = insert.execute(params![
                source,
                t.session_id,
                t.message_id,
                t.ts,
                t.project,
                t.project_key,
                json,
                fingerprint,
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
            let source = source_str(&e.source)?;
            let json = serde_json::to_string(e)?;
            let changed = insert.execute(params![id, source, e.session_id, e.ts, json])?;
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
            let source = source_str(&r.source)?;
            let relationship_type = source_str(&r.relationship_type)?;
            let json = serde_json::to_string(r)?;
            let changed = insert.execute(params![
                id,
                source,
                r.session_id,
                r.related_session_id,
                relationship_type,
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
                 (id_fingerprint, source, session_id, tool_use_id, event_index, ts, record_json)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        for r in records {
            let id = tool_result_event_id_fingerprint(r);
            let source = source_str(&r.source)?;
            let json = serde_json::to_string(r)?;
            let changed = insert.execute(params![
                id,
                source,
                r.session_id,
                r.tool_use_id,
                r.event_index as i64,
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
            let source = source_str(&r.source)?;
            let json = serde_json::to_string(r)?;
            let changed = insert.execute(params![
                id, source, r.session_id, r.user_uuid, r.ts, json,
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
    let written_at = now_iso();
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
            let source = source_str(&rel.source)?;
            let relationship_type = source_str(&rel.relationship_type)?;
            let json = serde_json::to_string(&rel)?;
            tx.prepare(
                "INSERT OR IGNORE INTO relationships
                     (id_fingerprint, source, session_id, related_session_id,
                      relationship_type, ts, record_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )?
            .execute(params![
                id,
                source,
                rel.session_id,
                rel.related_session_id,
                relationship_type,
                rel.ts,
                json,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn append_content(
    conn: &mut Connection,
    records: &[ContentRecord],
) -> Result<usize> {
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
        let now = now_iso();
        for r in records {
            if !is_valid_session_id(&r.session_id) {
                return Err(LedgerError::InvalidSessionId(r.session_id.clone()));
            }
            let source = source_str(&r.source)?;
            let body = serde_json::to_string(r)?;
            let body_bytes = body.as_bytes();
            let hash = content_blob_fingerprint(body_bytes);
            let changed = insert.execute(params![
                source,
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
    use relayburn_reader::{RelationshipSourceKind, RelationshipType};
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

#[allow(dead_code)]
pub(crate) fn debug_now() -> String {
    now_iso()
}

