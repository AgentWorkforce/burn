//! Filesystem layout for the relayburn ledger under `RELAYBURN_HOME`.
//!
//! The 2.0 layout is two SQLite databases at well-known names:
//!
//! ```text
//! $RELAYBURN_HOME/
//!     burn.sqlite      # events + stamps + archive_state
//!     content.sqlite   # content blobs + FTS5 index
//! ```
//!
//! `$RELAYBURN_HOME` defaults to `~/.agentworkforce/burn` so the Rust 2.0
//! port and the TS 1.x package (still at `~/.relayburn`) can coexist on
//! disk during the #249 cutover.
//!
//! Both paths are overridable via env vars so they can live on different
//! mounts (e.g. cheaper/bigger storage for `content.sqlite`). See the
//! redesign issue for the rationale behind splitting them.

use std::env;
use std::path::PathBuf;

/// `$RELAYBURN_HOME`, defaulting to `~/.agentworkforce/burn`. Reads the
/// env var on every call so test harnesses can flip it between cases
/// without process restart.
pub fn ledger_home() -> PathBuf {
    if let Ok(env) = env::var("RELAYBURN_HOME") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut p = PathBuf::from(home);
    p.push(".agentworkforce");
    p.push("burn");
    p
}

/// Path to the events DB. `RELAYBURN_SQLITE_PATH` overrides the default
/// `$RELAYBURN_HOME/burn.sqlite`.
pub fn burn_sqlite_path() -> PathBuf {
    if let Ok(env) = env::var("RELAYBURN_SQLITE_PATH") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    ledger_home().join("burn.sqlite")
}

/// Path to the content DB. `RELAYBURN_CONTENT_PATH` overrides the default
/// `$RELAYBURN_HOME/content.sqlite`.
pub fn content_sqlite_path() -> PathBuf {
    if let Ok(env) = env::var("RELAYBURN_CONTENT_PATH") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    ledger_home().join("content.sqlite")
}

/// Bound on session-id length so any path we ever derive from one (e.g.
/// content paths in 1.x) stays well under filesystem name limits. Kept
/// in 2.0 even though session ids no longer hit the filesystem — they
/// still flow into stamps, indexes, and exports that benefit from a
/// stable upper bound.
const MAX_SESSION_ID_LENGTH: usize = 128;

/// True iff the session id consists of safe identifier characters and
/// is bounded in length.
///
/// Mirrors the TS `isValidSessionId` so cross-tree comparisons (e.g.
/// `burn export ledger` round-tripping) accept the same id space.
pub fn is_valid_session_id(session_id: &str) -> bool {
    let bytes = session_id.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_SESSION_ID_LENGTH {
        return false;
    }
    if session_id == "." || session_id == ".." {
        return false;
    }
    let first = bytes[0];
    if !is_id_alnum(first) {
        return false;
    }
    bytes.iter().all(|&b| is_id_char(b))
}

fn is_id_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn is_id_char(b: u8) -> bool {
    is_id_alnum(b) || matches!(b, b'.' | b'_' | b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialises tests that mutate `RELAYBURN_HOME` / `HOME` so they
    /// don't trample one another (cargo runs tests in parallel by
    /// default; env vars are process-global).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn ledger_home_defaults_to_agentworkforce_burn_under_home() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = env::var("HOME").ok();
        let prev_relayburn = env::var("RELAYBURN_HOME").ok();
        env::remove_var("RELAYBURN_HOME");
        env::set_var("HOME", "/tmp/burn-paths-test-home");

        let p = ledger_home();
        assert_eq!(p, PathBuf::from("/tmp/burn-paths-test-home/.agentworkforce/burn"));

        match prev_home {
            Some(v) => env::set_var("HOME", v),
            None => env::remove_var("HOME"),
        }
        if let Some(v) = prev_relayburn {
            env::set_var("RELAYBURN_HOME", v);
        }
    }

    #[test]
    fn ledger_home_env_var_override_takes_precedence() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_relayburn = env::var("RELAYBURN_HOME").ok();
        env::set_var("RELAYBURN_HOME", "/tmp/explicit-burn-home");

        let p = ledger_home();
        assert_eq!(p, PathBuf::from("/tmp/explicit-burn-home"));

        match prev_relayburn {
            Some(v) => env::set_var("RELAYBURN_HOME", v),
            None => env::remove_var("RELAYBURN_HOME"),
        }
    }

    #[test]
    fn rejects_traversal_and_empty() {
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("."));
        assert!(!is_valid_session_id(".."));
        assert!(!is_valid_session_id("/etc/passwd"));
        assert!(!is_valid_session_id("a/b"));
        assert!(!is_valid_session_id("\0"));
    }

    #[test]
    fn accepts_real_session_ids() {
        assert!(is_valid_session_id(
            "0a1b2c3d-4e5f-6789-abcd-ef0123456789"
        ));
        assert!(is_valid_session_id("ses_abc123"));
        assert!(is_valid_session_id("sess_x"));
        assert!(is_valid_session_id("turn_42"));
    }

    #[test]
    fn rejects_overlong() {
        let long: String = "a".repeat(129);
        assert!(!is_valid_session_id(&long));
    }
}
