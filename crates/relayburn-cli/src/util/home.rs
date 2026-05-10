//! Resolve the user's `$HOME` directory with a `.` fallback.
//!
//! Hoisted out of `harnesses/{claude,codex,opencode}.rs` per issue #328 —
//! each adapter previously carried its own copy of this exact snippet.
//! Resolved on every call so tests that flip `$HOME` between runs (see
//! [`crate::harnesses::test_env::with_test_home`]) see the override.

use std::path::PathBuf;

/// Return `$HOME` as a `PathBuf`, falling back to `"."` when the env
/// var is unset.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
