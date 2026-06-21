//! Crate-internal helpers shared across reader / analyze / query modules.
//!
//! Modules here are deliberately `pub(crate)`; they do not appear on the
//! published SDK surface. New helpers should live here only if they're
//! genuinely cross-module — single-module utilities belong with their
//! caller.

use std::path::PathBuf;

pub(crate) mod time;

/// Resolve the user's home directory for locating harness data/config
/// directories (`~/.claude`, `~/.codex`, …).
///
/// Honors `HOME` (POSIX) so tests can inject an isolated home dir; falls back
/// to `USERPROFILE` for parity with Node's `os.homedir()` on Windows, where
/// `HOME` is typically unset; then to `.` as a last resort. This is distinct
/// from [`crate::ledger::ledger_home`], which resolves `RELAYBURN_HOME` for
/// burn's own data root.
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
