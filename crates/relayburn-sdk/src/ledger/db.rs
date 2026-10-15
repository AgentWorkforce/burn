//! Connection management for the two SQLite databases.
//!
//! The two DBs are opened independently (no PRAGMA-shared connection)
//! and tuned for concurrent ingest + analytic queries:
//!
//! - **WAL** — readers don't block on a writer holding `BEGIN IMMEDIATE`.
//!   This is what lets us drop the user-space file-lock module entirely;
//!   SQLite serializes writers itself.
//! - **`busy_timeout`** — turns transient `SQLITE_BUSY` into an internal
//!   retry loop so callers never see it under normal contention.
//! - **`foreign_keys = ON`** — defensive default; we don't currently
//!   declare FKs but bumping this flag has no cost on tables without one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

use crate::ledger::error::Result;
use crate::ledger::schema::{BURN_DDL, CONTENT_DDL, SCHEMA_VERSION};

const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// A pair of open connections — events DB + content DB. Stored as
/// owned `Connection`s rather than wrapped in a shared mutex so the
/// caller can decide whether to keep the [`Ledger`](crate::Ledger)
/// behind a `Mutex` or per-task instance.
pub(crate) struct Connections {
    pub burn: Connection,
    pub content: Connection,
    pub burn_path: PathBuf,
    pub content_path: PathBuf,
}

impl Connections {
    /// Open both DBs at the given paths, applying PRAGMAs + DDL. The
    /// parent directory of each path must already exist.
    pub fn open(burn_path: &Path, content_path: &Path) -> Result<Self> {
        if let Some(parent) = burn_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        if let Some(parent) = content_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // Snapshot whether a bootstrap is needed BEFORE `Connection::open`
        // creates `burn.sqlite` as a side effect — if we waited, the
        // freshly-created (and newer-than-JSONL) sqlite mtime would
        // always look "current" and we'd skip the rebuild.
        let bootstrap_decision = crate::ledger::bootstrap::decide_bootstrap(burn_path);

        let mut burn = Connection::open(burn_path)?;
        configure_pragmas(&burn)?;
        burn.execute_batch(BURN_DDL)?;
        migrate_burn_schema(&burn)?;
        verify_schema_version(&burn)?;

        // Bootstrap from `ledger.jsonl` sibling if the sqlite mirror is
        // stale or missing. No-op when the JSONL doesn't exist (the
        // SDK is in pure-sqlite mode) or when the sqlite is already
        // current. See `bootstrap.rs` for the rationale.
        crate::ledger::bootstrap::apply_bootstrap(&mut burn, bootstrap_decision)?;

        let content = Connection::open(content_path)?;
        configure_pragmas(&content)?;
        content.execute_batch(CONTENT_DDL)?;

        Ok(Self {
            burn,
            content,
            burn_path: burn_path.to_path_buf(),
            content_path: content_path.to_path_buf(),
        })
    }
}

