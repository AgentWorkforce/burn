//! Characterization lock: the `SessionRecords` burn derives from every
//! fixture in `tests/fixtures/{claude,codex,opencode}`, snapshotted under
//! `tests/fixtures/sourcing-snapshots/`. Any change to what burn derives
//! from a session shows up as a snapshot diff.
//!
//! The corpus is staged into a temp directory first, with the fixtures'
//! `/tmp/project` cwd pointed at an empty directory, so project resolution
//! never sees the host filesystem.
//!
//! `session_path` stays unset: given a path, the Claude reader takes a
//! session's identity from the file name, which in production is the
//! session id and in this corpus is a descriptive fixture name.
//!
//! Regenerate with `UPDATE_SOURCING_SNAPSHOTS=1 cargo test -p relayburn-sdk sourcing_snapshots`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::SessionRecords;
use crate::ingest::walk::walk_opencode_sessions;
use crate::reader::{
    parse_claude_session, parse_codex_session_incremental, parse_opencode_session_incremental,
    ClaudeParseOptions, ContentStoreMode, ParseCodexIncrementalOptions,
    ParseOpencodeIncrementalOptions,
};

/// The cwd every fixture records.
const FIXTURE_CWD: &str = "/tmp/project";

/// Fields that name a session's working directory. Only these are pointed at
/// the empty staged directory; a tool argument that mentions the path keeps
/// its bytes, and with them its `argsHash`.
const CWD_FIELDS: [&str; 3] = ["cwd", "directory", "root"];

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn snapshot_dir() -> PathBuf {
    fixtures_root().join("sourcing-snapshots")
}

/// The corpus copied under `files`, with `FIXTURE_CWD` rewritten to `cwd`,
/// an empty directory with no repository above it.
struct Staged {
    _root: tempfile::TempDir,
    files: PathBuf,
    cwd: PathBuf,
}

fn stage() -> Staged {
    let root = tempfile::tempdir().unwrap();
    let files = root.path().join("fixtures");
    let cwd = root.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    for kind in ["claude", "codex", "opencode"] {
        copy_rewritten(&fixtures_root().join(kind), &files.join(kind), &cwd);
    }
    Staged {
        _root: root,
        files,
        cwd,
    }
}

fn copy_rewritten(src: &Path, dst: &Path, cwd: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        let target = dst.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_rewritten(&path, &target, cwd);
        } else {
            let text = std::fs::read_to_string(&path).unwrap();
            std::fs::write(&target, point_cwd_fields(text, cwd)).unwrap();
        }
    }
}

fn point_cwd_fields(mut text: String, cwd: &Path) -> String {
    let staged = cwd.to_string_lossy();
    for field in CWD_FIELDS {
        for separator in [":", ": "] {
            let from = format!("\"{field}\"{separator}\"{FIXTURE_CWD}\"");
            let to = format!("\"{field}\"{separator}\"{staged}\"");
            text = text.replace(&from, &to);
        }
    }
    text
}

fn claude_records(path: &Path) -> SessionRecords {
    let parsed = parse_claude_session(
        path,
        &ClaudeParseOptions {
            session_path: None,
            content_mode: Some(ContentStoreMode::Full),
            file_session_id: None,
        },
    )
    .unwrap();
    SessionRecords {
        turns: parsed.turns,
        content: parsed.content,
        compactions: parsed.events,
        relationships: parsed.relationships,
        tool_result_events: parsed.tool_result_events,
        user_turns: parsed.user_turns,
        request_id_lookup: parsed.request_id_lookup,
    }
}

fn codex_records(path: &Path) -> SessionRecords {
    let parsed = parse_codex_session_incremental(
        path,
        &ParseCodexIncrementalOptions {
            session_path: None,
            content_mode: Some(ContentStoreMode::Full),
            start_offset: Some(0),
            ..Default::default()
        },
    )
    .unwrap();
    SessionRecords {
        turns: parsed.turns,
        content: parsed.content,
        compactions: parsed.events,
        relationships: parsed.relationships,
        tool_result_events: parsed.tool_result_events,
        user_turns: parsed.user_turns,
        request_id_lookup: Default::default(),
    }
}

