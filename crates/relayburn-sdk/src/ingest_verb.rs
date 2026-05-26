//! Ingest verb — wrapper over [`crate::ingest::ingest_all`].
//!
//! Mirrors the TS `ingest` verb in `packages/sdk/index.js`. The Rust port
//! threads the ledger location through [`crate::Ledger::open`] explicitly
//! instead of swapping `RELAYBURN_HOME`, so embeddings can run against
//! multiple ledgers in the same process.
//!
//! Sync by design: the body is filesystem walks plus rusqlite writes, none
//! of which yield to the tokio runtime. Callers running this from an async
//! context (the napi binding, MCP server) should wrap the call in
//! `tokio::task::spawn_blocking`.

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
/// `ledger_home` chooses the sidecar home for config and pending-stamp
/// manifests. The free [`ingest`] function also uses it to open the ledger;
/// the [`LedgerHandle::ingest`] method defaults it from the open ledger path
/// when omitted.
#[derive(Default)]
pub struct IngestOptions {
    /// Override for `$RELAYBURN_HOME`-scoped sidecars. Forwarded to
    /// [`LedgerOpenOptions::home`] when the free function opens a ledger.
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
            ledger_home: self.ledger_home,
            roots: self.roots,
        }
    }
}

impl LedgerHandle {
    /// Run [`ingest_all`] against this ledger handle. Returns the merged
    /// per-harness report.
    pub fn ingest(&mut self, mut opts: IngestOptions) -> anyhow::Result<IngestReport> {
        if opts.ledger_home.is_none() {
            opts.ledger_home = self.inner.burn_path().parent().map(|p| p.to_path_buf());
        }
        let raw = opts.into_raw();
        ingest_all(&mut self.inner, &raw)
    }
}

/// Free-function form of the ingest verb. Opens a fresh ledger using
/// `opts.ledger_home`, runs [`ingest_all`], and returns the report.
pub fn ingest(opts: IngestOptions) -> anyhow::Result<IngestReport> {
    let mut handle = Ledger::open(LedgerOpenOptions {
        home: opts.ledger_home.clone(),
        ..Default::default()
    })?;
    handle.ingest(opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ingest_with_empty_roots_returns_zero_report() {
        let home = TempDir::new().expect("home tmp");
        let claude = TempDir::new().expect("claude tmp");
        let codex = TempDir::new().expect("codex tmp");
        let opencode = TempDir::new().expect("opencode tmp");

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

        let report = ingest(opts).expect("ingest");
        assert_eq!(report.scanned_sessions, 0);
        assert_eq!(report.ingested_sessions, 0);
        assert_eq!(report.appended_turns, 0);
    }
}
