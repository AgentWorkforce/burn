//! Bootstrap `burn.sqlite` from a `ledger.jsonl` sibling on `Ledger::open`.
//!
//! ## Why this exists
//!
//! The 2.0 SQLite-only design (see #259) treats `burn.sqlite` /
//! `content.sqlite` as the steady-state storage. But the TS 1.x ledger
//! is JSONL-of-record, and during the #240 cutover both write paths can
//! coexist on disk:
//!
//!   * a 1.x writer ingesting on the side, leaving `ledger.jsonl` ahead
//!     of any sqlite mirror;
//!   * a freshly built fixture (the cli-golden corpus is JSONL-only —
//!     the sqlite binaries are `.gitignore`d because they're rebuilt on
//!     demand);
//!   * a user upgrading and pointing the new SDK at their old
//!     `~/.relayburn/` home.
//!
//! In all three cases the Rust SDK was returning empty rows because it
//! reads exclusively from sqlite. The TS SDK didn't have this problem
//! because it treats sqlite as a derived view rebuilt on demand. This
//! module lifts that bootstrap algorithm into the Rust SDK so reads
//! always see the latest data.
//!
//! ## Algorithm — Option A (eager, on `Ledger::open`)
//!
//! Compare mtimes: if `ledger.jsonl` is newer than `burn.sqlite` (or
//! `burn.sqlite` is missing entirely), wipe the sqlite mirror, replay
//! the JSONL line-by-line via `Ledger::append_*`, and continue. If
//! `burn.sqlite` is at-or-newer than the JSONL, do nothing and let the
//! existing connection serve queries. If there is no `ledger.jsonl` at
//! all, do nothing — the SDK is in pure-sqlite mode and the caller is
//! responsible for any prior ingest.
//!
//! We picked Option A (eager on open) over Option B (lazy on first
//! read) because:
//!
//!   * Open is a rare event. Embedded callers usually open once per
//!     process; CLI invocations open once per `burn …` command. The
//!     bootstrap cost lands on the caller already paying for cold
//!     start.
//!   * Open has clean read-modify-write semantics — we already hold
//!     the only `&mut Connection` for `burn.sqlite`. A lazy bootstrap
//!     on first read would have to reach into every read verb and
//!     acquire a mutable handle just to maybe-rebuild, complicating
//!     `&self`-only read paths.
//!   * Bootstrap is idempotent and cheap when the mtime check is a
//!     no-op (the steady state). The only loss is an extra `stat()`
//!     pair per open.
//!
//! ## Concurrency
//!
//! Replay is a read-modify-write on `burn.sqlite`. SQLite's WAL mode
//! (configured in `db.rs`) plus the `busy_timeout` we set there
//! serialize peer writers without a user-space lockfile — the same
//! design choice that let us drop the 1.x `lock.ts` module from the
//! Rust port (see #259). Two concurrent `Ledger::open` callers that
//! both observe a stale sqlite will each attempt the rebuild; the
//! second will see an already-warm sqlite (mtime ≥ jsonl mtime) and
//! skip. Worst case is one redundant rebuild, which is cheap and
//! deterministic.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::Connection;

use crate::ledger::error::Result;
use crate::ledger::schema::DERIVABLE_TABLES;
use crate::ledger::stamp::Stamp;
use crate::ledger::writer;
use crate::reader::{
    CompactionEvent, SessionRelationshipRecord, ToolResultEventRecord, TurnRecord, UserTurnRecord,
};

/// Result of the staleness check, captured BEFORE `Connection::open`
/// creates `burn.sqlite` as a side effect (which would otherwise make
/// every fresh sqlite look "current" relative to the JSONL).
pub(crate) enum BootstrapDecision {
    /// No JSONL on disk OR sqlite already at-or-newer than JSONL — do
    /// nothing on open.
    Skip,
    /// JSONL is newer (or sqlite was missing). Replay this file once
    /// the sqlite handle is open + DDL'd.
    Rebuild { jsonl_path: PathBuf },
}

