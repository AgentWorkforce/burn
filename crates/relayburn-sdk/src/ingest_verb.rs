//! Ingest verb — async wrapper over [`crate::ingest::ingest_all`].
//!
//! Mirrors the TS `ingest` verb in `packages/sdk/index.js`. The Rust port
//! threads the ledger location through [`crate::Ledger::open`] explicitly
//! instead of swapping `RELAYBURN_HOME`, so embeddings can run against
//! multiple ledgers in the same process.

use std::path::PathBuf;

use crate::ingest::{ingest_all, IngestOptions as RawIngestOptions, IngestReport, IngestRoots};

use crate::{Ledger, LedgerHandle, LedgerOpenOptions};

/// Sink for short orchestration progress strings (one per phase).
pub type ProgressSink = Box<dyn Fn(&str) + Send + Sync>;

/// Sink for content-capture gap warnings emitted during ingest.
pub type WarnSink = Box<dyn Fn(&str) + Send + Sync>;

/// SDK-level options for the [`ingest`] verb. Mirrors the TS shape but
/// uses Rust-friendly types (`PathBuf`, boxed sinks).
///
/// `ledger_home` is only consulted by the free [`ingest`] function (which
/// opens its own [`LedgerHandle`]); the [`LedgerHandle::ingest`] method
/// uses the already-open ledger and ignores it.
#[derive(Default)]
pub struct IngestOptions {
    /// Override for `$RELAYBURN_HOME`. Forwarded to [`LedgerOpenOptions::home`]
    /// when the free function opens a ledger.
    pub ledger_home: Option<PathBuf>,
    /// Per-harness session-store roots. Defaults to scanning the developer's
    /// home dir (`~/.claude/projects`, `~/.codex/sessions`,
    /// `~/.local/share/opencode/storage`); tests must inject explicit roots.
    pub roots: IngestRoots,
    /// Optional sink for short orchestration progress strings.
    pub on_progress: Option<ProgressSink>,
    /// Optional sink for content-capture gap warnings.
    pub on_warn: Option<WarnSink>,
}

impl IngestOptions {
    /// Convert to the lower-crate options struct, consuming the closure
    /// sinks. `ledger_home` is dropped here — it is consumed at ledger-open
    /// time and is not part of the lower-crate API.
    fn into_raw(self) -> RawIngestOptions {
        RawIngestOptions {
            on_progress: self.on_progress,
            on_warn: self.on_warn,
            roots: self.roots,
        }
    }
}

impl LedgerHandle {
    /// Run [`ingest_all`] against this ledger handle. Returns the merged
    /// per-harness report.
    pub async fn ingest(&mut self, opts: IngestOptions) -> anyhow::Result<IngestReport> {
        let raw = opts.into_raw();
        ingest_all(&mut self.inner, &raw).await
    }
}

/// Free-function form of the ingest verb. Opens a fresh ledger using
/// `opts.ledger_home`, runs [`ingest_all`], and returns the report.
pub async fn ingest(opts: IngestOptions) -> anyhow::Result<IngestReport> {
    let mut handle = Ledger::open(LedgerOpenOptions {
        home: opts.ledger_home.clone(),
        ..Default::default()
    })?;
    handle.ingest(opts).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn ingest_with_empty_roots_returns_zero_report() {
        let home = TempDir::new().expect("home tmp");
        let claude = TempDir::new().expect("claude tmp");
        let codex = TempDir::new().expect("codex tmp");
        let opencode = TempDir::new().expect("opencode tmp");

        // Point `RELAYBURN_HOME` at the temp dir so `cleanup_stale_pending_stamps`
        // and `load_config` (called inside ingest_all) don't touch the real
        // `~/.relayburn`. Set before any ledger-open call.
        std::env::set_var("RELAYBURN_HOME", home.path());

        let opts = IngestOptions {
            ledger_home: Some(home.path().to_path_buf()),
            roots: IngestRoots {
                claude_projects_dir: Some(claude.path().to_path_buf()),
                codex_sessions_dir: Some(codex.path().to_path_buf()),
                opencode_storage_dir: Some(opencode.path().to_path_buf()),
            },
            on_progress: None,
            on_warn: None,
        };

        let report = ingest(opts).await.expect("ingest");
        assert_eq!(report.scanned_sessions, 0);
        assert_eq!(report.ingested_sessions, 0);
        assert_eq!(report.appended_turns, 0);
    }
}
