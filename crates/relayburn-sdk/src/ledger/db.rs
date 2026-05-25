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
/// Migrations are tagged by destination schema version; each step is
/// guarded so re-running `Ledger::open` after a crash mid-migration picks
/// up where it left off without surfacing `duplicate column name` errors.
fn migrate_burn_schema(conn: &Connection) -> Result<()> {
    let current_version: u32 = conn
        .query_row(
            "SELECT schema_version FROM archive_state WHERE id = 1",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v as u32)
        .unwrap_or(SCHEMA_VERSION);

    if current_version < 2 {
        // v1 → v2: add the denormalized `turns.stop_reason` column for
        // outcome aggregation. `CREATE TABLE IF NOT EXISTS` in the DDL
        // already covers fresh DBs (the column lives in the DDL); this
        // branch handles existing v1 ledgers whose `turns` table
        // pre-existed the bump.
        //
        // We try the `ALTER TABLE` unconditionally and swallow the
        // `duplicate column name` failure rather than pre-checking with
        // `PRAGMA table_info`. The check-then-act sequence is racy under
        // concurrent ledger opens: two processes can both observe the
        // column missing, both issue the `ALTER`, and the second loses.
        // Letting SQLite arbitrate via the duplicate-column error keeps
        // the migration genuinely idempotent. We deliberately don't
        // catch the `SqliteFailure(_, None)` shape — that's too broad
        // and would mask real schema breakage.
        match conn.execute("ALTER TABLE turns ADD COLUMN stop_reason TEXT", []) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") => {}
            Err(e) => return Err(e.into()),
        }
        conn.execute(
            "UPDATE archive_state SET schema_version = 2 WHERE id = 1",
            [],
        )?;
    }

    if current_version < 3 {
        // v2 → v3: add nullable `output_bytes` / `output_truncated` to
        // `tool_result_events` for hotspots-by-bytes ranking. Same
        // duplicate-column-only swallow pattern as the v1 → v2 step —
        // any other `SqliteFailure` (including `(_, None)`) must
        // propagate rather than silently advance `schema_version`.
        for ddl in [
            "ALTER TABLE tool_result_events ADD COLUMN output_bytes INTEGER",
            "ALTER TABLE tool_result_events ADD COLUMN output_truncated INTEGER",
        ] {
            match conn.execute(ddl, []) {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                    if msg.contains("duplicate column name") => {}
                Err(e) => return Err(e.into()),
            }
        }
        conn.execute(
            "UPDATE archive_state SET schema_version = 3 WHERE id = 1",
            [],
        )?;
    }

    if current_version < 4 {
        // v3 → v4: add nullable `turns.subagent_id` so subagent rows
        // (sidechain rows in the parent file today; sidecar transcripts
        // under `<sessionId>/subagents/` once the discovery path is
        // wired into ingest) are queryable structurally without
        // re-deserializing `record_json`. Same duplicate-column-only
        // swallow pattern as the earlier steps. See
        // AgentWorkforce/burn#435.
        match conn.execute("ALTER TABLE turns ADD COLUMN subagent_id TEXT", []) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") => {}
            Err(e) => return Err(e.into()),
        }
        conn.execute(
            "UPDATE archive_state SET schema_version = 4 WHERE id = 1",
            [],
        )?;
    }

    if current_version < 5 {
        // v4 → v5: add the `inferences` derived table. The full DDL is
        // baked into `BURN_DDL`'s `CREATE TABLE IF NOT EXISTS` block so a
        // fresh open is already correct; here we only need to make sure
        // the table EXISTS on legacy DBs whose `BURN_DDL` pre-pass might
        // have happened against the pre-v5 schema (e.g. if a future
        // migration step changes ordering). The CREATE is idempotent on
        // every open, so re-running this step costs only a catalog
        // probe. The table is populated by the ingest pipeline; pre-v5
        // ledgers stay empty until `burn state rebuild`. See issue #434.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS inferences (\
                 source              TEXT NOT NULL,\
                 session_id          TEXT NOT NULL,\
                 request_id          TEXT NOT NULL,\
                 request_id_source   TEXT NOT NULL,\
                 turn_id             TEXT NOT NULL,\
                 model               TEXT NOT NULL,\
                 kind                TEXT NOT NULL,\
                 start_ts            TEXT NOT NULL,\
                 end_ts              TEXT NOT NULL,\
                 record_json         TEXT NOT NULL,\
                 PRIMARY KEY (source, session_id, request_id)\
             ) STRICT",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_inferences_session ON inferences(session_id)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_inferences_turn ON inferences(turn_id)",
            [],
        )?;
        conn.execute(
            "UPDATE archive_state SET schema_version = 5 WHERE id = 1",
            [],
        )?;
    }

    // The `idx_turns_stop_reason` index is created here rather than in
    // the static DDL so a legacy v1 table (no `stop_reason` column yet)
    // doesn't fail the DDL pre-pass. By this point the column either
    // existed all along (fresh v2+ DDL) or was just added by the v1 → v2
    // step above, so the index is safe to create idempotently every
    // open.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_turns_stop_reason \
         ON turns(stop_reason) WHERE stop_reason IS NOT NULL",
        [],
    )?;

    // `idx_turns_subagent_id` lives here for the same reason as
    // `idx_turns_stop_reason`: a legacy table without the column would
    // otherwise blow up the DDL pre-pass. Partial-index predicate
    // (`WHERE subagent_id IS NOT NULL`) keeps the index size tracking
    // subagent row count, not the full turn count — subagent rows are
    // a tiny minority on most ledgers.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_turns_subagent_id \
         ON turns(subagent_id) WHERE subagent_id IS NOT NULL",
        [],
    )?;

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