fn opencode_records(path: &Path) -> SessionRecords {
    let parsed = parse_opencode_session_incremental(
        path,
        &ParseOpencodeIncrementalOptions {
            session_path: None,
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();
    SessionRecords {
        turns: parsed.turns,
        content: parsed.content,
        compactions: parsed.events,
        relationships: parsed.relationships,
        tool_result_events: parsed.tool_result_events,
        user_turns: parsed.user_turns,
        request_id_lookup: Default::default(),
    }
}

/// One fixture: its snapshot name and the records the builtin readers derive.
fn builtin_corpus(files: &Path) -> Vec<(String, SessionRecords)> {
    let mut out = Vec::new();
    for path in sorted_files(&files.join("claude"), "jsonl") {
        out.push((format!("claude-{}", stem(&path)), claude_records(&path)));
    }
    for path in sorted_files(&files.join("codex"), "jsonl") {
        out.push((format!("codex-{}", stem(&path)), codex_records(&path)));
    }
    let mut opencode_dirs: Vec<PathBuf> = std::fs::read_dir(files.join("opencode"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    opencode_dirs.sort();
    for dir in opencode_dirs {
        let mut sessions = walk_opencode_sessions(dir.join("storage/session"));
        sessions.sort();
        for session in sessions {
            let name = format!("opencode-{}-{}", stem(&dir), stem(&session));
            out.push((name, opencode_records(&session)));
        }
    }
    out
}

fn sorted_files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .collect();
    files.sort();
    files
}

fn stem(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().into_owned()
}

/// Serialize with the staged paths mapped back to stable placeholders: the
/// corpus copy becomes `<fixtures>`, the empty cwd becomes `FIXTURE_CWD`.
fn render(records: &SessionRecords, staged: &Staged) -> String {
    let mut json = serde_json::to_string_pretty(records).unwrap() + "\n";
    let replacements = [(&staged.files, "<fixtures>"), (&staged.cwd, FIXTURE_CWD)];
    for (path, placeholder) in replacements {
        let canonical = path.canonicalize().unwrap();
        for form in [canonical.as_path(), path.as_path()] {
            json = json.replace(&*form.to_string_lossy(), placeholder);
        }
    }
    json
}

/// Snapshot names already on disk.
fn existing_snapshots(dir: &Path) -> BTreeSet<String> {
    sorted_files(dir, "json").iter().map(|p| stem(p)).collect()
}

#[test]
fn sourcing_snapshots() {
    let update = std::env::var_os("UPDATE_SOURCING_SNAPSHOTS").is_some();
    let dir = snapshot_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let staged = stage();
    let corpus = builtin_corpus(&staged.files);
    let produced: BTreeSet<String> = corpus.iter().map(|(name, _)| name.clone()).collect();
    let stale: Vec<String> = existing_snapshots(&dir)
        .difference(&produced)
        .cloned()
        .collect();
    let mut mismatched = Vec::new();
    for (name, records) in &corpus {
        let path = dir.join(format!("{name}.json"));
        let rendered = render(records, &staged);
        if update {
            std::fs::write(&path, rendered).unwrap();
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(expected) if expected == rendered => {}
            _ => mismatched.push(name.clone()),
        }
    }
    if update {
        for name in &stale {
            std::fs::remove_file(dir.join(format!("{name}.json"))).unwrap();
        }
        return;
    }
    assert!(
        mismatched.is_empty() && stale.is_empty(),
        "sourcing snapshots differ or are missing: {mismatched:?}; snapshots without a fixture: {stale:?} \
         (rerun with UPDATE_SOURCING_SNAPSHOTS=1 to accept)"
    );
}
