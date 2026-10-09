//! Ingest: bring the ledger up to date with the relayhistory store.
//!
//! relayhistory owns reading harness sessions. One ingest syncs the store
//! (bootstrapping it on first use), reads which sessions the store's change
//! feed reports since the ledger's watermark, rebuilds each one's records,
//! and appends them. Appends are idempotent, so a session read twice is
//! billed once, and the watermark advances only after every session it
//! covers landed: a failure re-reads those sessions next time rather than
//! skipping them.
//!
//! ```no_run
//! use relayburn_sdk::{ingest_all, IngestOptions, RawLedger};
//! # fn run() -> anyhow::Result<()> {
//! let mut ledger = RawLedger::open_default()?;
//! let report = ingest_all(&mut ledger, &IngestOptions::default())?;
//! println!("ingested {} turns", report.appended_turns);
//! # Ok(()) }
//! ```

use std::path::{Path, PathBuf};

use ai_hist::{HydrateOptions, HydrateStatus, SessionIdentity, SessionRef, SessionStore, Source};
use serde::{Deserialize, Serialize};

use super::import::{import_sessions, ImportContext};
use super::pending_stamps::cleanup_stale_pending_stamps_in;
use crate::ledger::{load_config, Ledger};
use crate::reader::ContentStoreMode;
use crate::source::locate::{open_store, HistoryStoreOptions};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestReport {
    /// Sessions whose records were rebuilt.
    pub scanned_sessions: usize,
    /// Sessions that gained at least one turn.
    pub ingested_sessions: usize,
    pub appended_turns: usize,
    #[serde(default)]
    pub applied_pending_stamps: usize,
}

impl IngestReport {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn merge(&mut self, other: &IngestReport) {
        self.scanned_sessions += other.scanned_sessions;
        self.ingested_sessions += other.ingested_sessions;
        self.appended_turns += other.appended_turns;
        self.applied_pending_stamps += other.applied_pending_stamps;
    }
}

/// Sink for short orchestration progress strings (one per phase). The CLI
/// uses it to drive a spinner.
pub type ProgressSink = Box<dyn Fn(&str) + Send + Sync>;

/// Options shared by every ingest entry point.
#[derive(Default)]
pub struct IngestOptions {
    pub on_progress: Option<ProgressSink>,
    /// Override for the relayburn home holding config and pending-stamp
    /// manifests. The opened [`Ledger`] still owns the database paths.
    pub ledger_home: Option<PathBuf>,
    /// The relayhistory store sessions are read from.
    pub store: HistoryStoreOptions,
}

impl IngestOptions {
    pub(super) fn progress(&self, msg: &str) {
        if let Some(cb) = &self.on_progress {
            cb(msg);
        }
    }

    pub(super) fn context(&self) -> ImportContext<'_> {
        ImportContext {
            content_mode: resolve_content_mode(self.ledger_home.as_deref()),
            ledger_home: self.ledger_home.as_deref(),
        }
    }
}

/// `content.store` from the relayburn config, `Full` when the config cannot
/// be read so a corrupt file never stops ingest.
fn resolve_content_mode(ledger_home: Option<&Path>) -> ContentStoreMode {
    let config = match ledger_home {
        Some(home) => crate::ledger::load_config_at(&home.join("config.json")),
        None => load_config(),
    };
    config
        .map(|c| c.content.store)
        .unwrap_or(ContentStoreMode::Full)
}

/// Sync the relayhistory store, then append every session it changed since
/// the ledger's watermark.
pub fn ingest_all(ledger: &mut Ledger, opts: &IngestOptions) -> anyhow::Result<IngestReport> {
    opts.progress("opening session history");
    let store = open_store(&opts.store)?;
    opts.progress("syncing session history");
    sync(&store)?;
    pull(ledger, &store, opts)
}

/// One sweep of the store. A sweep another process holds is the same
/// sweep, so its lock is waited on briefly and then left to it.
fn sync(store: &SessionStore) -> anyhow::Result<()> {
    let mut options = ai_hist::SyncOptions::default();
    options.lock_timeout_ms = SYNC_LOCK_WAIT_MS;
    match store.sync(options) {
        Ok(_) | Err(ai_hist::Error::SyncLocked { .. }) => Ok(()),
        Err(error) => Err(anyhow::Error::new(error).context("sync session history")),
    }
}

const SYNC_LOCK_WAIT_MS: u64 = 2_000;

/// Append every session the store changed since the ledger's watermark,
/// without syncing it first. The watch loop calls this after each sweep.
pub(super) fn pull(
    ledger: &mut Ledger,
    store: &SessionStore,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    opts.progress("cleaning pending spawn stamps");
    cleanup_stale_pending_stamps_in(opts.ledger_home.as_deref())?;
    opts.progress("reading changed sessions");
    let from = super::watermark::stored(ledger);
    let (changed, head) = super::watermark::changed_since(store, from)?;
    opts.progress("importing sessions");
    let imported = import_sessions(ledger, store, &opts.context(), changed)?;
    if imported.complete && from != Some(head) {
        super::watermark::store(ledger, head)?;
    }
    Ok(imported.report)
}

/// The hook fast path: index the one Claude transcript the hook names and
/// append its session, without sweeping every provider. A transcript that
/// is missing or names no session yet (still being written, or a subagent
/// sidecar) is an empty report, so a hook never fails its harness.
pub fn ingest_claude_transcript_path(
    ledger: &mut Ledger,
    transcript_path: &Path,
    opts: &IngestOptions,
) -> anyhow::Result<IngestReport> {
    match std::fs::metadata(transcript_path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(IngestReport::empty()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(IngestReport::empty()),
        Err(err) => return Err(err.into()),
    }
    let store = open_store(&opts.store)?;
    let reference = SessionRef::path(Source::Claude, transcript_path);
    let hydrated = store.hydrate(&reference, HydrateOptions::default())?;
    let SessionRef::Id { session_id, .. } = hydrated.session else {
        return Ok(IngestReport::empty());
    };
    if matches!(
        hydrated.status,
        HydrateStatus::Missing | HydrateStatus::Unidentified | HydrateStatus::Mismatched
    ) {
        return Ok(IngestReport::empty());
    }
    cleanup_stale_pending_stamps_in(opts.ledger_home.as_deref())?;
    let identity = SessionIdentity::new(Source::Claude.as_str(), session_id);
    let imported = import_sessions(ledger, &store, &opts.context(), [identity].into())?;
    Ok(imported.report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_merge_sums_components() {
        let mut a = IngestReport {
            scanned_sessions: 1,
            ingested_sessions: 2,
            appended_turns: 3,
            applied_pending_stamps: 4,
        };
        let b = IngestReport {
            scanned_sessions: 10,
            ingested_sessions: 20,
            appended_turns: 30,
            applied_pending_stamps: 40,
        };
        a.merge(&b);
        assert_eq!(a.scanned_sessions, 11);
        assert_eq!(a.ingested_sessions, 22);
        assert_eq!(a.appended_turns, 33);
        assert_eq!(a.applied_pending_stamps, 44);
    }
}
