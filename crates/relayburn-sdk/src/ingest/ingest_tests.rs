//! Ingest through the relayhistory store: each test lays fixture sessions
//! out under a throwaway provider home, ingests into a throwaway ledger,
//! and checks what landed.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::pending_stamps::{write_pending_stamp, PendingStampHarness, WriteOptions};
use super::*;
use crate::ledger::{Enrichment, Ledger, LedgerLayout, Query};
use crate::source::fixtures::fixtures_root;
use crate::source::locate::HistoryStoreOptions;

/// A provider home, a relayhistory database and a ledger home, all
/// throwaway.
struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            dir: TempDir::new().unwrap(),
        }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn ledger_home(&self) -> PathBuf {
        self.dir.path().join("ledger")
    }

    fn options(&self) -> IngestOptions {
        IngestOptions {
            ledger_home: Some(self.ledger_home()),
            store: self.store(self.dir.path().join("ai-history.db")),
            ..Default::default()
        }
    }

    fn store(&self, db_path: PathBuf) -> HistoryStoreOptions {
        HistoryStoreOptions {
            db_path: Some(db_path),
            home: Some(self.home()),
        }
    }

    fn ledger(&self) -> Ledger {
        let layout = LedgerLayout::under(self.ledger_home());
        fs::create_dir_all(&layout.home).unwrap();
        Ledger::open(&layout.burn, &layout.content).unwrap()
    }

    fn claude_project(&self) -> PathBuf {
        let dir = self.home().join(".claude/projects/-tmp-project");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Copy `tests/fixtures/<fixture>` to `<home>/<dest>`.
    fn place(&self, fixture: &str, dest: &str) -> PathBuf {
        let target = self.home().join(dest);
        copy_tree(&fixtures_root().join(fixture), &target);
        target
    }

    fn ingest(&self, ledger: &mut Ledger) -> IngestReport {
        ingest_all(ledger, &self.options()).unwrap()
    }
}

fn copy_tree(src: &Path, dst: &Path) {
    if src.is_file() {
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::copy(src, dst).unwrap();
        return;
    }
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        copy_tree(&path, &dst.join(path.file_name().unwrap()));
    }
}

fn turn_ids(ledger: &Ledger, session_id: &str) -> Vec<String> {
    ledger
        .query_turns(&Query::for_session(session_id))
        .unwrap()
        .into_iter()
        .map(|t| t.turn.message_id)
        .collect()
}

const SIMPLE_SESSION: &str = "11111111-1111-1111-1111-111111111111";

