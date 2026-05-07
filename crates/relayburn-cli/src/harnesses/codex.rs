//! Codex `HarnessAdapter` — Rust port of `packages/cli/src/harnesses/codex.ts`.
//!
//! Codex shares the pending-stamp + watch-loop shape with OpenCode, so the
//! adapter is constructed via [`super::pending_stamp::adapter_static`]
//! instead of re-implementing the trait. The only codex-specific bits are:
//!
//! * `name = "codex"` — the dispatch key and log-line label.
//! * `session_root` — `$HOME/.codex/sessions`, resolved lazily so tests
//!   that override `$HOME` see the override.
//! * `ingest_sessions` — opens a fresh ledger handle and runs
//!   [`relayburn_sdk::ingest_codex_sessions`] (the codex-only ingest pass).
//!   The TS sibling calls `ingestCodexSessions()` directly here; the Rust
//!   SDK function takes `&mut Ledger`, so the closure opens a handle each
//!   call. That mirrors the TS lock-then-write-then-close shape, and the
//!   per-tick open is cheap (SQLite WAL, no DDL after first open).
//!
//! The factory's [`super::pending_stamp::adapter_static`] does the
//! `Box::leak` so the registry can store the result as
//! `&'static dyn HarnessAdapter`. See the factory module for the leak
//! rationale (codex/opencode are the only two callers; runtime cost is
//! a few dozen bytes per process).

use std::path::PathBuf;
use std::sync::Arc;

use relayburn_sdk::{ingest_codex_sessions, Ledger, LedgerOpenOptions, RawIngestOptions};

use super::pending_stamp::{self, IngestSessionsFn, PendingStampAdapter};
use super::HarnessAdapter;

/// Resolve the codex session-store root. Mirrors the TS sibling
/// (`path.join(homedir(), '.codex', 'sessions')`) and the SDK's internal
/// `codex_sessions_dir` default. Resolved on every call so tests that
/// flip `$HOME` between runs see the override.
fn codex_sessions_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".codex").join("sessions")
}

/// Build the [`PendingStampAdapter`] config for codex. Exposed as a
/// constructor function (rather than a `static`) because the closure
/// captures and the `Arc<dyn Fn>`s inside don't fit a const initializer.
/// The registry calls this once and feeds the result to
/// [`pending_stamp::adapter_static`].
pub fn config() -> PendingStampAdapter {
    let session_root: Arc<dyn Fn() -> PathBuf + Send + Sync> = Arc::new(codex_sessions_dir);
    let ingest_sessions: IngestSessionsFn = Arc::new(|ledger_home| {
        Box::pin(async move {
            // Open a fresh ledger handle per tick. The TS sibling's
            // `ingestCodexSessions` does the same via `withLock('ledger', …)`;
            // SQLite WAL keeps the per-call open cheap. Use the same typed
            // ledger home the pending-stamp writer used so explicit
            // `--ledger-path` runs keep manifest writes and resolution scoped
            // to one home.
            let ledger_opts = match ledger_home.as_deref() {
                Some(home) => LedgerOpenOptions::with_home(home),
                None => LedgerOpenOptions::default(),
            };
            let mut handle = Ledger::open(ledger_opts)?;
            let opts = RawIngestOptions {
                ledger_home,
                ..RawIngestOptions::default()
            };
            ingest_codex_sessions(handle.raw_mut(), &opts).await
        })
    });
    PendingStampAdapter::new("codex", session_root, ingest_sessions)
}

/// Convenience: hand out a `&'static dyn HarnessAdapter` for the codex
/// adapter. The registry calls this once at lazy-init time. See
/// [`pending_stamp::adapter_static`] for the leak semantics — codex is
/// one of two callers and the leaked footprint is bytes, not megabytes.
pub fn adapter() -> &'static dyn HarnessAdapter {
    pending_stamp::adapter_static(config())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harnesses::test_env::with_test_home;

    /// `config()` returns a `PendingStampAdapter` named `codex` with the
    /// standard 1s tick interval. Sanity check that the constructor wires
    /// the name through the factory contract.
    #[test]
    fn config_has_codex_name() {
        let cfg = config();
        assert_eq!(cfg.name, "codex");
        // session_root closure resolves to `$HOME/.codex/sessions`. Use a
        // controlled $HOME so the assertion doesn't depend on the
        // developer's actual home dir; restored after via `with_test_home`.
        with_test_home("/tmp/burn-codex-test-home", || {
            let resolved = (cfg.session_root)();
            assert_eq!(
                resolved,
                PathBuf::from("/tmp/burn-codex-test-home/.codex/sessions")
            );
        });
    }

    /// `adapter()` round-trips through the trait surface — name, session
    /// root, and the `&'static` lifetime the registry requires. Mirrors
    /// the registry's `pending_stamp_adapter_static_fits_runtime_registry`
    /// check, but pinned to the codex configuration specifically.
    #[test]
    fn adapter_round_trip() {
        let a: &'static dyn HarnessAdapter = adapter();
        assert_eq!(a.name(), "codex");
        with_test_home("/tmp/burn-codex-test-home", || {
            assert_eq!(
                a.session_root(),
                PathBuf::from("/tmp/burn-codex-test-home/.codex/sessions")
            );
        });
    }
}
