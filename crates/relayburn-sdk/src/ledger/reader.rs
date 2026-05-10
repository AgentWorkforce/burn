//! Read paths: turn / compaction / relationship / tool-result-event /
//! user-turn queries, plus stamp folding for enrichment.
//!
//! Streams from a prepared `SELECT … ORDER BY rowid` so insertion order
//! is preserved on the wire — same contract as the JSONL ledger of 1.x,
//! so downstream consumers comparing two adapters byte-for-byte stay
//! happy.

use std::collections::{BTreeMap, HashSet};

use rusqlite::{Connection, params_from_iter};
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
    let (sql, bound) = build_select_sql(
        "turns",
        q,
        TableFilters {
            ts_nullable: false,
            session_id_or_related: false,
            project_columns: true,
        },
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(bound.iter()), |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for json in rows {
        let turn: TurnRecord = match serde_json::from_str(&json) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let enrichment = fold_stamps(&turn, &stamps);
        if !enrichment_filter_passes(&enrichment, q) {
            continue;
        }
        out.push(EnrichedTurn { turn, enrichment });
    }
    Ok(out)
}

pub(crate) fn query_compactions(conn: &Connection, q: &Query) -> Result<Vec<CompactionEvent>> {
    select_filtered_records(
        conn,
        "compactions",
        q,
        TableFilters {
            ts_nullable: false,
            session_id_or_related: false,
            project_columns: false,
        },
    )
}

pub(crate) fn query_relationships(
    conn: &Connection,
    q: &Query,
) -> Result<Vec<SessionRelationshipRecord>> {
    select_filtered_records(
        conn,
        "relationships",
        q,
        TableFilters {
            ts_nullable: true,
            session_id_or_related: true,
            project_columns: false,
        },
    )
}

pub(crate) fn query_tool_result_events(
    conn: &Connection,
    q: &Query,
) -> Result<Vec<ToolResultEventRecord>> {
    select_filtered_records(
        conn,
        "tool_result_events",
        q,
        TableFilters {
            ts_nullable: true,
            session_id_or_related: false,
            project_columns: false,
        },
    )
}

pub(crate) fn query_user_turns(conn: &Connection, q: &Query) -> Result<Vec<UserTurnRecord>> {
    select_filtered_records(
        conn,
        "user_turns",
        q,
        TableFilters {
            ts_nullable: false,
            session_id_or_related: false,
            project_columns: false,
        },
    )
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
    let mut stmt = conn.prepare_cached("SELECT DISTINCT session_id FROM user_turns")?;
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

/// Per-table column shape used to build the SQL `WHERE` clause from a
/// [`Query`]. Each table has the same logical filters, but their column
/// shapes differ:
///
/// - `ts_nullable`: relationships and tool_result_events store `ts` as
///   nullable, and the historical Rust filter passed rows whose `ts` was
///   `NULL` regardless of `since`/`until`. Mirror that with
///   `(ts IS NULL OR ts >= ?)`.
/// - `session_id_or_related`: relationships match the filter against
///   either `session_id` or `related_session_id`.
/// - `project_columns`: only `turns` carries the `project` /
///   `project_key` columns surfaced via `Query::project`.
#[derive(Clone, Copy)]
struct TableFilters {
    ts_nullable: bool,
    session_id_or_related: bool,
    project_columns: bool,
}

/// Build a `SELECT record_json FROM <table> WHERE … ORDER BY rowid`
/// statement that pushes every supported [`Query`] predicate into SQL,
/// alongside the parameter list to bind. Generates a stable SQL string
/// per filter combination so [`Connection::prepare_cached`] can reuse the
/// compiled statement across calls.
fn build_select_sql(table: &str, q: &Query, shape: TableFilters) -> (String, Vec<String>) {
    let mut sql = format!("SELECT record_json FROM {table}");
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut bound: Vec<String> = Vec::new();

    if let Some(since) = &q.since {
        if shape.ts_nullable {
            clauses.push("(ts IS NULL OR ts >= ?)");
        } else {
            clauses.push("ts >= ?");
        }
        bound.push(since.clone());
    }
    if let Some(until) = &q.until {
        if shape.ts_nullable {
            clauses.push("(ts IS NULL OR ts <= ?)");
        } else {
            clauses.push("ts <= ?");
        }
        bound.push(until.clone());
    }
    if let Some(sid) = &q.session_id {
        if shape.session_id_or_related {
            clauses.push("(session_id = ? OR related_session_id = ?)");
            bound.push(sid.clone());
            bound.push(sid.clone());
        } else {
            clauses.push("session_id = ?");
            bound.push(sid.clone());
        }
    }
    if let Some(source) = q.source {
        clauses.push("source = ?");
        bound.push(source.wire_str().to_string());
    }
    if shape.project_columns {
        if let Some(project) = &q.project {
            clauses.push("(project = ? OR project_key = ?)");
            bound.push(project.clone());
            bound.push(project.clone());
        }
    }

    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY rowid");
    (sql, bound)
}

fn select_filtered_records<T>(
    conn: &Connection,
    table: &str,
    q: &Query,
    shape: TableFilters,
) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let (sql, bound) = build_select_sql(table, q, shape);
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(bound.iter()), |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for json in rows {
        match serde_json::from_str::<T>(&json) {
            Ok(record) => out.push(record),
            Err(_) => continue,
        }
    }
    Ok(out)
}

fn collect_stamps(conn: &Connection) -> Result<Vec<Stamp>> {
    let mut stmt = conn.prepare_cached(
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

/// Folded-stamp enrichment is the only [`Query`] predicate that can't be
/// pushed into SQL — stamps live in their own table and the match logic
/// runs in Rust. SQL handles `since` / `until` / `session_id` / `source`
/// / `project` for us, so this is the last gate before yielding a turn.
fn enrichment_filter_passes(enrichment: &Enrichment, q: &Query) -> bool {
    let Some(ref wanted) = q.enrichment else {
        return true;
    };
    wanted
        .iter()
        .all(|(key, value)| enrichment.get(key) == Some(value))
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
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