fn configure_pragmas(conn: &Connection) -> Result<()> {
    // `journal_mode = WAL` returns the new mode as a row; we use
    // `query_row` so the result is consumed (rusqlite's `pragma_update`
    // assumes a no-row pragma and was leaving the WAL switch un-applied
    // on some platforms, which surfaced as `SQLITE_BUSY` under
    // contention because writers were still using rollback-journal
    // semantics).
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// In-place forward migrations for `burn.sqlite`. Re-applying is a no-op so
/// open is idempotent; called BEFORE [`verify_schema_version`] so the
/// version we read reflects the post-migration state.
///
/// Each step in [`MIGRATIONS`] runs when the stored version is below its
/// destination, then records that version. Every step is idempotent so
/// re-running `Ledger::open` after a crash mid-migration picks up where it
/// left off without surfacing `duplicate column name` errors.
fn migrate_burn_schema(conn: &Connection) -> Result<()> {
    let current_version: u32 = conn
        .query_row(
            "SELECT schema_version FROM archive_state WHERE id = 1",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v as u32)
        .unwrap_or(SCHEMA_VERSION);

    for (version, step) in MIGRATIONS {
        if current_version < *version {
            step(conn)?;
            conn.execute(
                "UPDATE archive_state SET schema_version = ? WHERE id = 1",
                [version],
            )?;
        }
    }

    // Partial indexes over back-added columns are created here rather than
    // in the static DDL so a legacy table without the column doesn't fail
    // the DDL pre-pass. By this point every column exists, so each is safe
    // to create idempotently on every open. The predicates keep each index
    // tracking only the rows it serves:
    // - `idx_turns_stop_reason` / `idx_turns_subagent_id`: rows that carry
    //   the field (subagent rows are a tiny minority on most ledgers).
    // - `idx_turns_unaccounted_codex`: Codex rows still awaiting request
    //   accounting, so ingest finds them cheaply and the index empties as
    //   they are re-accounted.
    for ddl in [
        "CREATE INDEX IF NOT EXISTS idx_turns_stop_reason \
         ON turns(stop_reason) WHERE stop_reason IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_turns_subagent_id \
         ON turns(subagent_id) WHERE subagent_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_turns_unaccounted_codex \
         ON turns(session_id) WHERE source = 'codex' AND request_count IS NULL",
    ] {
        conn.execute(ddl, [])?;
    }
    Ok(())
}

type Migration = fn(&Connection) -> Result<()>;

/// Forward steps by destination schema version (see
/// [`crate::ledger::schema::SCHEMA_VERSION`] for what each version is).
const MIGRATIONS: &[(u32, Migration)] = &[
    // Denormalized `turns.stop_reason` for outcome aggregation. (#437)
    (2, |conn| {
        add_column(conn, "ALTER TABLE turns ADD COLUMN stop_reason TEXT")
    }),
    // `tool_result_events` payload bytes for hotspots-by-bytes. (#436)
    (3, |conn| {
        add_column(
            conn,
            "ALTER TABLE tool_result_events ADD COLUMN output_bytes INTEGER",
        )?;
        add_column(
            conn,
            "ALTER TABLE tool_result_events ADD COLUMN output_truncated INTEGER",
        )
    }),
    // `turns.subagent_id`, queryable without re-parsing `record_json`. (#435)
    (4, |conn| {
        add_column(conn, "ALTER TABLE turns ADD COLUMN subagent_id TEXT")
    }),
    // The `inferences` table; `BURN_DDL` already creates it on every open,
    // this keeps a legacy ledger correct whatever the DDL ordering. (#434)
    (5, |conn| {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS inferences (
                 source              TEXT NOT NULL,
                 session_id          TEXT NOT NULL,
                 request_id          TEXT NOT NULL,
                 request_id_source   TEXT NOT NULL,
                 turn_id             TEXT NOT NULL,
                 model               TEXT NOT NULL,
                 kind                TEXT NOT NULL,
                 start_ts            TEXT NOT NULL,
                 end_ts              TEXT NOT NULL,
                 record_json         TEXT NOT NULL,
                 PRIMARY KEY (source, session_id, request_id)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS idx_inferences_session ON inferences(session_id);
             CREATE INDEX IF NOT EXISTS idx_inferences_turn ON inferences(turn_id);",
        )?;
        Ok(())
    }),
    // `archive_state.source_fingerprint`. (#468)
    (6, |conn| {
        add_column(
            conn,
            "ALTER TABLE archive_state ADD COLUMN source_fingerprint TEXT NOT NULL DEFAULT ''",
        )
    }),
    // `archive_state.last_write_at_ms`, seeded conservatively from the
    // newest event so an old snapshot is warned about on its first
    // post-upgrade read instead of made fresh by the migration. (#507)
    (7, |conn| {
        add_column(
            conn,
            "ALTER TABLE archive_state ADD COLUMN last_write_at_ms INTEGER",
        )?;
        seed_last_write_from_events(conn)
    }),
    // The 4.x readers' per-file cursors are meaningless once sessions come
    // from relayhistory: drop them. The next ingest reconciles against the
    // store and writes its watermark.
    (8, |conn| {
        conn.execute(
            "UPDATE archive_state SET upstream_cursors_json = '{}' WHERE id = 1",
            [],
        )?;
        Ok(())
    }),
    // `turns.request_count`. Codex rows written before request accounting
    // keep NULL, which is how ingest finds the sessions to re-account.
    (9, |conn| {
        add_column(conn, "ALTER TABLE turns ADD COLUMN request_count INTEGER")
    }),
];

/// Run one `ALTER TABLE … ADD COLUMN`, treating a column that already
/// exists as added.
///
/// The `ALTER` runs unconditionally and only the `duplicate column name`
/// failure is swallowed, rather than pre-checking with `PRAGMA table_info`:
/// the check-then-act sequence is racy under concurrent ledger opens (both
/// observe the column missing, both `ALTER`, the second loses). Letting
/// SQLite arbitrate keeps the migration idempotent. Any other failure,
/// including `SqliteFailure(_, None)`, propagates rather than silently
/// advancing `schema_version`.
pub(super) fn add_column(conn: &Connection, ddl: &str) -> Result<()> {
    match conn.execute(ddl, []) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
            if msg.contains("duplicate column name") =>
        {
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn verify_schema_version(conn: &Connection) -> Result<()> {
    let version: u32 = conn
        .query_row(
            "SELECT schema_version FROM archive_state WHERE id = 1",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v as u32)
        .unwrap_or(SCHEMA_VERSION);
    if version > SCHEMA_VERSION {
        return Err(crate::ledger::error::LedgerError::SchemaTooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// Infer legacy freshness from event time, preserving any newer known clock.
/// Used by both schema migration and JSONL replay; neither is live activity.
pub(crate) fn seed_last_write_from_events(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE archive_state
             SET last_write_at_ms = (
                 SELECT MAX(ms) FROM (
                     SELECT last_write_at_ms AS ms FROM archive_state WHERE id = 1
                     UNION ALL
                     SELECT CAST(strftime('%s', ts) AS INTEGER) * 1000
                         + CAST(substr(strftime('%f', ts), 4, 3) AS INTEGER) AS ms FROM turns
                     UNION ALL SELECT CAST(strftime('%s', ts) AS INTEGER) * 1000
                         + CAST(substr(strftime('%f', ts), 4, 3) AS INTEGER) FROM compactions
                     UNION ALL SELECT CAST(strftime('%s', ts) AS INTEGER) * 1000
                         + CAST(substr(strftime('%f', ts), 4, 3) AS INTEGER) FROM relationships
                     UNION ALL SELECT CAST(strftime('%s', ts) AS INTEGER) * 1000
                         + CAST(substr(strftime('%f', ts), 4, 3) AS INTEGER) FROM tool_result_events
                     UNION ALL SELECT CAST(strftime('%s', ts) AS INTEGER) * 1000
                         + CAST(substr(strftime('%f', ts), 4, 3) AS INTEGER) FROM user_turns
                     UNION ALL SELECT CAST(strftime('%s', end_ts) AS INTEGER) * 1000
                         + CAST(substr(strftime('%f', end_ts), 4, 3) AS INTEGER) FROM inferences
                 )
             )
             WHERE id = 1",
        [],
    )?;
    Ok(())
}