/// Path to the `ledger.jsonl` sibling of `burn.sqlite`. Returns `None`
/// when the burn path has no parent (e.g. a bare filename in cwd, in
/// which case the JSONL would also be in cwd — but we're conservative
/// and skip).
fn jsonl_sibling(burn_path: &Path) -> Option<PathBuf> {
    burn_path.parent().map(|p| p.join("ledger.jsonl"))
}

fn mtime(path: &Path) -> io::Result<SystemTime> {
    fs::metadata(path)?.modified()
}

/// Snapshot the JSONL-vs-sqlite staleness state. Must be called BEFORE
/// `Connection::open(burn_path)`, since that call creates the sqlite
/// file as a side effect.
///
///   * No JSONL → `Skip` (pure-sqlite ledger).
///   * JSONL exists but no `burn.sqlite` → `Rebuild`.
///   * JSONL mtime > sqlite mtime → `Rebuild`.
///   * Otherwise → `Skip`.
pub(crate) fn decide_bootstrap(burn_path: &Path) -> BootstrapDecision {
    let Some(jsonl_path) = jsonl_sibling(burn_path) else {
        return BootstrapDecision::Skip;
    };
    if !jsonl_path.is_file() {
        return BootstrapDecision::Skip;
    }
    let Ok(jsonl_mtime) = mtime(&jsonl_path) else {
        return BootstrapDecision::Skip;
    };
    match mtime(burn_path) {
        Ok(burn_mtime) if burn_mtime >= jsonl_mtime => BootstrapDecision::Skip,
        // burn.sqlite missing OR older than JSONL.
        _ => BootstrapDecision::Rebuild { jsonl_path },
    }
}

/// Apply the decision captured by [`decide_bootstrap`]. A no-op for
/// `BootstrapDecision::Skip`; for `Rebuild`, wipes derivable tables
/// and replays the JSONL via `writer::append_*`.
pub(crate) fn apply_bootstrap(
    burn: &mut Connection,
    decision: BootstrapDecision,
) -> Result<()> {
    match decision {
        BootstrapDecision::Skip => Ok(()),
        BootstrapDecision::Rebuild { jsonl_path } => rebuild_from_jsonl(burn, &jsonl_path),
    }
}

/// Wipe derivable tables, parse `ledger.jsonl`, and replay the records
/// through the writer. Stamps are first-party in 2.0 (preserved across
/// rebuild) but the JSONL replay re-emits them too — `append_stamp` is
/// idempotent on the `(source, session_id, ts, written_at)` PK so a
/// duplicate replay is a no-op.
fn rebuild_from_jsonl(burn: &mut Connection, jsonl_path: &Path) -> Result<()> {
    // Drop derivable tables only. Stamps + archive_state are first-party
    // and survive — the JSONL replay below will re-add stamp rows but
    // any existing ones are preserved if the JSONL doesn't list them.
    for table in DERIVABLE_TABLES {
        burn.execute(&format!("DELETE FROM {table}"), [])?;
    }

    let raw = fs::read_to_string(jsonl_path)?;

    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut user_turns: Vec<UserTurnRecord> = Vec::new();
    let mut tool_results: Vec<ToolResultEventRecord> = Vec::new();
    let mut relationships: Vec<SessionRelationshipRecord> = Vec::new();
    let mut compactions: Vec<CompactionEvent> = Vec::new();
    let mut stamps: Vec<Stamp> = Vec::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Tolerate malformed envelopes: a single bad line shouldn't
        // wedge the SDK on open. The `burn.sqlite` mirror will be
        // missing those records, but every well-formed line still
        // lands.
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let kind = envelope.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let mut record = envelope
            .get("record")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match kind {
            "turn" => {
                if let Ok(t) = serde_json::from_value::<TurnRecord>(record) {
                    turns.push(t);
                }
            }
            "user_turn" => {
                if let Ok(u) = serde_json::from_value::<UserTurnRecord>(record) {
                    user_turns.push(u);
                }
            }
            "tool_result_event" => {
                normalize_tool_result_event(&mut record);
                if let Ok(e) = serde_json::from_value::<ToolResultEventRecord>(record) {
                    tool_results.push(e);
                }
            }
            "relationship" => {
                if let Ok(r) = serde_json::from_value::<SessionRelationshipRecord>(record) {
                    relationships.push(r);
                }
            }
            "compaction" => {
                if let Ok(c) = serde_json::from_value::<CompactionEvent>(record) {
                    compactions.push(c);
                }
            }
            "stamp" => {
                stamps.push(stamp_from_envelope(&envelope));
            }
            _ => {
                // Unknown kinds (`text`, `tool_result`, etc. emitted by
                // older content-sidecar writers) are noise here — they
                // belong to the content DB lifecycle, not the events DB.
            }
        }
    }

    if !turns.is_empty() {
        writer::append_turns(burn, &turns)?;
    }
    if !user_turns.is_empty() {
        writer::append_user_turns(burn, &user_turns)?;
    }
    if !tool_results.is_empty() {
        writer::append_tool_result_events(burn, &tool_results)?;
    }
    if !relationships.is_empty() {
        writer::append_relationships(burn, &relationships)?;
    }
    if !compactions.is_empty() {
        writer::append_compactions(burn, &compactions)?;
    }
    for s in &stamps {
        writer::append_stamp(burn, s)?;
    }
    Ok(())
}

