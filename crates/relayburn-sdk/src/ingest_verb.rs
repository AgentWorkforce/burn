//! Ingest verb: [`crate::ingest_all`] against a [`LedgerHandle`].
//!
//! Threads the ledger location through [`crate::Ledger::open`] explicitly
//! instead of swapping `RELAYBURN_HOME`, so embeddings can run against
//! multiple ledgers in the same process.
//!
//! Sync by design: the body is a relayhistory sync plus rusqlite writes,
//! none of which yield to the tokio runtime. Callers running this from an
//! async context (the napi binding, MCP server) should wrap the call in
//! `tokio::task::spawn_blocking`.

use crate::ingest::{ingest_all, IngestOptions, IngestReport};

use crate::{Ledger, LedgerHandle, LedgerOpenOptions};

impl LedgerHandle {
    /// Run [`ingest_all`] against this ledger handle. `opts.ledger_home`
    /// defaults to the open ledger's directory.
    pub fn ingest(&mut self, mut opts: IngestOptions) -> anyhow::Result<IngestReport> {
        if opts.ledger_home.is_none() {
            opts.ledger_home = self.inner.burn_path().parent().map(|p| p.to_path_buf());
        }
        ingest_all(&mut self.inner, &opts)
    }
}

/// Free-function form of the ingest verb. Opens the ledger at
/// `opts.ledger_home`, runs [`ingest_all`], and returns the report.
pub fn ingest(opts: IngestOptions) -> anyhow::Result<IngestReport> {
    let mut handle = Ledger::open(LedgerOpenOptions {
        home: opts.ledger_home.clone(),
        ..Default::default()
    })?;
    handle.ingest(opts)
}