/// A Claude transcript with one prompt and one assistant turn per entry of
/// `replies`. Each session writes in its own minute: the ledger bills a
/// turn identical in time, model and usage to one it holds only once.
fn claude_transcript(session_id: &str, cwd: &str, replies: &[&str]) -> String {
    let minute = session_id.as_bytes()[0] % 60;
    let mut lines = Vec::new();
    let mut parent: Option<String> = None;
    for (i, reply) in replies.iter().enumerate() {
        let user = format!("u-user-{i}");
        lines.push(serde_json::json!({
            "parentUuid": parent, "isSidechain": false, "type": "user",
            "message": {"role": "user", "content": format!("prompt {i}")},
            "uuid": user, "timestamp": format!("2026-04-22T00:{minute:02}:{:02}.000Z", i * 2),
            "cwd": cwd, "sessionId": session_id,
        }));
        let assistant = format!("u-asst-{i}");
        lines.push(serde_json::json!({
            "parentUuid": user, "isSidechain": false, "type": "assistant",
            "message": {
                "model": "claude-sonnet-4-6", "id": format!("msg-asst-{i}"),
                "type": "message", "role": "assistant",
                "content": [{"type": "text", "text": reply}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 5,
                          "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
            },
            "uuid": assistant, "timestamp": format!("2026-04-22T00:{minute:02}:{:02}.500Z", i * 2),
            "cwd": cwd, "sessionId": session_id,
        }));
        parent = Some(assistant);
    }
    lines.iter().map(|l| format!("{l}\n")).collect::<String>()
}

#[test]
fn a_claude_session_lands_once() {
    let sandbox = Sandbox::new();
    let file = sandbox
        .claude_project()
        .join(format!("{SIMPLE_SESSION}.jsonl"));
    fs::write(
        &file,
        claude_transcript(SIMPLE_SESSION, "/tmp/project", &["hi"]),
    )
    .unwrap();
    let mut ledger = sandbox.ledger();

    let first = sandbox.ingest(&mut ledger);
    assert_eq!(first.ingested_sessions, 1);
    assert_eq!(first.appended_turns, 1);
    assert_eq!(turn_ids(&ledger, SIMPLE_SESSION), ["msg-asst-0"]);
    assert!(
        ledger
            .query_inferences(&Query::for_session(SIMPLE_SESSION))
            .unwrap()
            .len()
            == 1,
        "inferences are rebuilt with the turns"
    );

    let again = sandbox.ingest(&mut ledger);
    assert_eq!(again, IngestReport::empty(), "nothing changed upstream");
}

#[test]
fn an_appended_session_adds_only_its_new_turns() {
    let sandbox = Sandbox::new();
    let file = sandbox
        .claude_project()
        .join(format!("{SIMPLE_SESSION}.jsonl"));
    fs::write(
        &file,
        claude_transcript(SIMPLE_SESSION, "/tmp/project", &["one"]),
    )
    .unwrap();
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);

    let transcript = claude_transcript(SIMPLE_SESSION, "/tmp/project", &["one", "two"]);
    fs::write(&file, transcript).unwrap();
    let report = sandbox.ingest(&mut ledger);
    assert_eq!(report.scanned_sessions, 1);
    assert_eq!(report.appended_turns, 1);
    assert_eq!(
        turn_ids(&ledger, SIMPLE_SESSION),
        ["msg-asst-0", "msg-asst-1"]
    );
}

#[test]
fn claude_subagent_sidecars_are_billed_to_the_session_that_spawned_them() {
    let sandbox = Sandbox::new();
    let project = ".claude/projects/-tmp-project";
    sandbox.place(
        "claude-sidecars/sidecar-session.jsonl",
        &format!("{project}/sidecar-session.jsonl"),
    );
    sandbox.place(
        "claude-sidecars/sidecar-session",
        &format!("{project}/sidecar-session"),
    );
    let mut ledger = sandbox.ledger();
    let report = sandbox.ingest(&mut ledger);

    assert_eq!(report.ingested_sessions, 1, "subagents are not sessions");
    let turns = ledger
        .query_turns(&Query::for_session("sidecar-session"))
        .unwrap();
    let agents: Vec<Option<String>> = turns
        .iter()
        .map(|t| t.turn.subagent.as_ref().and_then(|s| s.agent_id.clone()))
        .collect();
    assert!(agents.contains(&None), "the root's own turns");
    assert!(agents.contains(&Some("a1".into())));
    assert!(agents.contains(&Some("a2".into())));
    assert!(ledger
        .query_turns(&Query::for_session("a1"))
        .unwrap()
        .is_empty());
}

#[test]
fn a_codex_child_thread_is_a_session_of_its_own() {
    let sandbox = Sandbox::new();
    let day = ".codex/sessions/2026/04/23";
    sandbox.place(
        "codex/with-spawn-agent.jsonl",
        &format!("{day}/rollout-2026-04-23T00-00-00-parent.jsonl"),
    );
    sandbox.place(
        "codex-delegated/subagent-child.jsonl",
        &format!("{day}/rollout-2026-04-23T00-00-01-child.jsonl"),
    );
    let mut ledger = sandbox.ledger();
    let report = sandbox.ingest(&mut ledger);

    assert_eq!(report.ingested_sessions, 2);
    assert_eq!(turn_ids(&ledger, "sess_spawn_1").len(), 1);
    assert_eq!(turn_ids(&ledger, "agent_inv_42").len(), 1);
}

