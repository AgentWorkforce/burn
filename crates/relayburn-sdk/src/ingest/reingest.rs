//! Codex session-id derivation used by the orchestrator. The richer
//! `reingestMissingContent` workflow from the TS port hasn't been wired
//! up yet on the Rust side; only the small filename-to-uuid helper that
//! `ingest::ingest` needs lives here.

use std::path::Path;

use crate::reader::read_codex_session_id_hint;

/// Map a Codex rollout file path back to its session id. The canonical
/// filename pattern is `rollout-<ts>-<uuid>.jsonl`; we lift the trailing
/// uuid off the stem without parsing. If the pattern doesn't match,
/// peek at Codex's first-line `session_meta` hint before falling back to
/// `None`.
///
/// Mirrors TS `deriveCodexSessionId`.
pub fn derive_codex_session_id(file: &Path) -> Option<String> {
    let stem = file.file_stem().and_then(|s| s.to_str())?;
    if let Some(uuid) = trailing_uuid(stem) {
        return Some(uuid);
    }
    read_codex_session_id_hint(file)
}

fn trailing_uuid(stem: &str) -> Option<String> {
    // Match the TS regex
    // `/([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$/`.
    if stem.len() < 36 {
        return None;
    }
    let candidate = &stem[stem.len() - 36..];
    let bytes = candidate.as_bytes();
    let dash_positions = [8, 13, 18, 23];
    for (i, b) in bytes.iter().enumerate() {
        if dash_positions.contains(&i) {
            if *b != b'-' {
                return None;
            }
        } else if !b.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn derive_codex_session_id_recognizes_canonical_filenames() {
        let p = PathBuf::from(
            "/x/rollout-2026-04-24T00-00-00-000Z-11111111-2222-3333-4444-555555555555.jsonl",
        );
        assert_eq!(
            derive_codex_session_id(&p),
            Some("11111111-2222-3333-4444-555555555555".to_string())
        );
    }

    #[test]
    fn derive_codex_session_id_returns_none_for_opaque_filename_when_file_missing() {
        let p = PathBuf::from("/tmp/this/path/should/not/exist-opaque.jsonl");
        assert_eq!(derive_codex_session_id(&p), None);
    }
}
