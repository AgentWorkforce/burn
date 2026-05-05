//! `relayburn-ingest` — Rust port of `@relayburn/ingest`. See
//! AgentWorkforce/burn#245.
//!
//! Owns session-store discovery, parse-and-append orchestration, the
//! pending-stamp coordination layer, and the poll-based watch loop. The
//! per-harness ingest helpers are scaffolded; the standalone modules
//! (`pending_stamps`, `walk`, `watch_loop`, `cursors`) are fully ported
//! and tested.
//!
//! Example: run a one-shot ingest sweep against the default ledger paths.
//!
//! ```no_run
//! use relayburn_ingest::{ingest_all, IngestOptions};
//! use relayburn_ledger::Ledger;
//! # async fn run() -> anyhow::Result<()> {
//! let mut ledger = Ledger::open_default()?;
//! let report = ingest_all(&mut ledger, &IngestOptions::default()).await?;
//! println!("ingested {} turns", report.appended_turns);
//! # Ok(()) }
//! ```

pub mod cursors;
pub mod ingest;
pub mod pending_stamps;
pub mod walk;
pub mod watch_loop;

pub use cursors::{
    load_cursors, save_cursor_changes, save_cursors, ClaudeCursor, CodexCumulative, CodexCursor,
    Cursors, FileCursor, OpencodeCursor, OpencodeStreamCursor,
};
pub use ingest::{
    ingest_all, ingest_claude_projects, ingest_claude_session, ingest_codex_sessions,
    ingest_opencode_sessions, reingest_missing_content, ContentStoreMode, IngestOptions,
    IngestReport, IngestRoots, ReingestContentReport,
};
pub use pending_stamps::{
    cleanup_stale_pending_stamps, cleanup_stale_pending_stamps_at, pending_stamps_dir,
    resolve_pending_stamps_for_session, write_pending_stamp, PendingStamp,
    PendingStampCleanupResult, PendingStampHarness, PendingStampResolveResult,
    PendingStampSessionCandidate, PendingStampWriteResult, WriteOptions, PENDING_STAMP_TTL_MS,
};
pub use walk::{walk_jsonl, walk_opencode_sessions};
pub use watch_loop::{
    run_ingest_tick, start_watch_loop, ErrorSink, IngestFn, ReportSink, StartWatchLoopOptions,
    WatchController,
};
