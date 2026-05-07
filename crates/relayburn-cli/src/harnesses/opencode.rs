//! OpenCode `HarnessAdapter` — Rust port of `packages/cli/src/harnesses/opencode.ts`.
//!
//! OpenCode shares the pending-stamp + watch-loop shape with codex, so the
//! adapter is constructed via [`super::pending_stamp::adapter_static`]
//! instead of re-implementing the trait. The only opencode-specific bits are:
//!
//! * `name = "opencode"` — the dispatch key and log-line label.
//! * `session_root` — `$HOME/.local/share/opencode/storage/session`,
//!   resolved lazily so tests that override `$HOME` see the override.
//!   Mirrors the TS sibling's `path.join(homedir(), '.local', 'share',
//!   'opencode', 'storage', 'session')` exactly.
//! * `ingest_sessions` — opens a fresh ledger handle and runs
//!   [`relayburn_sdk::ingest_opencode_sessions`] (the opencode-only ingest
//!   pass). The TS sibling calls `ingestOpencodeSessions()` directly here;
//!   the Rust SDK function takes `&mut Ledger`, so the closure opens a
//!   handle each call. That mirrors the TS lock-then-write-then-close
//!   shape, and the per-tick open is cheap (SQLite WAL, no DDL after first
//!   open).
//!
//! The factory's [`super::pending_stamp::adapter_static`] does the
//! `Box::leak` so the registry can store the result as
//! `&'static dyn HarnessAdapter`. See the factory module for the leak
//! rationale (codex/opencode are the only two callers; runtime cost is
//! a few dozen bytes per process).

use std::path::PathBuf;
use std::sync::Arc;

use relayburn_sdk::{ingest_opencode_sessions, Ledger, LedgerOpenOptions, RawIngestOptions};

use super::pending_stamp::{self, IngestSessionsFn, PendingStampAdapter};
use super::HarnessAdapter;

/// Resolve the opencode session-store root. Mirrors the TS sibling
/// (`path.join(homedir(), '.local', 'share', 'opencode', 'storage',
/// 'session')`) and the SDK's internal `opencode_sessions_dir` default.
/// Resolved on every call so tests that flip `$HOME` between runs see
/// the override.
fn opencode_sessions_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local")
        .join("share")
        .join("opencode")
        .join("storage")
        .join("session")
}

/// Build the [`PendingStampAdapter`] config for opencode. Exposed as a
/// constructor function (rather than a `static`) because the closure
/// captures and the `Arc<dyn Fn>`s inside don't fit a const initializer.
/// The registry calls this once and feeds the result to
/// [`pending_stamp::adapter_static`].
pub fn config() -> PendingStampAdapter {
    let session_root: Arc<dyn Fn() -> PathBuf + Send + Sync> = Arc::new(opencode_sessions_dir);
    let ingest_sessions: IngestSessionsFn = Arc::new(|| {
        Box::pin(async move {
            // Open a fresh ledger handle per tick. The TS sibling's
            // `ingestOpencodeSessions` does the same via `withLock('ledger', …)`;
            // SQLite WAL keeps the per-call open cheap (no DDL after first
            // open). Defaults pull `$RELAYBURN_HOME` (or `~/.relayburn`)
            // and the same per-harness session-store root the factory's
            // `session_root` closure resolves above.
            let mut handle = Ledger::open(LedgerOpenOptions::default())?;
            let opts = RawIngestOptions::default();
            ingest_opencode_sessions(handle.raw_mut(), &opts).await
        })
    });
    PendingStampAdapter::new("opencode", session_root, ingest_sessions)
}

/// Convenience: hand out a `&'static dyn HarnessAdapter` for the opencode
/// adapter. The registry calls this once at lazy-init time. See
/// [`pending_stamp::adapter_static`] for the leak semantics — opencode is
/// one of two callers and the leaked footprint is bytes, not megabytes.
pub fn adapter() -> &'static dyn HarnessAdapter {
    pending_stamp::adapter_static(config())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize tests that mutate `$HOME` so a parallel test (in this
    /// module or another using the same env var) can't observe a
    /// half-set state. Mirrors the `ENV_LOCK` pattern in
    /// `relayburn_sdk::query_verbs::state_status` tests and the codex
    /// sibling. Poisoned-mutex recovery is intentional — a panicking
    /// test shouldn't break every subsequent run.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Runs `f` with `$HOME` pinned to `home`, restoring (or removing)
    /// the prior value before returning. Holds `ENV_LOCK` for the whole
    /// closure so concurrent tests serialize on the env mutation.
    fn with_test_home(home: &str, f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// `config()` returns a `PendingStampAdapter` named `opencode` with
    /// the standard 1s tick interval. Sanity check that the constructor
    /// wires the name through the factory contract and that the
    /// `session_root` closure resolves to the TS-mirrored path.
    #[test]
    fn config_has_opencode_name() {
        let cfg = config();
        assert_eq!(cfg.name, "opencode");
        // session_root closure resolves to
        // `$HOME/.local/share/opencode/storage/session`. Use a controlled
        // $HOME so the assertion doesn't depend on the developer's actual
        // home dir; restored after via `with_test_home`.
        with_test_home("/tmp/burn-opencode-test-home", || {
            let resolved = (cfg.session_root)();
            assert_eq!(
                resolved,
                PathBuf::from(
                    "/tmp/burn-opencode-test-home/.local/share/opencode/storage/session"
                )
            );
        });
    }

    /// `adapter()` round-trips through the trait surface — name, session
    /// root, and the `&'static` lifetime the registry requires. Mirrors
    /// the registry's `pending_stamp_adapter_static_fits_runtime_registry`
    /// check, but pinned to the opencode configuration specifically.
    #[test]
    fn adapter_round_trip() {
        let a: &'static dyn HarnessAdapter = adapter();
        assert_eq!(a.name(), "opencode");
        with_test_home("/tmp/burn-opencode-test-home", || {
            assert_eq!(
                a.session_root(),
                PathBuf::from(
                    "/tmp/burn-opencode-test-home/.local/share/opencode/storage/session"
                )
            );
        });
    }
}
