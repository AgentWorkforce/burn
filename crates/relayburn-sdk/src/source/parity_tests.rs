//! Parity: the records burn maps from relayhistory evidence must equal the
//! characterization snapshots the builtin readers produced for the same
//! fixture. Each fixture is staged into its own throwaway HOME, synced
//! through `ai_hist::SessionStore`, and mapped with [`super::relayhistory`].
//!
//! `PARITY_REPORT=1` prints every differing field instead of failing fast.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ai_hist::{CatalogQuery, ProviderRoots, SessionQuery, SessionStore, StoreOptions};
use serde_json::Value;

use super::relayhistory::records_from_evidence;
use super::snapshot_tests::{fixtures_root, render_value, snapshot_dir};

/// One staged fixture: the snapshot name prefix and a populated HOME.
struct Staged {
    prefix: String,
    home: tempfile::TempDir,
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        let target = dst.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

fn sorted(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    entries
}

fn name_of(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().into_owned()
}

fn stage_corpus() -> Vec<Staged> {
    let root = fixtures_root();
    let mut staged = Vec::new();
    for path in sorted(&root.join("claude")) {
        if path.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/projects/-tmp-project");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&path, dir.join(path.file_name().unwrap())).unwrap();
        staged.push(Staged { prefix: format!("claude-{}", name_of(&path)), home });
    }
    for path in sorted(&root.join("codex")) {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".codex/sessions/2026/04/20");
        std::fs::create_dir_all(&dir).unwrap();
        let file = format!("rollout-2026-04-20T00-00-00-{}.jsonl", name_of(&path));
        std::fs::copy(&path, dir.join(file)).unwrap();
        staged.push(Staged { prefix: format!("codex-{}", name_of(&path)), home });
    }
    for dir in sorted(&root.join("opencode")) {
        let home = tempfile::tempdir().unwrap();
        copy_tree(
            &dir.join("storage"),
            &home.path().join(".local/share/opencode/storage"),
        );
        staged.push(Staged { prefix: format!("opencode-{}", name_of(&dir)), home });
    }
    staged
}

/// Snapshot name → rendered records mapped from relayhistory evidence.
fn mapped(staged: &Staged) -> BTreeMap<String, Value> {
    let home = staged.home.path();
    let mut options = StoreOptions::default();
    options.db_path = Some(home.join("ai-history.db"));
    options.roots = Some(ProviderRoots::from_home(
        home.to_path_buf(),
        home.join(".local/share/opencode/opencode.db"),
    ));
    let store = SessionStore::open(options).unwrap();
    store.sync(Default::default()).unwrap();
    let mut out = BTreeMap::new();
    for row in store.sessions(CatalogQuery::default()) {
        let row = row.unwrap();
        let evidence = store
            .session(&row.session_ref(), SessionQuery::default())
            .unwrap()
            .unwrap();
        let name = if staged.prefix.starts_with("opencode-") {
            format!("{}-{}", staged.prefix, row.session_id)
        } else {
            staged.prefix.clone()
        };
        let records = records_from_evidence(&evidence);
        out.insert(name, render_value(&records, home));
    }
    out
}

/// Every leaf path where `got` differs from `want`.
fn diff(path: &str, want: &Value, got: &Value, out: &mut Vec<String>) {
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => {
            for key in w.keys().chain(g.keys().filter(|k| !w.contains_key(*k))) {
                let null = Value::Null;
                diff(
                    &format!("{path}.{key}"),
                    w.get(key).unwrap_or(&null),
                    g.get(key).unwrap_or(&null),
                    out,
                );
            }
        }
        (Value::Array(w), Value::Array(g)) => {
            if w.len() != g.len() {
                out.push(format!("{path}: len {} != {}", w.len(), g.len()));
            }
            for (i, (a, b)) in w.iter().zip(g.iter()).enumerate() {
                diff(&format!("{path}[{i}]"), a, b, out);
            }
        }
        _ if want != got => out.push(format!("{path}: want {want} got {got}")),
        _ => {}
    }
}

#[test]
fn relayhistory_parity() {
    let mut failures: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut expected: BTreeMap<String, Value> = BTreeMap::new();
    for path in sorted(&snapshot_dir()) {
        let text = std::fs::read_to_string(&path).unwrap();
        expected.insert(name_of(&path), serde_json::from_str(&text).unwrap());
    }
    let mut seen = Vec::new();
    for staged in stage_corpus() {
        for (name, got) in mapped(&staged) {
            seen.push(name.clone());
            let Some(want) = expected.get(&name) else {
                failures.entry(name).or_default().push("no snapshot".into());
                continue;
            };
            let mut diffs = Vec::new();
            diff("", want, &got, &mut diffs);
            if !diffs.is_empty() {
                failures.insert(name, diffs);
            }
        }
    }
    for name in expected.keys() {
        if !seen.contains(name) {
            failures.entry(name.clone()).or_default().push("no session mapped".into());
        }
    }
    if std::env::var_os("PARITY_REPORT").is_some() {
        for (name, diffs) in &failures {
            println!("== {name} ({} diffs)", diffs.len());
            for d in diffs {
                println!("  {d}");
            }
        }
    }
    assert!(
        failures.is_empty(),
        "relayhistory parity failed for {} fixtures: {:?}",
        failures.len(),
        failures.keys().collect::<Vec<_>>()
    );
}
