//! `relayburn-ledger` — Rust port of `@relayburn/ledger`.
//!
//! Supersedes #243's literal port. The 2.0 design is two SQLite databases
//! (events + stamps; content + FTS5) with no JSONL files, no hash
//! sidecars, and no user-space file locks. See AgentWorkforce/burn#259
//! for the full rationale.
//!
//! ```no_run
//! use relayburn_sdk::RawLedger;
//! let ledger = RawLedger::open_default().unwrap();
//! // append turns / compactions / stamps / content via Ledger methods.
//! ```

mod bootstrap;
mod config;
mod content;
mod db;
mod error;
mod fingerprint;
mod paths;
mod query;
mod reader;
mod schema;
mod stamp;
mod writer;

use std::collections::HashSet;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use rusqlite::params;

pub use crate::ledger::config::{
    config_path, config_path_at_home, load_config, load_config_at, load_config_with_home,
    BurnConfig, ContentConfig, Retention, DEFAULT_RETENTION_DAYS,
};
pub use crate::ledger::content::{PruneStats, SearchHit, SearchOptions};
pub use crate::ledger::error::{LedgerError, Result};
pub use crate::ledger::paths::{
    burn_sqlite_path, content_sqlite_path, is_valid_session_id, ledger_home,
};
pub use crate::ledger::query::Query;
pub use crate::ledger::reader::EnrichedTurn;
pub use crate::ledger::stamp::{Enrichment, MessageRange, Stamp, StampError, StampSelector};

use crate::ledger::schema::DERIVABLE_TABLES;

use crate::ledger::db::Connections;

/// Owning handle on the two SQLite databases. Holds open connections,
/// applies DDL on first open, and exposes append / query / search / state
/// verbs.
///
/// Not `Sync`. Wrap in a `Mutex` if you want to share across threads;
/// the WAL gives you concurrent reads from separate `Ledger` instances
/// pointing at the same files.
pub struct Ledger {
    conns: Connections,
}

impl Ledger {
    /// Open with default paths from `RELAYBURN_HOME`.
    pub fn open_default() -> Result<Self> {
        Self::open(&burn_sqlite_path(), &content_sqlite_path())
    }

    /// Open at the given paths. Creates parent directories if missing
    /// and applies the DDL idempotently.
    pub fn open(burn_path: &Path, content_path: &Path) -> Result<Self> {
        Ok(Self {
            conns: Connections::open(burn_path, content_path)?,
        })
    }

    pub fn burn_path(&self) -> &Path {
        &self.conns.burn_path
    }

    pub fn content_path(&self) -> &Path {
        &self.conns.content_path
    }

    // --- append paths -------------------------------------------------

    pub fn append_turns(&mut self, turns: &[crate::reader::TurnRecord]) -> Result<usize> {
        writer::append_turns(&mut self.conns.burn, turns)
    }

    pub fn append_compactions(
        &mut self,
        events: &[crate::reader::CompactionEvent],
    ) -> Result<usize> {
        writer::append_compactions(&mut self.conns.burn, events)
    }

    pub fn append_relationships(
        &mut self,
        records: &[crate::reader::SessionRelationshipRecord],
    ) -> Result<usize> {
        writer::append_relationships(&mut self.conns.burn, records)
    }

    pub fn append_tool_result_events(
        &mut self,
        records: &[crate::reader::ToolResultEventRecord],
    ) -> Result<usize> {
        writer::append_tool_result_events(&mut self.conns.burn, records)
    }

    pub fn append_user_turns(
        &mut self,
        records: &[crate::reader::UserTurnRecord],
    ) -> Result<usize> {
        writer::append_user_turns(&mut self.conns.burn, records)
    }

    pub fn append_stamp(&mut self, stamp: &Stamp) -> Result<()> {
        writer::append_stamp(&mut self.conns.burn, stamp)
    }

    pub fn append_content(&mut self, records: &[crate::reader::ContentRecord]) -> Result<usize> {
        writer::append_content(&mut self.conns.content, records)
    }

    // --- query paths --------------------------------------------------

    pub fn query_turns(&self, q: &Query) -> Result<Vec<EnrichedTurn>> {
        reader::query_turns(&self.conns.burn, q)
    }

    pub fn query_compactions(&self, q: &Query) -> Result<Vec<crate::reader::CompactionEvent>> {
        reader::query_compactions(&self.conns.burn, q)
    }

    pub fn query_relationships(
        &self,
        q: &Query,
    ) -> Result<Vec<crate::reader::SessionRelationshipRecord>> {
        reader::query_relationships(&self.conns.burn, q)
    }

