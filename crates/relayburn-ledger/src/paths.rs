//! Path layout under `RELAYBURN_HOME` (defaults to `~/.relayburn`).
//!
//! Mirrors `packages/ledger/src/paths.ts` — every helper resolves env at call
//! time so tests that swap `RELAYBURN_HOME` between calls don't need to bust
//! a cache.

use std::path::PathBuf;

use crate::errors::LedgerError;

pub fn ledger_home() -> PathBuf {
    if let Ok(env) = std::env::var("RELAYBURN_HOME") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".relayburn")
}

pub fn ledger_path() -> PathBuf {
    ledger_home().join("ledger.jsonl")
}

pub fn hwm_path() -> PathBuf {
    ledger_home().join("hwm.json")
}

pub fn cursors_path() -> PathBuf {
    ledger_home().join("cursors.json")
}

pub fn ledger_index_path() -> PathBuf {
    ledger_home().join("ledger.idx")
}

pub fn ledger_content_index_path() -> PathBuf {
    ledger_home().join("ledger.content.idx")
}

pub fn lock_path(name: &str) -> PathBuf {
    ledger_home().join(format!("{name}.lock"))
}

pub fn archive_path() -> PathBuf {
    ledger_home().join("archive.sqlite")
}

pub fn content_dir() -> PathBuf {
    ledger_home().join("content")
}

const MAX_SESSION_ID_LENGTH: usize = 128;

pub fn is_valid_session_id(session_id: &str) -> bool {
    if session_id.is_empty() || session_id.len() > MAX_SESSION_ID_LENGTH {
        return false;
    }
    if session_id == "." || session_id == ".." {
        return false;
    }
    let bytes = session_id.as_bytes();
    let first = bytes[0];
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn content_file_path(session_id: &str) -> Result<PathBuf, LedgerError> {
    if !is_valid_session_id(session_id) {
        return Err(LedgerError::InvalidSessionId(session_id.to_string()));
    }
    Ok(content_dir().join(format!("{session_id}.jsonl")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_session_ids() {
        assert!(is_valid_session_id("abc-123"));
        assert!(is_valid_session_id("ses_abcDEF"));
        assert!(is_valid_session_id("a"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("."));
        assert!(!is_valid_session_id(".."));
        assert!(!is_valid_session_id("../escape"));
        assert!(!is_valid_session_id(".dotleader"));
        assert!(!is_valid_session_id("has space"));
        assert!(!is_valid_session_id(&"a".repeat(129)));
    }

    #[test]
    fn content_path_rejects_bad_ids() {
        assert!(content_file_path("..").is_err());
        assert!(content_file_path("ok-id").is_ok());
    }
}
