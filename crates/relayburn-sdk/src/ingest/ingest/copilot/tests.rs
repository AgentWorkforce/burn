//! Copilot ingest helper tests: cursor resume decisions, watch roots, and
//! source-fingerprint contributions.

use std::path::PathBuf;

use super::*;

fn cursor(inode: u64, offset_bytes: u64, mtime_ms: i64) -> CopilotCursor {
    CopilotCursor {
        inode,
        offset_bytes,
        mtime_ms,
        ..Default::default()
    }
}

#[test]
fn resume_offset_continues_unchanged_and_grown_files() {
    let prior = cursor(7, 100, 1_000);
    // Same size, same mtime: nothing new, but no rotation either.
    assert_eq!(resume_offset(&prior, 7, 1_000, 100), Some(100));
    // Appended bytes and a newer mtime.
    assert_eq!(resume_offset(&prior, 7, 2_000, 150), Some(100));
}

#[test]
fn resume_offset_restarts_rotated_files() {
    let prior = cursor(7, 100, 1_000);
    assert_eq!(resume_offset(&prior, 8, 1_000, 100), None, "new inode");
    assert_eq!(resume_offset(&prior, 7, 999, 100), None, "mtime regressed");
    assert_eq!(resume_offset(&prior, 7, 1_000, 99), None, "file shrank");
}

#[test]
fn watch_dirs_add_each_injected_parent_once() {
    let roots = IngestRoots {
        copilot_otel_files: Some(vec![
            PathBuf::from("/x/otel/a.jsonl"),
            PathBuf::from("/x/otel/b.jsonl"),
            PathBuf::from("/x/claude/c.jsonl"),
        ]),
        ..Default::default()
    };
    let mut dirs = vec![PathBuf::from("/x/claude")];
    push_watch_dirs(&roots, &mut dirs);
    assert_eq!(
        dirs,
        vec![PathBuf::from("/x/claude"), PathBuf::from("/x/otel")]
    );
}

#[test]
fn watch_dirs_default_to_the_copilot_otel_dir() {
    let mut dirs = Vec::new();
    push_watch_dirs(&IngestRoots::default(), &mut dirs);
    assert_eq!(dirs, vec![copilot_otel_dir()]);
    assert!(copilot_otel_dir().ends_with("otel"));
}

#[test]
fn fingerprint_counts_existing_export_files_only() {
    let tmp = tempfile::tempdir().unwrap();
    let present = tmp.path().join("copilot.jsonl");
    std::fs::write(&present, b"0123456789").unwrap();
    let roots = IngestRoots {
        copilot_otel_files: Some(vec![present, tmp.path().join("missing.jsonl")]),
        ..Default::default()
    };
    let (mut count, mut total_bytes, mut hash_sum) = (5, 7, 0);
    fingerprint_exports(&roots, &mut count, &mut total_bytes, &mut hash_sum);
    assert_eq!(count, 6);
    assert_eq!(total_bytes, 17);
    assert_ne!(hash_sum, 0);
}

#[test]
fn default_session_roots_list_harness_roots_then_copilot_exports() {
    let roots = IngestRoots {
        claude_projects_dir: Some(PathBuf::from("/x/claude")),
        codex_sessions_dir: Some(PathBuf::from("/x/codex")),
        opencode_storage_dir: Some(PathBuf::from("/x/opencode")),
        copilot_otel_files: Some(vec![PathBuf::from("/x/otel/copilot.jsonl")]),
    };
    assert_eq!(
        super::super::default_session_roots(&roots),
        ["/x/claude", "/x/codex", "/x/opencode", "/x/otel"]
            .map(PathBuf::from)
            .to_vec()
    );
}