    pub fn query_tool_result_events(
        &self,
        q: &Query,
    ) -> Result<Vec<crate::reader::ToolResultEventRecord>> {
        reader::query_tool_result_events(&self.conns.burn, q)
    }

    pub fn query_user_turns(&self, q: &Query) -> Result<Vec<crate::reader::UserTurnRecord>> {
        reader::query_user_turns(&self.conns.burn, q)
    }

    pub fn list_stamps(&self) -> Result<Vec<Stamp>> {
        reader::list_stamps(&self.conns.burn)
    }

    pub fn count_table(&self, table: &str) -> Result<i64> {
        reader::count_table(&self.conns.burn, table)
    }

    pub fn count_content(&self) -> Result<i64> {
        content::count_content(&self.conns.content)
    }

    pub fn query_content(&self, q: &Query) -> Result<Vec<crate::reader::ContentRecord>> {
        content::query(&self.conns.content, q)
    }

    /// Distinct session ids that have at least one content row in
    /// `content.sqlite`. Powers the skip filter in
    /// `relayburn-ingest::reingest_missing_content` (#278).
    pub fn list_content_session_ids(&self) -> Result<HashSet<String>> {
        content::list_session_ids(&self.conns.content)
    }

    /// Distinct session ids that have at least one user-turn row in
    /// `burn.sqlite`. Sibling of [`Self::list_content_session_ids`];
    /// `relayburn-ingest::reingest_missing_content` AND-combines the two
    /// to decide whether a session is fully covered or still needs a
    /// re-parse.
    pub fn list_user_turn_session_ids(&self) -> Result<HashSet<String>> {
        reader::list_user_turn_session_ids(&self.conns.burn)
    }

    // --- content + FTS5 ----------------------------------------------

    pub fn search_content(&self, opts: SearchOptions<'_>) -> Result<Vec<SearchHit>> {
        content::search(&self.conns.content, opts)
    }

    pub fn prune_content_older_than(&mut self, cutoff: &str) -> Result<PruneStats> {
        content::prune_older_than(&mut self.conns.content, cutoff)
    }

    // --- export ------------------------------------------------------

