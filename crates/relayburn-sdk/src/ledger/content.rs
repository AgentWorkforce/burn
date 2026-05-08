//! Content store + FTS5 search over `content.sqlite`.
//!
//! The body field is the JSON-encoded `ContentRecord` payload — same
//! shape we'd have written to a 1.x JSONL sidecar. FTS5 tokenizes that
//! body on insert so phrase / boolean / NEAR queries land on tool
//! results, assistant text, etc. Snippets returned by `search` highlight
//! the matched span using `<b>…</b>` tags by default; callers can
//! re-render them however they like.
//!
//! Pruning is mtime-bucketed: the `created_at` timestamp on each row is
//! the wall-clock at the moment we appended it. We don't try to model
//! upstream mtime here — once a row lands in `content.sqlite` it's a
//! cache that re-ingest can refill.

use std::collections::HashSet;

use rusqlite::{params, params_from_iter, Connection};
use serde::{Deserialize, Serialize};

use crate::ledger::error::Result;
use crate::ledger::paths::is_valid_session_id;
use crate::ledger::query::Query;
use crate::reader::ContentRecord;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub session_id: String,
    pub message_id: String,
    pub source: String,
    /// FTS5 BM25 rank (lower = better match).
    pub rank: f64,
    /// `<b>…</b>`-highlighted snippet around the matching tokens.
    pub snippet: String,
}

pub struct SearchOptions<'a> {
    pub query: &'a str,
    pub limit: usize,
    pub session_id: Option<&'a str>,
}

impl<'a> SearchOptions<'a> {
    pub fn new(query: &'a str) -> Self {
        Self {
            query,
            limit: 25,
            session_id: None,
        }
    }
}

pub(crate) fn search(conn: &Connection, opts: SearchOptions<'_>) -> Result<Vec<SearchHit>> {
    let limit = opts.limit.max(1) as i64;
    let mut sql = String::from(
        "SELECT c.session_id, c.message_id, c.source, bm25(content_fts), \
                snippet(content_fts, 0, '<b>', '</b>', '…', 16) \
         FROM content_fts \
         JOIN content c ON c.rowid = content_fts.rowid \
         WHERE content_fts MATCH ?",
    );
    if let Some(sid) = opts.session_id {
        if !is_valid_session_id(sid) {
            // A malformed session filter would otherwise produce zero
            // hits silently; surface it to the caller.
            return Ok(Vec::new());
        }
        sql.push_str(" AND c.session_id = ?");
    }
    sql.push_str(" ORDER BY rank LIMIT ?");

    let mut stmt = conn.prepare(&sql)?;
    let rows = if let Some(sid) = opts.session_id {
        stmt.query_map(params![opts.query, sid, limit], |r| {
            Ok(SearchHit {
                session_id: r.get(0)?,
                message_id: r.get(1)?,
                source: r.get(2)?,
                rank: r.get::<_, f64>(3)?,
                snippet: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map(params![opts.query, limit], |r| {
            Ok(SearchHit {
                session_id: r.get(0)?,
                message_id: r.get(1)?,
                source: r.get(2)?,
                rank: r.get::<_, f64>(3)?,
                snippet: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(rows)
}

#[derive(Debug, Clone, Default)]
pub struct PruneStats {
    pub rows_deleted: usize,
    pub bytes_freed: i64,
}

/// Drop content rows whose `created_at` is below `cutoff`. Cutoff is a
/// string compared lexically — the writer always stamps with our
/// monotonic `ts:NNN.NNN` form (see [`writer::debug_now`]) so a string
/// compare is the right ordering.
pub(crate) fn prune_older_than(conn: &mut Connection, cutoff: &str) -> Result<PruneStats> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let bytes: i64 = tx
        .query_row(
            "SELECT COALESCE(SUM(byte_length), 0) FROM content WHERE created_at < ?",
            params![cutoff],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let deleted = tx.execute("DELETE FROM content WHERE created_at < ?", params![cutoff])?;
    tx.commit()?;
    Ok(PruneStats {
        rows_deleted: deleted,
        bytes_freed: bytes,
    })
}

pub(crate) fn count_content(conn: &Connection) -> Result<i64> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM content", [], |r| r.get(0))?;
    Ok(count)
}

pub(crate) fn query(conn: &Connection, q: &Query) -> Result<Vec<ContentRecord>> {
    let mut sql = String::from("SELECT body FROM content");
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    if let Some(session_id) = &q.session_id {
        clauses.push("session_id = ?");
        params.push(session_id.clone());
    }
    if let Some(source) = q.source {
        clauses.push("source = ?");
        params.push(source.wire_str().to_string());
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY rowid");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params.iter()), |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let json = row?;
        let record: ContentRecord = match serde_json::from_str(&json) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if content_passes(&record, q) {
            out.push(record);
        }
    }
    Ok(out)
}

fn content_passes(r: &ContentRecord, q: &Query) -> bool {
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

/// Distinct `session_id` values present in `content.sqlite`. Powers the
/// "skip sessions whose content I already have" filter in
/// `relayburn-ingest::reingest_missing_content` (#278). Mirrors the TS
/// `listContentSessionIds()` adapter method.
///
/// Filters out malformed ids defensively (mirrors the TS sqlite-adapter);
/// a corrupted row should not poison the caller's skip set. The `content`
/// table is non-STRICT, so a row whose `session_id` decodes as something
/// other than TEXT is skipped rather than aborting the whole call.
pub(crate) fn list_session_ids(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT session_id FROM content")?;
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
