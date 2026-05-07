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
        let bootstrap_decision =
            crate::ledger::bootstrap::decide_bootstrap(burn_path);

        let mut burn = Connection::open(burn_path)?;
        configure_pragmas(&burn)?;
        burn.execute_batch(BURN_DDL)?;
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
