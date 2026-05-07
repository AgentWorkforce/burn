//! Read paths: turn / compaction / relationship / tool-result-event /
//! user-turn queries, plus stamp folding for enrichment.
//!
//! Streams from a prepared `SELECT … ORDER BY rowid` so insertion order
//! is preserved on the wire — same contract as the JSONL ledger of 1.x,
//! so downstream consumers comparing two adapters byte-for-byte stay
//! happy.

use std::collections::{BTreeMap, HashSet};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::reader::{
    CompactionEvent, SessionRelationshipRecord, ToolResultEventRecord, TurnRecord, UserTurnRecord,
};

use crate::ledger::error::Result;
use crate::ledger::paths::is_valid_session_id;
use crate::ledger::query::Query;
use crate::ledger::stamp::{Enrichment, Stamp, StampSelector, stamp_matches};

/// A turn with stamp enrichment folded in. Enrichment is a flat
/// string→string map; entries from later stamps win.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrichedTurn {
    #[serde(flatten)]
    pub turn: TurnRecord,
    pub enrichment: Enrichment,
}

pub(crate) fn query_turns(conn: &Connection, q: &Query) -> Result<Vec<EnrichedTurn>> {
    let stamps = collect_stamps(conn)?;
    let mut stmt = conn.prepare("SELECT record_json FROM turns ORDER BY rowid")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for json in rows {
        let turn: TurnRecord = match serde_json::from_str(&json) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let enrichment = fold_stamps(&turn, &stamps);
        if !turn_passes(&turn, q, &enrichment) {
            continue;
        }
        out.push(EnrichedTurn { turn, enrichment });
    }
    Ok(out)
}

pub(crate) fn query_compactions(conn: &Connection, q: &Query) -> Result<Vec<CompactionEvent>> {
    select_records(conn, "compactions", |r: CompactionEvent| {
        compaction_passes(&r, q).then_some(r)
    })
}

pub(crate) fn query_relationships(
    conn: &Connection,
    q: &Query,
) -> Result<Vec<SessionRelationshipRecord>> {
    select_records(conn, "relationships", |r: SessionRelationshipRecord| {
        relationship_passes(&r, q).then_some(r)
    })
}

pub(crate) fn query_tool_result_events(
    conn: &Connection,
    q: &Query,
) -> Result<Vec<ToolResultEventRecord>> {
    select_records(conn, "tool_result_events", |r: ToolResultEventRecord| {
        tool_result_event_passes(&r, q).then_some(r)
    })
}

pub(crate) fn query_user_turns(conn: &Connection, q: &Query) -> Result<Vec<UserTurnRecord>> {
    select_records(conn, "user_turns", |r: UserTurnRecord| {
        user_turn_passes(&r, q).then_some(r)
    })
}

pub(crate) fn list_stamps(conn: &Connection) -> Result<Vec<Stamp>> {
    collect_stamps(conn)
}

/// Distinct `session_id` values present in the `user_turns` table. Powers
/// the "skip sessions whose user-turn rows I already have" filter in
/// `relayburn-ingest::reingest_missing_content` (#278). Mirrors the TS
/// `(await queryUserTurns()).map((u) => u.sessionId)` extraction without
/// having to materialize every row.
///
/// Filters out malformed ids defensively (mirrors
/// `content::list_session_ids`); a corrupted row should not poison the
/// caller's skip set. The `user_turns` table is STRICT, so a non-TEXT
/// session_id should never reach us, but the extra guard keeps the
/// surface symmetric.
pub(crate) fn list_user_turn_session_ids(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT session_id FROM user_turns")?;
    let mut rows = stmt.query([])?;
    let mut out = HashSet::new();
    while let Some(row) = rows.next()? {
        let Ok(session_id) = row.get::<_, String>(0) else {
            continue;
        };
        if is_valid_session_id(&session_id) {
            out.insert(session_id);
        }
    }
    Ok(out)
}

fn select_records<T, F>(conn: &Connection, table: &str, mut filter: F) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
    F: FnMut(T) -> Option<T>,
{
    let sql = format!("SELECT record_json FROM {table} ORDER BY rowid");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for json in rows {
        let record: T = match serde_json::from_str(&json) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let Some(kept) = filter(record) {
            out.push(kept);
        }
    }
    Ok(out)
}

fn collect_stamps(conn: &Connection) -> Result<Vec<Stamp>> {
    let mut stmt = conn.prepare(
        "SELECT ts, selector_json, enrichment_json
         FROM stamps
         ORDER BY ts, written_at",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (ts, sel_json, enr_json) in rows {
        let selector: StampSelector = serde_json::from_str(&sel_json).unwrap_or_default();
        let enrichment: Enrichment =
            serde_json::from_str(&enr_json).unwrap_or_else(|_| BTreeMap::new());
        // Stamps were validated on write; re-read may surface a row
        // that an external editor turned into nonsense, but that's
        // exactly what we want to ignore — folding a no-op selector
        // can't enrich anything.
        out.push(Stamp {
            ts,
            selector,
            enrichment,
        });
    }
    Ok(out)
}