/// 1.x fixtures (and some early-port test corpora) wrote
/// `eventSource: "transcript"` for Claude `tool_result` events; the
/// canonical schema dropped that variant in favor of the more specific
/// `"tool_result"` value. The TS reader was lenient and stored the JSON
/// verbatim; the Rust SDK is strict. Normalize here so a stray legacy
/// row in upstream JSONL replays cleanly. Also fills in `eventIndex` if
/// the row omits it (required by the SDK schema; the TS reader defaults
/// missing values to `0`).
fn normalize_tool_result_event(record: &mut serde_json::Value) {
    let Some(obj) = record.as_object_mut() else {
        return;
    };
    if let Some(src) = obj.get_mut("eventSource") {
        if src.as_str() == Some("transcript") {
            *src = serde_json::Value::String("tool_result".to_string());
        }
    }
    obj.entry("eventIndex").or_insert(serde_json::json!(0));
}

fn stamp_from_envelope(envelope: &serde_json::Value) -> Stamp {
    Stamp {
        ts: envelope
            .get("ts")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        selector: serde_json::from_value(
            envelope
                .get("selector")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .unwrap_or_default(),
        enrichment: serde_json::from_value(
            envelope
                .get("enrichment")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    use crate::ledger::Ledger;

    /// Smallest possible turn JSONL envelope. `input` is parameterized
    /// because turns whose content fields exactly match collapse under
    /// the writer's `content_fingerprint` dedup — tests need distinct
    /// `input` counts to keep two synthetic turns from merging.
    fn turn_envelope_line(session: &str, message: &str, input: u64) -> String {
        let record = serde_json::json!({
            "v": 1,
            "source": "claude-code",
            "sessionId": session,
            "messageId": message,
            "turnIndex": 0,
            "ts": "2025-01-01T00:00:00Z",
            "model": "claude-sonnet-4-6",
            "usage": {
                "input": input,
                "output": 5,
                "reasoning": 0,
                "cacheRead": 0,
                "cacheCreate5m": 0,
                "cacheCreate1h": 0
            },
            "toolCalls": []
        });
        format!(
            r#"{{"v":1,"kind":"turn","record":{}}}"#,
            serde_json::to_string(&record).unwrap()
        )
    }

    #[test]
    fn no_jsonl_no_bootstrap() {
        // Pure-sqlite ledger — no JSONL on disk. Open should be a no-op
        // and turns table stays empty.
        let tmp = TempDir::new().unwrap();
        let burn = tmp.path().join("burn.sqlite");
        let content = tmp.path().join("content.sqlite");
        let l = Ledger::open(&burn, &content).unwrap();
        assert_eq!(l.count_table("turns").unwrap(), 0);
        assert!(!tmp.path().join("ledger.jsonl").exists());
    }

    #[test]
    fn jsonl_only_bootstraps_on_open() {
        // The "freshly-cloned cli-golden fixture" scenario: ledger.jsonl
        // exists, burn.sqlite does not. Open should populate the events
        // DB.
        let tmp = TempDir::new().unwrap();
        let jsonl = tmp.path().join("ledger.jsonl");
        let burn = tmp.path().join("burn.sqlite");
        let content = tmp.path().join("content.sqlite");

        let mut f = fs::File::create(&jsonl).unwrap();
        writeln!(f, "{}", turn_envelope_line("sess-a", "msg-1", 10)).unwrap();
        writeln!(f, "{}", turn_envelope_line("sess-a", "msg-2", 20)).unwrap();
        f.flush().unwrap();
        drop(f);

        let l = Ledger::open(&burn, &content).unwrap();
        assert_eq!(l.count_table("turns").unwrap(), 2);
    }

    /// Force a file's mtime via `File::set_modified` — stable since
    /// 1.75 and works on all OSes the workspace supports without an
    /// extra crate.
    fn set_mtime(path: &Path, when: SystemTime) {
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }

    #[test]
    fn fresh_sqlite_skips_bootstrap() {
        // burn.sqlite mtime ≥ ledger.jsonl mtime → no rebuild.
        let tmp = TempDir::new().unwrap();
        let jsonl = tmp.path().join("ledger.jsonl");
        let burn = tmp.path().join("burn.sqlite");
        let content = tmp.path().join("content.sqlite");

        // First write the JSONL, then build the sqlite. The sqlite's
        // mtime will be newer.
        fs::write(&jsonl, turn_envelope_line("sess-a", "msg-1", 10) + "\n").unwrap();
        // Build sqlite once (this rebuilds from JSONL — 1 row).
        {
            let _ = Ledger::open(&burn, &content).unwrap();
        }
        // Bump the sqlite's mtime explicitly so we don't depend on
        // filesystem resolution.
        set_mtime(&burn, SystemTime::now() + std::time::Duration::from_secs(60));

        // Append a second turn to the JSONL — but *force* its mtime to
        // be older than sqlite's. The reopen should NOT rebuild and the
        // count should stay at 1.
        let mut f = fs::OpenOptions::new().append(true).open(&jsonl).unwrap();
        writeln!(f, "{}", turn_envelope_line("sess-a", "msg-2", 20)).unwrap();
        drop(f);
        set_mtime(&jsonl, SystemTime::now() - std::time::Duration::from_secs(60));

        let l = Ledger::open(&burn, &content).unwrap();
        assert_eq!(l.count_table("turns").unwrap(), 1);
    }

    #[test]
    fn stale_sqlite_rebuilds_on_open() {
        // burn.sqlite is older than ledger.jsonl — rebuild and pick up
        // the newer JSONL contents.
        let tmp = TempDir::new().unwrap();
        let jsonl = tmp.path().join("ledger.jsonl");
        let burn = tmp.path().join("burn.sqlite");
        let content = tmp.path().join("content.sqlite");

        // Initial state: 1-line JSONL, sqlite built from it.
        fs::write(&jsonl, turn_envelope_line("sess-a", "msg-1", 10) + "\n").unwrap();
        {
            let l = Ledger::open(&burn, &content).unwrap();
            assert_eq!(l.count_table("turns").unwrap(), 1);
        }

        // Force sqlite's mtime well into the past.
        set_mtime(&burn, SystemTime::now() - std::time::Duration::from_secs(3600));

        // Append to JSONL — its mtime is now newer than sqlite's.
        let mut f = fs::OpenOptions::new().append(true).open(&jsonl).unwrap();
        writeln!(f, "{}", turn_envelope_line("sess-a", "msg-2", 20)).unwrap();
        drop(f);
        set_mtime(&jsonl, SystemTime::now());

        let l = Ledger::open(&burn, &content).unwrap();
        assert_eq!(l.count_table("turns").unwrap(), 2);
    }
}
