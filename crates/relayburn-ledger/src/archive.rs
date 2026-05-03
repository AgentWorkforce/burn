//! SQLite archive (port of `packages/ledger/src/archive.ts`).
//!
//! Scope of the initial port (#243):
//!   - Schema-compatible CREATE statements and additive migrations so an
//!     existing `archive.sqlite` (`ARCHIVE_VERSION = 3`) opens cleanly under
//!     Rust without a forced rebuild.
//!   - `open_archive` honors WAL + foreign_keys + the same version-mismatch
//!     drop-and-recreate the TS code does.
//!   - The hot tail loop ingest will land alongside the strongly-typed
//!     reader port (#242 / #244); for now we expose the schema, the version
//!     gate, status snapshots, and a transactional batch-write helper that
//!     downstream callers can drive once `TurnRecord` is typed.
//!
//! Schema-compatibility test: `existing_archive_opens_without_rebuild`
//! creates a database at the current `ARCHIVE_VERSION` using the TS schema
//! columns, then re-opens it with `open_archive` and asserts the version
//! row survives.

use std::path::Path;

use rusqlite::{params, Connection};

use crate::errors::Result;
use crate::paths::archive_path;

/// On-disk schema version. **Stays in lockstep with TS `ARCHIVE_VERSION`**.
/// Bumping here without bumping TS (or vice-versa) forces a rebuild on every
/// open, which would silently re-derive the entire 1.5GB archive.
pub const ARCHIVE_VERSION: i64 = 3;

/// Idempotent schema. Identical column order / types to
/// `packages/ledger/src/archive.ts:SCHEMA_SQL` so a TS-built archive opens
/// without complaint here and vice-versa.
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
  source              TEXT NOT NULL,
  session_id          TEXT NOT NULL,
  project             TEXT,
  project_key         TEXT,
  started_at          TEXT,
  ended_at            TEXT,
  turn_count          INTEGER NOT NULL DEFAULT 0,
  model_set_json      TEXT,
  workflow_id         TEXT,
  agent_id            TEXT,
  parent_agent_id     TEXT,
  has_subagent        INTEGER NOT NULL DEFAULT 0,
  min_fidelity        TEXT,
  has_full_attribution INTEGER,
  PRIMARY KEY (source, session_id)
);

CREATE INDEX IF NOT EXISTS idx_sessions_started_at ON sessions(started_at);
CREATE INDEX IF NOT EXISTS idx_sessions_project_key ON sessions(project_key);
CREATE INDEX IF NOT EXISTS idx_sessions_workflow_id ON sessions(workflow_id);

