//! Parity: the records burn maps from relayhistory evidence must equal the
//! characterization snapshots the builtin readers produced for the same
//! fixture. Each fixture is staged into its own throwaway HOME, synced
//! through `ai_hist::SessionStore`, and mapped with [`super::relayhistory`].
//!
//! Staging points the fixtures' `/tmp/project` cwd fields at an empty
//! directory inside that HOME, so project resolution never reads the host
//! filesystem; rendering maps the staged cwd back to `/tmp/project`.
//!
//! `PARITY_REPORT=1` prints every differing field instead of failing fast.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ai_hist::{CatalogQuery, ProviderRoots, SessionQuery, SessionStore, StoreOptions};
use serde_json::Value;

use super::fixtures::{fixtures_root, render_value, snapshot_dir};
use super::relayhistory::records_from_evidence;
use super::Billing;

/// The cwd every fixture records.
const FIXTURE_CWD: &str = "/tmp/project";

/// Fields that name a session's working directory. Only these are pointed
/// at the staged cwd; a tool argument that mentions the path keeps its
/// bytes, and with them its `argsHash`.
const CWD_FIELDS: [&str; 3] = ["cwd", "directory", "root"];

/// One staged fixture: the snapshot name prefix, a populated HOME, and the
/// empty directory the fixture's cwd fields point at.
struct Staged {
    prefix: String,
    home: tempfile::TempDir,
    cwd: PathBuf,
}

impl Staged {
    fn new(prefix: String) -> Self {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        Self { prefix, home, cwd }
    }

    fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }

    /// Copy `src` to `dst` with its cwd fields pointed at [`Self::cwd`].
    fn copy(&self, src: &Path, dst: &Path) {
        let text = std::fs::read_to_string(src).unwrap();
        std::fs::write(dst, point_cwd_fields(text, &self.cwd())).unwrap();
    }

    fn copy_tree(&self, src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let path = entry.unwrap().path();
            let target = dst.join(path.file_name().unwrap());
            if path.is_dir() {
                self.copy_tree(&path, &target);
            } else {
                self.copy(&path, &target);
            }
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
        let fixture = Staged::new(format!("claude-{}", name_of(&path)));
        let dir = fixture.home.path().join(".claude/projects/-tmp-project");
        std::fs::create_dir_all(&dir).unwrap();
        fixture.copy(&path, &dir.join(path.file_name().unwrap()));
        staged.push(fixture);
    }
    for path in sorted(&root.join("codex")) {
        let fixture = Staged::new(format!("codex-{}", name_of(&path)));
        let dir = fixture.home.path().join(".codex/sessions/2026/04/20");
        std::fs::create_dir_all(&dir).unwrap();
        let file = format!("rollout-2026-04-20T00-00-00-{}.jsonl", name_of(&path));
        fixture.copy(&path, &dir.join(file));
        staged.push(fixture);
    }
    for dir in sorted(&root.join("opencode")) {
        let fixture = Staged::new(format!("opencode-{}", name_of(&dir)));
        fixture.copy_tree(
            &dir.join("storage"),
            &fixture.home.path().join(".local/share/opencode/storage"),
        );
        staged.push(fixture);
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
        let records = records_from_evidence(&evidence, &Billing::settled());
        assert_session_paths(&name, &records, &evidence);
        out.insert(
            name,
            render_value(
                &records,
                &[(&staged.cwd(), FIXTURE_CWD), (home, "<fixtures>")],
            ),
        );
    }
    out
}

/// The snapshots leave `sessionPath` out (the builtin readers ran without
/// one), so it is checked here: every turn names the transcript the
/// session was read from, and that file exists.
fn assert_session_paths(
    name: &str,
    records: &super::SessionRecords,
    evidence: &ai_hist::SessionEvidence,
) {
    let source = evidence
        .session
        .raw_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    assert!(source.is_some(), "{name}: session has no transcript path");
    for turn in &records.turns {
        assert_eq!(
            turn.session_path, source,
            "{name}: {} sessionPath",
            turn.message_id
        );
        let path = turn.session_path.as_deref().unwrap_or_default();
        assert!(
            Path::new(path).exists(),
            "{name}: sessionPath {path} does not exist"
        );
    }
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
        // The database carries the tree's staged cwd.
        let db = Staged {
            prefix: tree.prefix.clone(),
            home: tempfile::tempdir().unwrap(),
            cwd: tree.cwd(),
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

/// Positive control for the hermetic staging: project resolution reads the
/// staged cwd, so a repository there sets every turn's `projectKey`, while
/// the empty staged cwd the parity corpus uses leaves it unset whatever
/// exists at `/tmp/project` on the host.
#[test]
fn project_resolution_reads_the_staged_cwd() {
    let project_keys = |with_repo: bool| -> Vec<Value> {
        let staged = stage_corpus()
            .into_iter()
            .find(|s| s.prefix == "claude-simple-turn")
            .unwrap();
        if with_repo {
            let git = staged.cwd().join(".git");
            std::fs::create_dir_all(&git).unwrap();
            std::fs::write(
                git.join("config"),
                "[remote \"origin\"]\n\turl = git@github.com:example/staged.git\n",
            )
            .unwrap();
        }
        let records = &mapped(&staged)["claude-simple-turn"];
        let turns = records["turns"].as_array().unwrap();
        assert!(!turns.is_empty());
        for turn in turns {
            assert_eq!(turn["project"], FIXTURE_CWD);
        }
        turns.iter().map(|t| t["projectKey"].clone()).collect()
    };
    assert!(project_keys(false).iter().all(Value::is_null));
    assert!(project_keys(true)
        .iter()
        .all(|k| k == "github.com/example/staged"));
}
