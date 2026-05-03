//! Test-only helpers shared across modules in this crate.
//!
//! `RELAYBURN_HOME` is process-wide. `cargo test` runs cases in parallel, so
//! anything that mutates the env var must serialize on a single global
//! mutex — otherwise concurrent tests in different modules clobber each
//! others' home directory mid-run, and reads of `lock_path()` /
//! `ledger_path()` race.

#![cfg(test)]

use tokio::sync::{Mutex, MutexGuard};

static GLOBAL_ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Acquire the cross-module env lock, set `RELAYBURN_HOME` to `dir`, and
/// invalidate any cached state keyed on the previous home. Hold the
/// returned guard for the whole test body — `tokio::sync::Mutex` is
/// async-aware so it can be held across `.await` without tripping
/// `clippy::await_holding_lock`.
pub async fn set_home(dir: &std::path::Path) -> MutexGuard<'static, ()> {
    let g = GLOBAL_ENV_LOCK.lock().await;
    std::env::set_var("RELAYBURN_HOME", dir);
    crate::sidecar::invalidate_index_cache().await;
    g
}
