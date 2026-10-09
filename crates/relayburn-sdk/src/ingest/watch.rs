//! `burn ingest --watch`: relayhistory's live-capture loop, with the ledger
//! pulled after every sweep.
//!
//! [`SessionStore::watch`] owns the filesystem-event driver, its polling
//! fallback and the sweep; each tick it reports, burn appends whatever the
//! change feed gained. The loop runs on the caller's thread until its stop
//! token is stopped.

use std::time::Duration;

use ai_hist::{StopToken, WatchOptions};

use super::ingest::pull;
use super::{IngestOptions, IngestReport};
use crate::ledger::Ledger;
use crate::source::locate::open_store;

/// How the watch loop wakes up.
#[derive(Debug, Clone)]
pub struct WatchIngestOptions {
    /// Polling cadence when filesystem events are off or unavailable.
    pub poll_interval: Duration,
    /// Drive sweeps from filesystem events, polling as the backstop.
    pub use_fs_events: bool,
    /// Ends the loop once stopped, cancelling a sweep in flight.
    pub stop: StopToken,
}

impl Default for WatchIngestOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            use_fs_events: true,
            stop: StopToken::new(),
        }
    }
}

/// Run the watch loop: one sweep and pull right away, then one per change
/// the store notices. `on_tick` receives each pull's report, or the error
/// that stopped that tick; the loop keeps going either way.
pub fn watch_ingest(
    ledger: &mut Ledger,
    opts: &IngestOptions,
    watch: WatchIngestOptions,
    mut on_tick: impl FnMut(anyhow::Result<IngestReport>),
) -> anyhow::Result<()> {
    let store = open_store(&opts.store)?;
    let mut options = WatchOptions::default();
    options.poll_interval_ms = u64::try_from(watch.poll_interval.as_millis()).unwrap_or(u64::MAX);
    options.use_fs_events = watch.use_fs_events;
    options.immediate = true;
    options.stop = Some(watch.stop);
    for tick in store.watch(options)? {
        match tick {
            Ok(_) => on_tick(pull(ledger, &store, opts)),
            Err(error) => on_tick(Err(error.into())),
        }
    }
    Ok(())
}
