//! Codex `HarnessAdapter` — Rust port of `packages/cli/src/harnesses/codex.ts`.
//!
//! Codex shares the pending-stamp + watch-loop shape with OpenCode, so the
//! adapter is constructed via [`super::pending_stamp::session_store_adapter`]
//! instead of re-implementing the trait. The only codex-specific bits are:
//!
//! * `name = "codex"` — the dispatch key and log-line label.
//! * `session_root` — `$HOME/.codex/sessions`, resolved lazily so tests
//!   that override `$HOME` see the override.
//! * `ingest_sessions` — defers to [`relayburn_sdk::ingest_codex_sessions`],
//!   the codex-only ingest pass. The factory opens a fresh ledger handle
//!   per call (mirrors the TS lock-then-write-then-close shape; SQLite WAL
//!   keeps the per-tick open cheap).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use relayburn_sdk::{ingest_codex_sessions, IngestReport, RawIngestOptions, RawLedger};

use super::pending_stamp;
use super::HarnessAdapter;
use crate::util::home::home_dir;

/// `$HOME/.codex/sessions`. Mirrors the TS sibling
/// (`path.join(homedir(), '.codex', 'sessions')`) and the SDK's internal
/// `codex_sessions_dir` default.
fn codex_sessions_dir() -> PathBuf {
    home_dir().join(".codex").join("sessions")
}

/// Box-pin the SDK's `async fn ingest_codex_sessions` into a fn pointer
/// the [`pending_stamp::SessionIngestor`] type alias accepts.
fn codex_ingest<'a>(
    ledger: &'a mut RawLedger,
    opts: &'a RawIngestOptions,
) -> Pin<Box<dyn Future<Output = anyhow::Result<IngestReport>> + Send + 'a>> {
    Box::pin(ingest_codex_sessions(ledger, opts))
}

/// Hand out a `&'static dyn HarnessAdapter` for codex. The registry calls
/// this once at lazy-init time. See
/// [`pending_stamp::session_store_adapter`] for the leak semantics.
pub fn adapter() -> &'static dyn HarnessAdapter {
    pending_stamp::session_store_adapter("codex", codex_sessions_dir, codex_ingest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harnesses::test_env::with_test_home;

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
