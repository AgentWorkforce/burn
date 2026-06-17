//! Read paths: turn / compaction / relationship / tool-result-event /
//! user-turn queries, plus stamp folding for enrichment.
//!
//! Streams from a prepared `SELECT … ORDER BY rowid` so insertion order
//! is preserved on the wire — same contract as the JSONL ledger of 1.x,
//! so downstream consumers comparing two adapters byte-for-byte stay
//! happy.

use std::collections::{BTreeMap, HashSet};

use rusqlite::{params_from_iter, Connection};
use serde::{Deserialize, Serialize};

use crate::reader::{
    CompactionEvent, Inference, SessionRelationshipRecord, ToolResultEventRecord, TurnRecord,
    UserTurnRecord,
};

use crate::ledger::error::Result;
use crate::ledger::paths::is_valid_session_id;
use crate::ledger::query::Query;
use crate::ledger::stamp::{stamp_matches, Enrichment, Stamp, StampSelector};

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
            prefer_ts_index: true,
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

/// Like `query_turns`, but matches any of `session_ids` in one SQL pass and
/// loads the stamps table once. `base` supplies the non-session filters
/// (source, since/until, project); its own `session_id` is ignored.
///
/// Empty `session_ids` returns `Ok(vec![])` without touching the DB.
/// IDs are chunked in groups of at most 500 so prepared statements stay
/// cacheable and the IN-list stays well under SQLite's host-parameter limit.
pub(crate) fn query_turns_in_sessions(
    conn: &Connection,
    base: &Query,
    session_ids: &[String],
) -> Result<Vec<EnrichedTurn>> {
    if session_ids.is_empty() {
        return Ok(vec![]);
    }

    let stamps = collect_stamps(conn)?;

    // Build the base WHERE clause without a session_id filter.
    let base_query = Query {
        session_id: None,
        ..base.clone()
    };
    let (base_sql, base_bound) = build_select_sql(
        "turns",
        &base_query,
        TableFilters {
            ts_nullable: false,
            session_id_or_related: false,
            project_columns: true,
            // This path targets specific sessions via the injected IN clause
            // below, which rides idx_turns_session — do not force the ts index.
            prefer_ts_index: false,
        },
    );

    // Determine the WHERE prefix to know how to append the IN clause.
    // `build_select_sql` appends " ORDER BY rowid" at the end; we need to
    // inject our IN clause before that suffix.
    const ORDER_SUFFIX: &str = " ORDER BY rowid";
    let base_without_order = base_sql.strip_suffix(ORDER_SUFFIX).unwrap_or(&base_sql);
    let has_where = base_without_order.contains(" WHERE ");

    let mut out = Vec::new();

    for chunk in session_ids.chunks(500) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let connector = if has_where { " AND " } else { " WHERE " };
        let sql =
            format!("{base_without_order}{connector}session_id IN ({placeholders}){ORDER_SUFFIX}");

        let mut bound: Vec<String> = base_bound.clone();
        bound.extend(chunk.iter().cloned());

        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(bound.iter()), |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        for json in rows {
            let turn: TurnRecord = match serde_json::from_str(&json) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let enrichment = fold_stamps(&turn, &stamps);
            if !enrichment_filter_passes(&enrichment, base) {
                continue;
            }
            out.push(EnrichedTurn { turn, enrichment });
        }
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
            prefer_ts_index: false,
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
            prefer_ts_index: false,
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
            prefer_ts_index: false,
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
            prefer_ts_index: false,
        },
    )
}

/// Read per-API-call inferences, applying the standard `Query` filters
/// (since / until / session_id / source). The `inferences` table stores
/// `start_ts` as `ts` for filter purposes — earliest row in the call
/// wins for "did anything happen in this window".
pub(crate) fn query_inferences(conn: &Connection, q: &Query) -> Result<Vec<Inference>> {
    // The inferences table doesn't carry a `ts` column literally; the
    // since/until filters route through `start_ts` instead. We reuse the
    // generic `build_select_sql` by wrapping the SQL ourselves rather
    // than threading another `TableFilters` knob for a single-table case.
    let mut sql = String::from("SELECT record_json FROM inferences");
    let mut clauses: Vec<&'static str> = Vec::new();
    let mut bound: Vec<String> = Vec::new();
    if let Some(since) = &q.since {
        clauses.push("start_ts >= ?");
        bound.push(since.clone());
    }
    if let Some(until) = &q.until {
        clauses.push("start_ts <= ?");
        bound.push(until.clone());
    }
    if let Some(sid) = &q.session_id {
        clauses.push("session_id = ?");
        bound.push(sid.clone());
    }
    if let Some(source) = q.source {
        clauses.push("source = ?");
        bound.push(source.wire_str().to_string());
    }
    // The `inferences` table doesn't carry `project` / `project_key`
    // directly — those live on `turns`. Inferences are derived per
    // session, so filtering by "session has any turn with this project"
    // is sufficient. Mirrors the predicate shape used by `query_turns`.
    if let Some(project) = &q.project {
        clauses.push(
            "session_id IN (SELECT DISTINCT session_id FROM turns \
             WHERE project = ? OR project_key = ?)",
        );
        bound.push(project.clone());
        bound.push(project.clone());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY rowid");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(bound.iter()), |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for json in rows {
        if let Ok(rec) = serde_json::from_str::<Inference>(&json) {
            out.push(rec);
        }
    }
    Ok(out)
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
    /// Force `idx_turns_ts` for a bounded `ts` window (see the access-path
    /// note in [`build_select_sql`]). Set only by the full-window
    /// [`query_turns`] read against the `turns` table; left `false` for the
    /// session-IN path (which rides `idx_turns_session`) and every non-`turns`
    /// table (which has no such index).
    prefer_ts_index: bool,
}

