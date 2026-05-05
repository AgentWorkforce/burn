//! Directory walkers — Rust port of `packages/ingest/src/walk.ts`.
//!
//! These are non-recursive iterative DFS walks that match the TS adapter's
//! semantics: missing or unreadable directories yield empty (silent skip),
//! ordering is filesystem-defined (we don't sort).

use std::fs;
use std::path::{Path, PathBuf};

/// Collect every `*.jsonl` file under `root`, recursing through every
/// subdirectory. Mirrors `walkJsonl` in the TS adapter — used by Codex
/// rollouts (`~/.codex/sessions/**/*.jsonl`).
pub fn walk_jsonl<P: AsRef<Path>>(root: P) -> Vec<PathBuf> {
    walk_files(root.as_ref(), |name| name.ends_with(".jsonl"))
}

/// Collect every `ses_*.json` file under `root`. Mirrors
/// `walkOpencodeSessions` — OpenCode names session files
/// `ses_<base32>.json` and stores per-session message logs in a sibling
/// `message/<sessionId>/` directory the ingest pass walks separately.
pub fn walk_opencode_sessions<P: AsRef<Path>>(root: P) -> Vec<PathBuf> {
    walk_files(root.as_ref(), |name| {
        name.starts_with("ses_") && name.ends_with(".json")
    })
}

fn walk_files(root: &Path, accept: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = dir.join(entry.file_name());
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if accept(name_str) {
                    out.push(path);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, File};
    use std::io::Write;
    use tempfile::TempDir;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            create_dir_all(parent).unwrap();
        }
        File::create(p).unwrap().write_all(b"").unwrap();
    }

    #[test]
    fn walk_jsonl_recurses_and_filters() {
        let dir = TempDir::new().unwrap();
        touch(&dir.path().join("a.jsonl"));
        touch(&dir.path().join("nested/b.jsonl"));
        touch(&dir.path().join("nested/deep/c.jsonl"));
        touch(&dir.path().join("not-this.json"));
        touch(&dir.path().join("nested/skip.txt"));

        let mut got = walk_jsonl(dir.path());
        got.sort();
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|p| p.extension().unwrap() == "jsonl"));
    }

    #[test]
    fn walk_opencode_sessions_matches_ses_prefix() {
        let dir = TempDir::new().unwrap();
        touch(&dir.path().join("ses_aaa.json"));
        touch(&dir.path().join("nested/ses_bbb.json"));
        touch(&dir.path().join("not-prefixed.json"));
        touch(&dir.path().join("ses_skip.jsonl"));

        let mut got = walk_opencode_sessions(dir.path());
        got.sort();
        assert_eq!(got.len(), 2);
        for p in &got {
            let name = p.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("ses_") && name.ends_with(".json"));
        }
    }

    #[test]
    fn missing_root_returns_empty() {
        let dir = TempDir::new().unwrap();
        let absent = dir.path().join("does-not-exist");
        assert!(walk_jsonl(&absent).is_empty());
        assert!(walk_opencode_sessions(&absent).is_empty());
    }
}