    /// Stream every event row as one JSONL line in the form
    /// `{"v":1,"kind":"turn","record":…}`. The output is byte-equivalent
    /// to what a 1.x JSONL ledger would have written for the same set of
    /// records, sufficient for `jq`/`grep` debugging.
    pub fn export_ledger_jsonl<W: std::io::Write>(&self, w: &mut W) -> Result<()> {
        for kind in [
            "turn",
            "compaction",
            "relationship",
            "tool_result_event",
            "user_turn",
        ] {
            let table = match kind {
                "turn" => "turns",
                "compaction" => "compactions",
                "relationship" => "relationships",
                "tool_result_event" => "tool_result_events",
                "user_turn" => "user_turns",
                _ => unreachable!(),
            };
            let rows = reader::raw_record_jsons(&self.conns.burn, table)?;
            for json in rows {
                writeln!(w, r#"{{"v":1,"kind":"{kind}","record":{json}}}"#)?;
            }
        }
        Ok(())
    }

    /// Stream every stamp row as a JSONL line. `burn stamps export`
    /// uses this for backup / version-control workflows.
    pub fn export_stamps_jsonl<W: std::io::Write>(&self, w: &mut W) -> Result<()> {
        for stamp in self.list_stamps()? {
            let line = serde_json::json!({
                "v": 1,
                "kind": "stamp",
                "ts": stamp.ts,
                "selector": stamp.selector,
                "enrichment": stamp.enrichment,
            });
            writeln!(w, "{}", line)?;
        }
        Ok(())
    }

    // --- state rebuild -----------------------------------------------

    /// Drop the derivable tables in `burn.sqlite` and the entire
    /// `content.sqlite`, then re-create them empty. Stamps, archive
    /// state, and ingest cursors are preserved.
    ///
    /// Returns the path to the (now-empty) content DB so the caller can
    /// move on to re-ingest from upstream files. Re-ingest is the
    /// caller's responsibility — `relayburn-ingest` (#245) drives it
    /// against the discovery layer.
    pub fn rebuild_derivable(&mut self) -> Result<RebuildSummary> {
        let mut rows_dropped = 0i64;
        for table in DERIVABLE_TABLES {
            let count: i64 = self
                .conns
                .burn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap_or(0);
            rows_dropped += count;
            self.conns
                .burn
                .execute(&format!("DELETE FROM {table}"), [])?;
        }
        // Replay stamp-synthesized relationships. `relationships` is a
        // derivable table (the upstream session log is the source of
        // truth) and was dropped above, but stamp-synthesized rows
        // live and die with the stamp itself — `append_stamp` produced
        // them by reading the stamp's `parentAgentId` enrichment, and
        // since stamps are first-party data that survives rebuild we
        // need to re-emit those edges here. Without this replay,
        // subagent parent/child queries would see incomplete graphs
        // until callers happened to re-write each stamp.
        let stamps = reader::list_stamps(&self.conns.burn)?;
        let synthesized: Vec<_> = stamps
            .iter()
            .filter_map(writer::synthesize_relationship)
            .collect();
        if !synthesized.is_empty() {
            writer::append_relationships(&mut self.conns.burn, &synthesized)?;
        }

        let now = writer::debug_now();
        self.conns.burn.execute(
            "UPDATE archive_state SET last_rebuild_at = ? WHERE id = 1",
            params![now],
        )?;

        // Wipe content + the FTS index. The straightforward
        // `DELETE FROM content` would fire `content_fts_ad` once per
        // row and pay tokenization-heavy work for an index we're about
        // to rebuild anyway, so we drop the sync triggers, bulk-delete,
        // recreate the triggers, then issue a single FTS5 `rebuild`.
        let content_count = content::count_content(&self.conns.content)?;
        self.conns.content.execute_batch(
            "DROP TRIGGER IF EXISTS content_fts_ad;
             DROP TRIGGER IF EXISTS content_fts_au;
             DELETE FROM content;
             CREATE TRIGGER content_fts_ad AFTER DELETE ON content BEGIN
                 INSERT INTO content_fts(content_fts, rowid, body) VALUES('delete', old.rowid, old.body);
             END;
             CREATE TRIGGER content_fts_au AFTER UPDATE ON content BEGIN
                 INSERT INTO content_fts(content_fts, rowid, body) VALUES('delete', old.rowid, old.body);
                 INSERT INTO content_fts(rowid, body) VALUES (new.rowid, new.body);
             END;
             INSERT INTO content_fts(content_fts) VALUES('rebuild');",
        )?;

        Ok(RebuildSummary {
            rows_dropped: rows_dropped as usize,
            content_rows_dropped: content_count as usize,
        })
    }

    /// Snapshot the single-row `archive_state` table as a JSON object —
    /// `{ schema_version, upstream_cursors_json, last_built_at,
    /// last_rebuild_at }`. Powers `state_status`'s `archive` block; kept
    /// here rather than at the SDK verb so callers don't have to bind
    /// to rusqlite directly to read first-party rows.
    pub fn read_archive_state_json(&self) -> Result<String> {
        let row: (i64, String, Option<String>, Option<String>) = self.conns.burn.query_row(
            "SELECT schema_version, upstream_cursors_json, last_built_at, last_rebuild_at \
             FROM archive_state WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let value = serde_json::json!({
            "schema_version": row.0,
            "upstream_cursors_json": row.1,
            "last_built_at": row.2,
            "last_rebuild_at": row.3,
        });
        Ok(value.to_string())
    }

    /// Directly access the `archive_state.upstream_cursors_json` blob.
    /// Cursors are caller-defined JSON; we just round-trip the string.
    pub fn read_cursors(&self) -> Result<String> {
        let cursors: String = self.conns.burn.query_row(
            "SELECT upstream_cursors_json FROM archive_state WHERE id = 1",
            [],
            |r| r.get(0),
        )?;
        Ok(cursors)
    }

    pub fn write_cursors(&mut self, cursors_json: &str) -> Result<()> {
        let now = writer::debug_now();
        self.conns.burn.execute(
            "UPDATE archive_state
             SET upstream_cursors_json = ?,
                 last_built_at = ?
             WHERE id = 1",
            params![cursors_json, now],
        )?;
        Ok(())
    }

    /// Vacuum both databases. Useful after a large prune.
    pub fn vacuum(&mut self) -> Result<()> {
        self.conns.burn.execute_batch("VACUUM")?;
        self.conns.content.execute_batch("VACUUM")?;
        Ok(())
    }

    // --- state reset --------------------------------------------------

