//! Shared test-only env-mutation helper for harness tests.
//!
//! `HOME` is process-global, so a per-module `Mutex` (one in `codex.rs`,
//! one in `opencode.rs`, …) doesn't actually serialize anything — `cargo
//! test` runs modules in parallel and a `set_var("HOME", …)` in one
//! module's test can be observed by `(cfg.session_root)()` in another.
//!
//! This module hosts a single crate-wide [`ENV_LOCK`] and a
//! [`with_test_home`] helper that all harness tests should funnel
//! through. As a side benefit, the helper also normalizes the
//! save-set-restore-resume_unwind pattern so individual adapters don't
//! re-derive it (and don't drift on the unwind-safety details — a
//! panicking assertion still must restore `HOME` before propagating).
//!
//! Scope: harness-side `HOME` mutation only. SDK-side `RELAYBURN_*` env
//! tests carry their own lock in `relayburn_sdk::query_verbs`. A future
//! workspace-wide consolidation could lift both into a shared
//! `dev-dependencies` test-utility crate, but that's a deliberate
//! follow-up — keeping the scope tight here.

use std::sync::Mutex;

/// Crate-wide lock for any harness test that mutates `HOME`. Poisoned-
/// mutex recovery is intentional — a panicking test shouldn't break
/// every subsequent run.
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with `$HOME` pinned to `home`, restoring (or removing) the
/// prior value before returning. Holds [`ENV_LOCK`] for the whole
/// closure so concurrent harness tests serialize on the env mutation.
///
/// Wraps `f` in [`std::panic::catch_unwind`] so an assertion failure
/// inside the closure still restores `HOME` before the panic
/// propagates — without this, a failed test would leak its sentinel
/// `HOME` value into whichever test acquired the lock next.
pub(crate) fn with_test_home(home: &str, f: impl FnOnce()) {
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