/// Build a `SELECT record_json FROM <table> WHERE … ORDER BY rowid`
/// statement that pushes every supported [`Query`] predicate into SQL,
/// alongside the parameter list to bind. Generates a stable SQL string
/// per filter combination so [`Connection::prepare_cached`] can reuse the
/// compiled statement across calls.
fn build_select_sql(table: &str, q: &Query, shape: TableFilters) -> (String, Vec<String>) {
    // Access-path hint: a bounded `ts` window on `turns` should seek the
    // matching tail via `idx_turns_ts` instead of scanning every row. Because
    // the statement ends in `ORDER BY rowid`, SQLite otherwise prefers a full
    // rowid `SCAN` (no sort needed) and reads the entire table — ~85ms / ~97k
    // rows on a large ledger even when the window matches ~1% of turns. Forcing
    // the index turns that into an index range-seek plus an in-memory sort of
    // just the matched rows; output stays byte-identical (same rows, same
    // `ORDER BY rowid`). Applied only when:
    //   (a) the table is `turns` (the only one carrying `idx_turns_ts`),
    //   (b) there is a `ts` bound to seek on, and
    //   (c) no `session_id` filter — session-scoped reads ride
    //       `idx_turns_session` and are already cheap; forcing the ts index
    //       there would pessimize them.
    // Trade-off: a window covering most of the table pays an index seek + a
    // large sort instead of a plain scan, but such unbounded reads are already
    // aggregation-bound (well over any interactive budget), so the common
    // recent-window path is the one worth optimizing.
    let use_ts_index =
        shape.prefer_ts_index && q.session_id.is_none() && (q.since.is_some() || q.until.is_some());
    let from = if use_ts_index {
        std::borrow::Cow::Owned(format!("{table} INDEXED BY idx_turns_ts"))
    } else {
        std::borrow::Cow::Borrowed(table)
    };
    let mut sql = format!("SELECT record_json FROM {from}");
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
    "inferences",
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

#[cfg(test)]
mod build_select_sql_tests {
    use super::*;

    fn turns_shape(prefer_ts_index: bool) -> TableFilters {
        TableFilters {
            ts_nullable: false,
            session_id_or_related: false,
            project_columns: true,
            prefer_ts_index,
        }
    }

    /// The `query_turns` full-window path forces `idx_turns_ts` for a bounded
    /// `ts` filter — that's the access-path fix that keeps `summary` /
    /// `hotspots` / `overhead` / `compare` off a full table scan. Ordering
    /// stays `ORDER BY rowid`, so rows (and golden output) are unchanged.
    #[test]
    fn ts_window_forces_index_and_keeps_rowid_order() {
        let q = Query {
            since: Some("2026-06-16T00:00:00.000Z".into()),
            ..Query::default()
        };
        let (sql, bound) = build_select_sql("turns", &q, turns_shape(true));
        assert!(
            sql.contains("FROM turns INDEXED BY idx_turns_ts"),
            "bounded ts window must seek the ts index, got: {sql}"
        );
        assert!(sql.trim_end().ends_with("ORDER BY rowid"), "got: {sql}");
        assert_eq!(bound, vec!["2026-06-16T00:00:00.000Z".to_string()]);
    }

    /// A `session_id` filter rides `idx_turns_session` and is already cheap;
    /// forcing the ts index there would pessimize it, so the hint is dropped
    /// even though `prefer_ts_index` is set.
    #[test]
    fn session_filter_does_not_force_ts_index() {
        let q = Query {
            since: Some("2026-06-16T00:00:00.000Z".into()),
            session_id: Some("abc".into()),
            ..Query::default()
        };
        let (sql, _) = build_select_sql("turns", &q, turns_shape(true));
        assert!(!sql.contains("INDEXED BY"), "got: {sql}");
    }

    /// No `ts` bound → nothing to seek on → no hint (forcing the index with no
    /// `ts` predicate would be invalid).
    #[test]
    fn no_ts_bound_does_not_force_index() {
        let q = Query::default();
        let (sql, _) = build_select_sql("turns", &q, turns_shape(true));
        assert!(!sql.contains("INDEXED BY"), "got: {sql}");
    }

    /// Callers that don't opt in (`prefer_ts_index: false`, e.g. the
    /// session-IN path and every non-`turns` table) never get the hint.
    #[test]
    fn opt_out_never_forces_index() {
        let q = Query {
            since: Some("2026-06-16T00:00:00.000Z".into()),
            ..Query::default()
        };
        let (sql, _) = build_select_sql("turns", &q, turns_shape(false));
        assert!(!sql.contains("INDEXED BY"), "got: {sql}");
    }
}
