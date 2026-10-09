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

use super::fixtures::{fixtures_root, render_value, snapshot_dir};
use super::relayhistory::records_from_evidence;

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
        staged.push(Staged {
            prefix: format!("claude-{}", name_of(&path)),
            home,
        });
    }
    for path in sorted(&root.join("codex")) {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".codex/sessions/2026/04/20");
        std::fs::create_dir_all(&dir).unwrap();
        let file = format!("rollout-2026-04-20T00-00-00-{}.jsonl", name_of(&path));
        std::fs::copy(&path, dir.join(file)).unwrap();
        staged.push(Staged {
            prefix: format!("codex-{}", name_of(&path)),
            home,
        });
    }
    for dir in sorted(&root.join("opencode")) {
        let home = tempfile::tempdir().unwrap();
        copy_tree(
            &dir.join("storage"),
            &home.path().join(".local/share/opencode/storage"),
        );
        staged.push(Staged {
            prefix: format!("opencode-{}", name_of(&dir)),
            home,
        });
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
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
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
            failures
                .entry(name.clone())
                .or_default()
                .push("no session mapped".into());
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

/// `storage`'s OpenCode JSON tree loaded into an `opencode.db` at `db`,
/// the layout current OpenCode releases write: one row per session,
/// message and part, each carrying the provider's JSON as `data`.
fn write_opencode_db(storage: &Path, db: &Path) {
    let read = |path: &Path| -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    };
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT,
                               time_created INTEGER, time_updated INTEGER);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
                               time_created INTEGER, data TEXT);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                            time_created INTEGER, data TEXT);",
    )
    .unwrap();
    for scope in sorted(&storage.join("session")) {
        for path in sorted(&scope) {
            let s = read(&path);
            conn.execute(
                "INSERT INTO session VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    s["id"].as_str(),
                    s["parentID"].as_str(),
                    s["directory"].as_str(),
                    s["time"]["created"].as_i64(),
                    s["time"]["updated"].as_i64(),
                ],
            )
            .unwrap();
        }
    }
    for session in sorted(&storage.join("message")) {
        for path in sorted(&session) {
            let m = read(&path);
            conn.execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    m["id"].as_str(),
                    m["sessionID"].as_str(),
                    m["time"]["created"].as_i64(),
                    m.to_string(),
                ],
            )
            .unwrap();
        }
    }
    for message in sorted(&storage.join("part")) {
        for path in sorted(&message) {
            let p = read(&path);
            conn.execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, NULL, ?4)",
                rusqlite::params![
                    p["id"].as_str(),
                    p["messageID"].as_str(),
                    p["sessionID"].as_str(),
                    p.to_string(),
                ],
            )
            .unwrap();
        }
    }
}

/// An OpenCode session maps to the same records whether relayhistory read
/// it from the legacy JSON tree or from `opencode.db`.
#[test]
fn opencode_db_sessions_map_like_the_json_tree() {
    let mut compared = 0;
    for tree in stage_corpus() {
        if !tree.prefix.starts_with("opencode-") {
            continue;
        }
        let db = Staged {
            prefix: tree.prefix.clone(),
            home: tempfile::tempdir().unwrap(),
        };
        write_opencode_db(
            &tree.home.path().join(".local/share/opencode/storage"),
            &db.home.path().join(".local/share/opencode/opencode.db"),
        );
        let (from_tree, from_db) = (mapped(&tree), mapped(&db));
        assert_eq!(
            from_tree.keys().collect::<Vec<_>>(),
            from_db.keys().collect::<Vec<_>>()
        );
        for (name, records) in &from_tree {
            assert_eq!(records, &from_db[name], "{name}");
            compared += 1;
        }
    }
    assert_eq!(compared, 6);
}