    /// Count what a `reset()` would delete without mutating either DB.
    /// Powers the dry-run path of `burn state reset` (no `--force`).
    ///
    /// SQL errors propagate via `Result`. A swallowed `unwrap_or(0)` here
    /// would silently report a healthy zero-count dry-run on a corrupt
    /// ledger and mislead operators into treating reset as a safe no-op.
    pub fn count_reset_targets(&self) -> Result<ResetSummary> {
        let mut rows_dropped = 0i64;
        for table in DERIVABLE_TABLES {
            let count: i64 = self.conns.burn.query_row(
                &format!("SELECT COUNT(*) FROM {table}"),
                [],
                |r| r.get(0),
            )?;
            rows_dropped += count;
        }
        let stamps_dropped: i64 = self
            .conns
            .burn
            .query_row("SELECT COUNT(*) FROM stamps", [], |r| r.get(0))?;
        let content_rows_dropped = content::count_content(&self.conns.content)?;
        Ok(ResetSummary {
            rows_dropped: rows_dropped as usize,
            stamps_dropped: stamps_dropped as usize,
            content_rows_dropped: content_rows_dropped as usize,
        })
    }

    /// Wipe **all** derived ledger state, including first-party stamps
    /// and ingest cursors. Stronger than [`Self::rebuild_derivable`],
    /// which preserves stamps and cursors so re-ingest is incremental.
    ///
    /// After `reset()` runs, both DBs are byte-equivalent to a fresh
    /// `Ledger::open` against an empty `$RELAYBURN_HOME`: every
    /// derivable table is empty, `stamps` is empty, `content.sqlite`
    /// is empty (FTS index included), and `archive_state` is reset to
    /// the bootstrap row (`schema_version` preserved, cursors blanked,
    /// `last_built_at` / `last_rebuild_at` cleared).
    ///
    /// Re-ingest is the caller's responsibility; the CLI offers
    /// `burn state reset --force --reingest` as a convenience that
    /// drives `burn ingest` afterwards.
    pub fn reset(&mut self) -> Result<ResetSummary> {
        // Snapshot counts BEFORE we mutate so the returned summary
        // describes what the call deleted, not what's left.
        let summary = self.count_reset_targets()?;

        // Wipe derivable + stamps in a single transaction so an early
        // failure can't leave the events DB half-emptied.
        let tx = self.conns.burn.transaction()?;
        for table in DERIVABLE_TABLES {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.execute("DELETE FROM stamps", [])?;
        // Reset archive_state to the bootstrap shape: keep the row
        // (the CHECK constraint pins id=1) and the schema_version, but
        // blank the cursors + build timestamps so the next ingest walks
        // every upstream file from offset 0.
        tx.execute(
            "UPDATE archive_state \
             SET upstream_cursors_json = '{}', \
                 last_built_at = NULL, \
                 last_rebuild_at = NULL \
             WHERE id = 1",
            [],
        )?;
        tx.commit()?;

        // Wipe content + the FTS index using the same drop-trigger /
        // bulk-delete / rebuild dance as `rebuild_derivable`, so the
        // FTS sync triggers don't pay tokenization cost per row.
        self.conns.content.execute_batch(
            "DROP TRIGGER IF EXISTS content_fts_ad;
             DROP TRIGGER IF EXISTS content_fts_au;
             DELETE FROM content;
             CREATE TRIGGER content_fts_ad AFTER DELETE ON content BEGIN
                 INSERT INTO content_fts(content_fts, rowid, body) VALUES('delete', old.rowid, old.body);
             END;
             CREATE TRIGGER content_fts_au AFTER UPDATE ON content BEGIN
                 INSERT INTO content_fts(content_fts, rowid, body) VALUES('delete', old.rowid, old.body);
                 INSERT INTO content_fts(rowid, body) VALUES (new.rowid, new.body);
             END;
             INSERT INTO content_fts(content_fts) VALUES('rebuild');",
        )?;

        Ok(summary)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildSummary {
    pub rows_dropped: usize,
    pub content_rows_dropped: usize,
}

/// Counts returned by [`Ledger::reset`] (and by the dry-run sibling
/// [`Ledger::count_reset_targets`]). `rows_dropped` covers the
/// derivable events tables; `stamps_dropped` is split out because
/// stamps are first-party data and the CLI surfaces them separately
/// so callers can see what they're about to lose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResetSummary {
    pub rows_dropped: usize,
    pub stamps_dropped: usize,
    pub content_rows_dropped: usize,
}

/// Convenience: layout describing where a `Ledger` will land. Callers
/// that want test isolation construct one with `under()` and pass the
/// paths to [`Ledger::open`].
#[cfg(test)]
pub struct LedgerLayout {
    pub home: PathBuf,
    pub burn: PathBuf,
    pub content: PathBuf,
}

#[cfg(test)]
impl LedgerLayout {
    pub fn under(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let burn = home.join("burn.sqlite");
        let content = home.join("content.sqlite");
        Self {
            home,
            burn,
            content,
        }
    }
}

#[cfg(test)]
mod tests;
