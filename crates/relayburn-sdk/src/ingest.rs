//! Ingest: the ledger kept up to date from the relayhistory store, plus the
//! pending-stamp layer launchers use to tag sessions they spawn.

mod import;
#[allow(clippy::module_inception)]
mod ingest;
pub mod pending_stamps;
mod watch;
mod watermark;

#[cfg(test)]
mod ingest_tests;
#[cfg(test)]
mod pending_stamps_compat_tests;

pub use ingest::{
    ingest_all, ingest_claude_transcript_path, IngestOptions, IngestReport, ProgressSink,
};
pub use pending_stamps::{
    cleanup_stale_pending_stamps, write_pending_stamp, PendingStamp, PendingStampHarness,
    PendingStampWriteResult, WriteOptions,
};
pub use watch::{watch_ingest, WatchIngestOptions};