#[test]
fn a_codex_child_thread_written_later_lands_from_the_change_feed() {
    let sandbox = Sandbox::new();
    let day = ".codex/sessions/2026/04/23";
    sandbox.place(
        "codex/with-spawn-agent.jsonl",
        &format!("{day}/rollout-2026-04-23T00-00-00-parent.jsonl"),
    );
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    assert!(turn_ids(&ledger, "agent_inv_42").is_empty());

    sandbox.place(
        "codex-delegated/subagent-child.jsonl",
        &format!("{day}/rollout-2026-04-23T00-00-01-child.jsonl"),
    );
    let report = sandbox.ingest(&mut ledger);
    assert_eq!(report.ingested_sessions, 1);
    assert_eq!(turn_ids(&ledger, "agent_inv_42").len(), 1);
    assert_eq!(turn_ids(&ledger, "sess_spawn_1").len(), 1);
}

#[test]
fn opencode_sessions_land() {
    let sandbox = Sandbox::new();
    sandbox.place(
        "opencode/multi-turn/storage",
        ".local/share/opencode/storage",
    );
    let mut ledger = sandbox.ledger();
    let report = sandbox.ingest(&mut ledger);
    assert_eq!(report.ingested_sessions, 2, "a child session is its own");
    assert!(!turn_ids(&ledger, "ses_multi").is_empty());
    assert!(!turn_ids(&ledger, "ses_child").is_empty());
}

#[test]
fn a_ledger_without_a_watermark_reconciles_without_duplicates() {
    let sandbox = Sandbox::new();
    let file = sandbox
        .claude_project()
        .join(format!("{SIMPLE_SESSION}.jsonl"));
    fs::write(
        &file,
        claude_transcript(SIMPLE_SESSION, "/tmp/project", &["a", "b"]),
    )
    .unwrap();
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    let turns = ledger.count_table("turns").unwrap();

    // An upgraded ledger holds turns but no relayhistory watermark.
    ledger.write_cursors("{}").unwrap();
    let report = sandbox.ingest(&mut ledger);
    assert_eq!(
        report.scanned_sessions, 1,
        "every stored session is re-read"
    );
    assert_eq!(report.appended_turns, 0);
    assert_eq!(ledger.count_table("turns").unwrap(), turns);
}

#[test]
fn a_replaced_store_resyncs_without_duplicates() {
    let sandbox = Sandbox::new();
    let file = sandbox
        .claude_project()
        .join(format!("{SIMPLE_SESSION}.jsonl"));
    fs::write(
        &file,
        claude_transcript(SIMPLE_SESSION, "/tmp/project", &["a"]),
    )
    .unwrap();
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);

    // Another database, whose watermarks the ledger's is not one of.
    let opts = IngestOptions {
        store: sandbox.store(sandbox.dir.path().join("replacement.db")),
        ..sandbox.options()
    };
    let report = ingest_all(&mut ledger, &opts).unwrap();
    assert_eq!(report.scanned_sessions, 1);
    assert_eq!(report.appended_turns, 0);
    assert_eq!(turn_ids(&ledger, SIMPLE_SESSION).len(), 1);
}