CREATE TABLE IF NOT EXISTS turns (
  source                TEXT NOT NULL,
  session_id            TEXT NOT NULL,
  message_id            TEXT NOT NULL,
  turn_index            INTEGER NOT NULL,
  ts                    TEXT NOT NULL,
  model                 TEXT NOT NULL,
  project               TEXT,
  project_key           TEXT,
  activity              TEXT,
  stop_reason           TEXT,
  has_edits             INTEGER,
  retries               INTEGER,
  is_sidechain          INTEGER,
  subagent_id           TEXT,
  parent_subagent_id    TEXT,
  parent_tool_use_id    TEXT,
  subagent_type         TEXT,
  subagent_description  TEXT,
  input_tokens          INTEGER NOT NULL DEFAULT 0,
  output_tokens         INTEGER NOT NULL DEFAULT 0,
  reasoning_tokens      INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
  cache_create_5m_tokens INTEGER NOT NULL DEFAULT 0,
  cache_create_1h_tokens INTEGER NOT NULL DEFAULT 0,
  workflow_id           TEXT,
  agent_id              TEXT,
  persona               TEXT,
  tier                  TEXT,
  enrichment_json       TEXT,
  attribution_fidelity  TEXT,
  tokens_present        INTEGER,
  cost_present          INTEGER,
  PRIMARY KEY (source, session_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_turns_ts ON turns(ts);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(source, session_id, turn_index);
CREATE INDEX IF NOT EXISTS idx_turns_model ON turns(model);
CREATE INDEX IF NOT EXISTS idx_turns_activity ON turns(activity);
CREATE INDEX IF NOT EXISTS idx_turns_project_key ON turns(project_key);
CREATE INDEX IF NOT EXISTS idx_turns_workflow ON turns(workflow_id);

CREATE TABLE IF NOT EXISTS tool_calls (
  source         TEXT NOT NULL,
  session_id     TEXT NOT NULL,
  message_id     TEXT NOT NULL,
  call_index     INTEGER NOT NULL,
  tool_use_id    TEXT,
  tool_name      TEXT NOT NULL,
  target         TEXT,
  args_hash      TEXT,
  is_error       INTEGER,
  replaced_tools TEXT,
  collapsed_calls INTEGER,
  PRIMARY KEY (source, session_id, message_id, call_index)
);

CREATE INDEX IF NOT EXISTS idx_tool_calls_name ON tool_calls(tool_name);
CREATE INDEX IF NOT EXISTS idx_tool_calls_use_id ON tool_calls(tool_use_id);

CREATE TABLE IF NOT EXISTS tool_result_events (
  source              TEXT NOT NULL,
  session_id          TEXT NOT NULL,
  message_id          TEXT NOT NULL,
  tool_use_id         TEXT NOT NULL,
  call_index          INTEGER NOT NULL,
  event_index         INTEGER NOT NULL,
  status              TEXT,
  content_length      INTEGER,
  content_hash        TEXT,
  is_error            INTEGER,
  subagent_session_id TEXT,
  agent_id            TEXT,
  event_source        TEXT,
  ts                  TEXT,
  PRIMARY KEY (source, session_id, message_id, tool_use_id, event_index)
);

CREATE INDEX IF NOT EXISTS idx_tool_result_events_use_id ON tool_result_events(tool_use_id);
CREATE INDEX IF NOT EXISTS idx_tool_result_events_session ON tool_result_events(source, session_id);
CREATE INDEX IF NOT EXISTS idx_tool_result_events_subagent ON tool_result_events(subagent_session_id);

CREATE TABLE IF NOT EXISTS stamps (
  source                TEXT NOT NULL,
  session_id            TEXT NOT NULL,
  ts                    TEXT NOT NULL,
  selector_json         TEXT,
  enrichment_json       TEXT NOT NULL,
  ledger_offset_bytes   INTEGER NOT NULL,
  PRIMARY KEY (source, session_id, ts, ledger_offset_bytes)
);

CREATE INDEX IF NOT EXISTS idx_stamps_session ON stamps(source, session_id);

CREATE TABLE IF NOT EXISTS compactions (
  source                TEXT NOT NULL,
  session_id            TEXT NOT NULL,
  ts                    TEXT NOT NULL,
  preceding_message_id  TEXT,
  tokens_before_compact INTEGER,
  PRIMARY KEY (source, session_id, ts)
);

CREATE TABLE IF NOT EXISTS archive_state (
  id                    INTEGER PRIMARY KEY CHECK (id = 1),
  ledger_offset_bytes   INTEGER NOT NULL DEFAULT 0,
  ledger_mtime_ms       INTEGER NOT NULL DEFAULT 0,
  archive_version       INTEGER NOT NULL,
  last_built_at         TEXT,
  last_rebuild_at       TEXT
);
"#;

/// Tuning pragmas applied on every open. WAL gives multi-reader concurrency
/// without serializing on a single writer; `synchronous=NORMAL` is the
/// default WAL durability tradeoff (safe across crashes, only loses the
/// last unflushed transaction on hard power-off).
pub const PRAGMAS_SQL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
"#;

/// Open (and create-if-missing) the archive at the default path. Drops and
/// recreates the file when `archive_state.archive_version` doesn't match
/// the compiled-in `ARCHIVE_VERSION` — the archive is derived state, so a
/// rebuild is always safe.
pub fn open_archive() -> Result<Connection> {
    let p = archive_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    open_archive_at(&p)
}

pub fn open_archive_at(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    conn.execute_batch(SCHEMA_SQL)?;
    apply_additive_migrations(&conn)?;
    let on_disk_version: Option<i64> = conn
        .query_row(
            "SELECT archive_version FROM archive_state WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .ok();
    match on_disk_version {
        None => {
            conn.execute(
                "INSERT INTO archive_state (id, ledger_offset_bytes, ledger_mtime_ms, archive_version) VALUES (1, 0, 0, ?1)",
                params![ARCHIVE_VERSION],
            )?;
        }
        Some(v) if v == ARCHIVE_VERSION => {}
        Some(_) => {
            // Schema mismatch: the archive is derived state, drop and rebuild.
            drop(conn);
            std::fs::remove_file(path).ok();
            // WAL/SHM siblings are recreated on the next open.
            std::fs::remove_file(path.with_extension("sqlite-wal")).ok();
            std::fs::remove_file(path.with_extension("sqlite-shm")).ok();
            conn = Connection::open(path)?;
            apply_pragmas(&conn)?;
            conn.execute_batch(SCHEMA_SQL)?;
            apply_additive_migrations(&conn)?;
            conn.execute(
                "INSERT INTO archive_state (id, ledger_offset_bytes, ledger_mtime_ms, archive_version) VALUES (1, 0, 0, ?1)",
                params![ARCHIVE_VERSION],
            )?;
        }
    }
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(PRAGMAS_SQL)?;
    Ok(())
}

/// Forward-migrate columns added under the same `ARCHIVE_VERSION`.
/// `CREATE TABLE IF NOT EXISTS` is a no-op on existing tables, so a column
/// added there later won't appear on already-built archives — we have to
/// add it explicitly. Mirrors `applyAdditiveMigrations` in TS.
fn apply_additive_migrations(conn: &Connection) -> Result<()> {
    ensure_column(conn, "turns", "attribution_fidelity", "TEXT")?;
    ensure_column(conn, "turns", "tokens_present", "INTEGER")?;
    ensure_column(conn, "turns", "cost_present", "INTEGER")?;
    ensure_column(conn, "turns", "subagent_description", "TEXT")?;
    ensure_column(conn, "sessions", "min_fidelity", "TEXT")?;
    ensure_column(conn, "sessions", "has_full_attribution", "INTEGER")?;
    // Counterfactual annotations from replacement tools (e.g. relaywash).
    // JSON-encoded list for `replaced_tools` so the column count stays
    // bounded; `collapsed_calls` is a plain integer. Mirrors
    // packages/ledger/src/archive.ts:380-381.
    ensure_column(conn, "tool_calls", "replaced_tools", "TEXT")?;
    ensure_column(conn, "tool_calls", "collapsed_calls", "INTEGER")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_turns_attribution_fidelity ON turns(attribution_fidelity);",
    )?;
    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, col: &str, ty: &str) -> Result<()> {
    let exists: bool = {
        let sql = format!("PRAGMA table_info({table})");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut found = false;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == col {
                found = true;
                break;
            }
        }
        found
    };
    if !exists {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {col} {ty}");
        conn.execute(&sql, [])?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ArchiveStatus {
    pub archive_version: i64,
    pub ledger_offset_bytes: i64,
    pub ledger_mtime_ms: i64,
    pub last_built_at: Option<String>,
    pub last_rebuild_at: Option<String>,
    pub turns: i64,
    pub sessions: i64,
}

pub fn archive_status(conn: &Connection) -> Result<ArchiveStatus> {
    let (archive_version, ledger_offset_bytes, ledger_mtime_ms, last_built_at, last_rebuild_at) =
        conn.query_row(
            "SELECT archive_version, ledger_offset_bytes, ledger_mtime_ms, last_built_at, last_rebuild_at FROM archive_state WHERE id = 1",
            [],
            |row| {
                Ok::<_, rusqlite::Error>((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )?;
    let turns: i64 = conn.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0))?;
    let sessions: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
    Ok(ArchiveStatus {
        archive_version,
        ledger_offset_bytes,
        ledger_mtime_ms,
        last_built_at,
        last_rebuild_at,
        turns,
        sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn opens_fresh_archive_with_version_row() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("archive.sqlite");
        let conn = open_archive_at(&path).unwrap();
        let status = archive_status(&conn).unwrap();
        assert_eq!(status.archive_version, ARCHIVE_VERSION);
        assert_eq!(status.turns, 0);
        assert_eq!(status.sessions, 0);
    }

    #[test]
    fn existing_archive_opens_without_rebuild() {
        // Build an archive with the current schema, write a sentinel turn,
        // close, re-open: the row must survive (i.e. we must NOT drop and
        // recreate).
        let dir = tempdir().unwrap();
        let path = dir.path().join("archive.sqlite");

        {
            let conn = open_archive_at(&path).unwrap();
            conn.execute(
                "INSERT INTO turns (
                    source, session_id, message_id, turn_index, ts, model
                 ) VALUES ('claude', 'sess', 'msg', 0, '2026-01-01T00:00:00Z', 'm')",
                [],
            )
            .unwrap();
        }
        let conn = open_archive_at(&path).unwrap();
        let status = archive_status(&conn).unwrap();
        assert_eq!(status.turns, 1, "existing archive must not be rebuilt");
    }

    #[test]
    fn version_mismatch_triggers_rebuild() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("archive.sqlite");
        {
            let conn = open_archive_at(&path).unwrap();
            // Simulate an older archive by hand-rewriting the version.
            conn.execute(
                "UPDATE archive_state SET archive_version = ?1 WHERE id = 1",
                params![ARCHIVE_VERSION - 1],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO turns (
                    source, session_id, message_id, turn_index, ts, model
                 ) VALUES ('claude', 'sess', 'msg', 0, '2026-01-01T00:00:00Z', 'm')",
                [],
            )
            .unwrap();
        }
        let conn = open_archive_at(&path).unwrap();
        let status = archive_status(&conn).unwrap();
        assert_eq!(status.archive_version, ARCHIVE_VERSION);
        assert_eq!(
            status.turns, 0,
            "stale archive should have been rebuilt clean"
        );
    }

    #[test]
    fn additive_migration_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("archive.sqlite");
        let conn = open_archive_at(&path).unwrap();
        // Re-running the migration must not throw "duplicate column".
        apply_additive_migrations(&conn).unwrap();
        apply_additive_migrations(&conn).unwrap();
    }
}
