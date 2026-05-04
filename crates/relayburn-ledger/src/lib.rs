//! `relayburn-ledger` — Rust port of `@relayburn/ledger`.
//!
//! Supersedes #243's literal port. The 2.0 design is two SQLite databases
//! (events + stamps; content + FTS5) with no JSONL files, no hash
//! sidecars, and no user-space file locks. See AgentWorkforce/burn#259
//! for the full rationale.
//!
//! ```no_run
//! use relayburn_ledger::Ledger;
//! let ledger = Ledger::open_default().unwrap();
//! // append turns / compactions / stamps / content via Ledger methods.
//! ```

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

use std::path::{Path, PathBuf};

use rusqlite::params;

pub use crate::content::{PruneStats, SearchHit, SearchOptions};
pub use crate::error::{LedgerError, Result};
pub use crate::fingerprint::{
    compaction_id_fingerprint, content_blob_fingerprint, relationship_id_fingerprint,
    tool_result_event_id_fingerprint, turn_content_fingerprint, turn_id_fingerprint,
    user_turn_id_fingerprint,
};
pub use crate::paths::{
    burn_sqlite_path, content_sqlite_path, is_valid_session_id, ledger_home,
};
pub use crate::query::Query;
pub use crate::reader::EnrichedTurn;
pub use crate::schema::{DERIVABLE_TABLES, FIRST_PARTY_TABLES, SCHEMA_VERSION};
pub use crate::stamp::{
    stamp_matches, Enrichment, MessageRange, Stamp, StampError, StampSelector,
};

use crate::db::Connections;

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

    pub fn append_turns(&mut self, turns: &[relayburn_reader::TurnRecord]) -> Result<usize> {
        writer::append_turns(&mut self.conns.burn, turns)
    }

    pub fn append_compactions(
        &mut self,
        events: &[relayburn_reader::CompactionEvent],
    ) -> Result<usize> {
        writer::append_compactions(&mut self.conns.burn, events)
    }

    pub fn append_relationships(
        &mut self,
        records: &[relayburn_reader::SessionRelationshipRecord],
    ) -> Result<usize> {
        writer::append_relationships(&mut self.conns.burn, records)
    }

    pub fn append_tool_result_events(
        &mut self,
        records: &[relayburn_reader::ToolResultEventRecord],
    ) -> Result<usize> {
        writer::append_tool_result_events(&mut self.conns.burn, records)
    }

    pub fn append_user_turns(
        &mut self,
        records: &[relayburn_reader::UserTurnRecord],
    ) -> Result<usize> {
        writer::append_user_turns(&mut self.conns.burn, records)
    }

    pub fn append_stamp(&mut self, stamp: &Stamp) -> Result<()> {
        writer::append_stamp(&mut self.conns.burn, stamp)
    }

    pub fn append_content(
        &mut self,
        records: &[relayburn_reader::ContentRecord],
    ) -> Result<usize> {
        writer::append_content(&mut self.conns.content, records)
    }

    // --- query paths --------------------------------------------------

    pub fn query_turns(&self, q: &Query) -> Result<Vec<EnrichedTurn>> {
        reader::query_turns(&self.conns.burn, q)
    }

    pub fn query_compactions(
        &self,
        q: &Query,
    ) -> Result<Vec<relayburn_reader::CompactionEvent>> {
        reader::query_compactions(&self.conns.burn, q)
    }

    pub fn query_relationships(
        &self,
        q: &Query,
    ) -> Result<Vec<relayburn_reader::SessionRelationshipRecord>> {
        reader::query_relationships(&self.conns.burn, q)
    }

    pub fn query_tool_result_events(
        &self,
        q: &Query,
    ) -> Result<Vec<relayburn_reader::ToolResultEventRecord>> {
        reader::query_tool_result_events(&self.conns.burn, q)
    }

    pub fn query_user_turns(
        &self,
        q: &Query,
    ) -> Result<Vec<relayburn_reader::UserTurnRecord>> {
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
        for kind in ["turn", "compaction", "relationship", "tool_result_event", "user_turn"] {
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

        // Wipe content + the FTS index. DELETE on `content` triggers
        // `content_fts_ad` per row; for a full wipe we instead bypass
        // the trigger with a direct delete on `content_fts` so we don't
        // pay O(rows) tokenization just to re-empty the index.
        let content_count = content::count_content(&self.conns.content)?;
        self.conns.content.execute("DELETE FROM content", [])?;
        self.conns
            .content
            .execute("INSERT INTO content_fts(content_fts) VALUES('rebuild')", [])?;

        Ok(RebuildSummary {
            rows_dropped: rows_dropped as usize,
            content_rows_dropped: content_count as usize,
        })
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildSummary {
    pub rows_dropped: usize,
    pub content_rows_dropped: usize,
}

/// Convenience: layout describing where a `Ledger` will land. Callers
/// that want test isolation construct one with `under()` and pass the
/// paths to [`Ledger::open`].
pub struct LedgerLayout {
    pub home: PathBuf,
    pub burn: PathBuf,
    pub content: PathBuf,
}

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
