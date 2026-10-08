//! Characterization lock: the `SessionRecords` burn derives from every
//! fixture in `tests/fixtures/{claude,codex,opencode}`, snapshotted under
//! `tests/fixtures/sourcing-snapshots/`. Any change to what burn derives
//! from a session shows up as a snapshot diff.
//!
//! Regenerate with `UPDATE_SOURCING_SNAPSHOTS=1 cargo test -p relayburn-sdk sourcing_snapshots`.

use std::path::{Path, PathBuf};

use super::SessionRecords;
use crate::ingest::walk::walk_opencode_sessions;
use crate::reader::{
    parse_claude_session, parse_codex_session_incremental, parse_opencode_session_incremental,
    ClaudeParseOptions, ContentStoreMode, ParseCodexIncrementalOptions,
    ParseOpencodeIncrementalOptions,
};

pub(super) fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

pub(super) fn snapshot_dir() -> PathBuf {
    fixtures_root().join("sourcing-snapshots")
}

/// One fixture: its snapshot name and the records the builtin readers derive.
fn builtin_corpus() -> Vec<(String, SessionRecords)> {
    let root = fixtures_root();
    let mut out = Vec::new();

    for path in sorted_files(&root.join("claude"), "jsonl") {
        let parsed = parse_claude_session(
            &path,
            &ClaudeParseOptions {
                session_path: None,
                content_mode: Some(ContentStoreMode::Full),
                file_session_id: None,
            },
        )
        .unwrap();
        out.push((
            format!("claude-{}", stem(&path)),
            SessionRecords {
                turns: parsed.turns,
                content: parsed.content,
                compactions: parsed.events,
                relationships: parsed.relationships,
                tool_result_events: parsed.tool_result_events,
                user_turns: parsed.user_turns,
                request_id_lookup: parsed.request_id_lookup,
            },
        ));
    }

    for path in sorted_files(&root.join("codex"), "jsonl") {
        let parsed = parse_codex_session_incremental(
            &path,
            &ParseCodexIncrementalOptions {
                content_mode: Some(ContentStoreMode::Full),
                start_offset: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        out.push((
            format!("codex-{}", stem(&path)),
            SessionRecords {
                turns: parsed.turns,
                content: parsed.content,
                compactions: parsed.events,
                relationships: parsed.relationships,
                tool_result_events: parsed.tool_result_events,
                user_turns: parsed.user_turns,
                request_id_lookup: Default::default(),
            },
        ));
    }

    let mut opencode_dirs: Vec<PathBuf> = std::fs::read_dir(root.join("opencode"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    opencode_dirs.sort();
    for dir in opencode_dirs {
        let mut sessions = walk_opencode_sessions(dir.join("storage/session"));
        sessions.sort();
        for session in sessions {
            let parsed = parse_opencode_session_incremental(
                &session,
                &ParseOpencodeIncrementalOptions {
                    content_mode: Some(ContentStoreMode::Full),
                    ..Default::default()
                },
            )
            .unwrap();
            out.push((
                format!("opencode-{}-{}", stem(&dir), stem(&session)),
                SessionRecords {
                    turns: parsed.turns,
                    content: parsed.content,
                    compactions: parsed.events,
                    relationships: parsed.relationships,
                    tool_result_events: parsed.tool_result_events,
                    user_turns: parsed.user_turns,
                    request_id_lookup: Default::default(),
                },
            ));
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

/// Serialize with machine-specific absolute paths replaced by `<fixtures>`.
fn render(records: &SessionRecords) -> String {
    serde_json::to_string_pretty(&render_value(records, &fixtures_root())).unwrap() + "\n"
}

/// `records` as JSON with every occurrence of `root` (and its canonical
/// form) replaced by `<fixtures>`, and `sessionPath` dropped: where a
/// session file lives is not something the readers derive.
pub(super) fn render_value(records: &SessionRecords, root: &Path) -> serde_json::Value {
    let mut json = serde_json::to_string(records).unwrap();
    let canonical = root.canonicalize().unwrap();
    for prefix in [canonical.as_path(), root] {
        json = json.replace(&*prefix.to_string_lossy(), "<fixtures>");
    }
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
    if let Some(turns) = value.get_mut("turns").and_then(|t| t.as_array_mut()) {
        for turn in turns {
            if let Some(obj) = turn.as_object_mut() {
                obj.remove("sessionPath");
            }
        }
    }
    value
}

#[test]
fn sourcing_snapshots() {
    let update = std::env::var_os("UPDATE_SOURCING_SNAPSHOTS").is_some();
    let dir = snapshot_dir();
    if update {
        std::fs::create_dir_all(&dir).unwrap();
    }
    let mut mismatched = Vec::new();
    for (name, records) in builtin_corpus() {
        let path = dir.join(format!("{name}.json"));
        let rendered = render(&records);
        if update {
            std::fs::write(&path, rendered).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing snapshot {}", path.display()));
        if expected != rendered {
            mismatched.push(name);
        }
    }
    assert!(
        mismatched.is_empty(),
        "sourcing snapshots differ (rerun with UPDATE_SOURCING_SNAPSHOTS=1 to accept): {mismatched:?}"
    );
}
