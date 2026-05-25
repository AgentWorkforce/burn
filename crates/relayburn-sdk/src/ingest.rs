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
//! use relayburn_sdk::{ingest_all, RawIngestOptions, RawLedger};
//! # fn run() -> anyhow::Result<()> {
//! let mut ledger = RawLedger::open_default()?;
//! let report = ingest_all(&mut ledger, &RawIngestOptions::default())?;
//! println!("ingested {} turns", report.appended_turns);
//! # Ok(()) }
//! ```

pub mod cursors;
pub(crate) mod fs_events;
pub mod gap;
pub mod ingest;
pub mod pending_stamps;
pub mod reingest;
pub mod walk;
pub mod watch_loop;

// Tests preserved from the pre-restructure `relayburn-ingest` integration
// `tests/` directory. They were promoted to in-crate tests when the
// monolith collapsed because they exercise crate-private items
// (`parse_pending_stamp`, `set_ingest_gap_writer`, `LedgerLayout`, etc.)
// that the new SDK surface intentionally doesn't re-export.
//
// Lock note: the original tests each owned their own `static ENV_LOCK`
// because they ran as separate integration-test binaries (= separate
// processes), so the locks didn't need to coordinate. Bundled into one
// binary those statics become distinct mutexes in the same address
// space — `$RELAYBURN_HOME` mutations in `orchestration_tests` then
// race gap-state mutations in `gap_warning_tests`. The shared
// `TEST_ENV_LOCK` and `TEST_GAP_LOCK` below fix that — every test that
// touches `$RELAYBURN_HOME` or the process-global gap tracker takes them.
#[cfg(test)]
mod gap_warning_tests;
#[cfg(test)]
mod orchestration_tests;
#[cfg(test)]
mod pending_stamps_compat_tests;
#[cfg(test)]
mod watch_loop_tests;

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
pub(crate) static TEST_GAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Test-only writer-override hooks. Gated to `cfg(test)` for in-crate
// tests and to the `test-utils` feature for downstream integration
// tests; deliberately NOT part of the default SDK surface so embedders
// can't hijack the global gap-warning writer for the whole process.
#[cfg(any(test, feature = "test-utils"))]
pub use gap::{restore_ingest_gap_writer, set_ingest_gap_writer};
pub use ingest::{
    default_session_roots, ingest_all, ingest_claude_session, ingest_claude_transcript_path,
    ingest_codex_sessions, ingest_opencode_sessions, IngestOptions, IngestReport, IngestRoots,
};
pub use pending_stamps::{
    cleanup_stale_pending_stamps, write_pending_stamp, PendingStamp, PendingStampHarness,
    PendingStampWriteResult, WriteOptions,
};
pub use watch_loop::{
    start_watch_loop, ErrorSink, IngestFn, ReportSink, StartWatchLoopOptions, WatchController,
    DEFAULT_FS_DEBOUNCE, DEFAULT_SLOW_FALLBACK,
};
