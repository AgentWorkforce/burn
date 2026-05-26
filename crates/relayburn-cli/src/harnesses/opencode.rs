//! OpenCode `HarnessAdapter` — Rust port of `packages/cli/src/harnesses/opencode.ts`.
//!
//! OpenCode shares the pending-stamp + watch-loop shape with codex, so the
//! adapter is constructed via [`super::pending_stamp::session_store_adapter`]
//! instead of re-implementing the trait. The only opencode-specific bits are:
//!
//! * `name = "opencode"` — the dispatch key and log-line label.
//! * `session_root` — `$HOME/.local/share/opencode/storage/session`,
//!   resolved lazily so tests that override `$HOME` see the override.
//!   Mirrors the TS sibling's `path.join(homedir(), '.local', 'share',
//!   'opencode', 'storage', 'session')` exactly.
//! * `ingest_sessions` — defers to
//!   [`relayburn_sdk::ingest_opencode_sessions`], the opencode-only ingest
//!   pass. The factory opens a fresh ledger handle per call (mirrors the
//!   TS lock-then-write-then-close shape; SQLite WAL keeps the per-tick
//!   open cheap). The SDK verb is sync, so we pass it directly as a fn
//!   pointer to [`pending_stamp::session_store_adapter`].

use std::path::PathBuf;

use relayburn_sdk::ingest_opencode_sessions;

use super::pending_stamp;
use super::HarnessAdapter;
use crate::util::home::home_dir;

/// `$HOME/.local/share/opencode/storage/session`. Mirrors the TS sibling
/// and the SDK's internal `opencode_sessions_dir` default.
fn opencode_sessions_dir() -> PathBuf {
    home_dir()
        .join(".local")
        .join("share")
        .join("opencode")
        .join("storage")
        .join("session")
}

/// Hand out a `&'static dyn HarnessAdapter` for opencode. The registry
/// calls this once at lazy-init time. See
/// [`pending_stamp::session_store_adapter`] for the leak semantics.
pub fn adapter() -> &'static dyn HarnessAdapter {
    pending_stamp::session_store_adapter(
        "opencode",
        opencode_sessions_dir,
        ingest_opencode_sessions,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harnesses::test_env::with_test_home;

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
                PathBuf::from("/tmp/burn-opencode-test-home/.local/share/opencode/storage/session")
            );
        });
    }
}