fn fold_stamps(turn: &TurnRecord, stamps: &[Stamp]) -> Enrichment {
    let mut out = Enrichment::new();
    for s in stamps {
        if !stamp_matches(s, turn) {
            continue;
        }
        for (k, v) in s.enrichment.iter() {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn turn_passes(turn: &TurnRecord, q: &Query, enrichment: &Enrichment) -> bool {
    if let Some(ref since) = q.since {
        if &turn.ts < since {
            return false;
        }
    }
    if let Some(ref until) = q.until {
        if &turn.ts > until {
            return false;
        }
    }
    if let Some(ref project) = q.project {
        let matches_project = turn.project.as_deref() == Some(project)
            || turn.project_key.as_deref() == Some(project);
        if !matches_project {
            return false;
        }
    }
    if let Some(ref sid) = q.session_id {
        if &turn.session_id != sid {
            return false;
        }
    }
    if let Some(source) = q.source {
        if turn.source != source {
            return false;
        }
    }
    if let Some(ref wanted) = q.enrichment {
        for (key, value) in wanted {
            if enrichment.get(key) != Some(value) {
                return false;
            }
        }
    }
    true
}

fn compaction_passes(e: &CompactionEvent, q: &Query) -> bool {
    if let Some(ref since) = q.since {
        if &e.ts < since {
            return false;
        }
    }
    if let Some(ref until) = q.until {
        if &e.ts > until {
            return false;
        }
    }
    if let Some(ref sid) = q.session_id {
        if &e.session_id != sid {
            return false;
        }
    }
    if let Some(source) = q.source {
        if e.source != source {
            return false;
        }
    }
    true
}

fn relationship_passes(r: &SessionRelationshipRecord, q: &Query) -> bool {
    if let (Some(ref since), Some(ref ts)) = (&q.since, &r.ts) {
        if ts < since {
            return false;
        }
    }
    if let (Some(ref until), Some(ref ts)) = (&q.until, &r.ts) {
        if ts > until {
            return false;
        }
    }
    if let Some(ref sid) = q.session_id {
        if &r.session_id != sid && r.related_session_id.as_ref() != Some(sid) {
            return false;
        }
    }
    if let Some(source) = q.source {
        // `Query.source` is a `SourceKind` (the harness identity);
        // `r.source` is a `RelationshipSourceKind` (a superset that
        // also covers `spawn-env`, `native-claude`, etc.). Compare via
        // their serialized kebab-case forms so a `source = "claude-code"`
        // filter matches both enums identically — same semantics as the
        // TS adapter, which compared the raw strings.
        let want = serde_json::to_value(source)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string));
        let have = serde_json::to_value(r.source)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string));
        if want != have {
            return false;
        }
    }
    true
}

fn tool_result_event_passes(r: &ToolResultEventRecord, q: &Query) -> bool {
    if let (Some(ref since), Some(ref ts)) = (&q.since, &r.ts) {
        if ts < since {
            return false;
        }
    }
    if let (Some(ref until), Some(ref ts)) = (&q.until, &r.ts) {
        if ts > until {
            return false;
        }
    }
    if let Some(ref sid) = q.session_id {
        if &r.session_id != sid {
            return false;
        }
    }
    if let Some(source) = q.source {
        if r.source != source {
            return false;
        }
    }
    true
}

fn user_turn_passes(r: &UserTurnRecord, q: &Query) -> bool {
    if let Some(ref since) = q.since {
        if &r.ts < since {
            return false;
        }
    }
    if let Some(ref until) = q.until {
        if &r.ts > until {
            return false;
        }
    }
    if let Some(ref sid) = q.session_id {
        if &r.session_id != sid {
            return false;
        }
    }
    if let Some(source) = q.source {
        if r.source != source {
            return false;
        }
    }
    true
}

/// Tables `count_table` and `raw_record_jsons` are allowed to address.
/// Both interpolate the table name directly into SQL (rusqlite doesn't
/// take identifiers as bound parameters), so this list is the
/// security boundary against subquery-injection by downstream callers.
const QUERYABLE_TABLES: &[&str] = &[
    "turns",
    "compactions",
    "relationships",
    "tool_result_events",
    "user_turns",
    "sessions",
    "stamps",
    "archive_state",
];

fn validate_table(table: &str) -> Result<()> {
    if QUERYABLE_TABLES.contains(&table) {
        Ok(())
    } else {
        Err(crate::ledger::error::LedgerError::Other(format!(
            "unknown ledger table: {table}"
        )))
    }
}

pub(crate) fn count_table(conn: &Connection, table: &str) -> Result<i64> {
    validate_table(table)?;
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let count: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
    Ok(count)
}

pub(crate) fn raw_record_jsons(conn: &Connection, table: &str) -> Result<Vec<String>> {
    validate_table(table)?;
    let sql = format!("SELECT record_json FROM {table} ORDER BY rowid");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