#[test]
fn a_pending_stamp_tags_the_session_it_launched() {
    let sandbox = Sandbox::new();
    let cwd = sandbox.dir.path().join("project");
    fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_string_lossy().into_owned();
    let mut enrichment = Enrichment::new();
    enrichment.insert("persona".to_string(), "code-reviewer".to_string());
    write_pending_stamp(WriteOptions {
        harness: PendingStampHarness::Claude,
        ledger_home: Some(sandbox.ledger_home()),
        cwd: cwd.clone(),
        enrichment: enrichment.clone(),
        ..Default::default()
    })
    .unwrap();
    let file = sandbox
        .claude_project()
        .join(format!("{SIMPLE_SESSION}.jsonl"));
    fs::write(&file, claude_transcript(SIMPLE_SESSION, &cwd, &["hi"])).unwrap();

    let mut ledger = sandbox.ledger();
    let report = sandbox.ingest(&mut ledger);
    assert_eq!(report.applied_pending_stamps, 1);
    let tagged = ledger
        .query_turns(&Query {
            enrichment: Some(enrichment),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(tagged.len(), 1);
}

#[test]
fn content_is_kept_only_in_full_mode() {
    let sandbox = Sandbox::new();
    let file = sandbox
        .claude_project()
        .join(format!("{SIMPLE_SESSION}.jsonl"));
    fs::write(
        &file,
        claude_transcript(SIMPLE_SESSION, "/tmp/project", &["hi"]),
    )
    .unwrap();
    fs::create_dir_all(sandbox.ledger_home()).unwrap();
    fs::write(
        sandbox.ledger_home().join("config.json"),
        r#"{"content":{"store":"off"}}"#,
    )
    .unwrap();
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    assert_eq!(turn_ids(&ledger, SIMPLE_SESSION).len(), 1);
    assert_eq!(ledger.count_content().unwrap(), 0);
}

#[test]
fn the_hook_path_ingests_the_one_transcript() {
    let sandbox = Sandbox::new();
    let file = sandbox
        .claude_project()
        .join(format!("{SIMPLE_SESSION}.jsonl"));
    fs::write(
        &file,
        claude_transcript(SIMPLE_SESSION, "/tmp/project", &["hi"]),
    )
    .unwrap();
    let other = "22222222-2222-2222-2222-222222222222";
    let other_file = sandbox.claude_project().join(format!("{other}.jsonl"));
    fs::write(
        &other_file,
        claude_transcript(other, "/tmp/project", &["x"]),
    )
    .unwrap();

    let mut ledger = sandbox.ledger();
    let report = ingest_claude_transcript_path(&mut ledger, &file, &sandbox.options()).unwrap();
    assert_eq!(report.ingested_sessions, 1);
    assert_eq!(turn_ids(&ledger, SIMPLE_SESSION).len(), 1);
    assert!(turn_ids(&ledger, other).is_empty(), "no sweep ran");

    // A later full ingest picks up the rest and bills nothing twice.
    let report = sandbox.ingest(&mut ledger);
    assert_eq!(report.appended_turns, 1);
    assert_eq!(turn_ids(&ledger, other).len(), 1);
}

#[test]
fn the_hook_path_ignores_a_missing_transcript() {
    let sandbox = Sandbox::new();
    let mut ledger = sandbox.ledger();
    let missing = sandbox.claude_project().join("gone.jsonl");
    let report = ingest_claude_transcript_path(&mut ledger, &missing, &sandbox.options()).unwrap();
    assert_eq!(report, IngestReport::empty());
}

#[test]
fn the_watch_loop_ingests_on_its_first_tick_and_stops() {
    let sandbox = Sandbox::new();
    let file = sandbox
        .claude_project()
        .join(format!("{SIMPLE_SESSION}.jsonl"));
    fs::write(
        &file,
        claude_transcript(SIMPLE_SESSION, "/tmp/project", &["hi"]),
    )
    .unwrap();
    let mut ledger = sandbox.ledger();
    let watch = WatchIngestOptions {
        use_fs_events: false,
        ..Default::default()
    };
    let stop = watch.stop.clone();
    let mut reports = Vec::new();
    watch_ingest(&mut ledger, &sandbox.options(), watch, |report| {
        reports.push(report.unwrap());
        stop.stop();
    })
    .unwrap();
    assert_eq!(reports[0].appended_turns, 1);
    assert_eq!(turn_ids(&ledger, SIMPLE_SESSION).len(), 1);
}
