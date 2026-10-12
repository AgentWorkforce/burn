//! Ingest through the relayhistory store: each test lays fixture sessions
//! out under a throwaway provider home, ingests into a throwaway ledger,
//! and checks what landed.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::ingest::ingest_all_at;
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
fn the_hook_path_ignores_a_directory_and_reports_an_unreadable_path() {
    let sandbox = Sandbox::new();
    let mut ledger = sandbox.ledger();
    let project = sandbox.claude_project();
    let report = ingest_claude_transcript_path(&mut ledger, &project, &sandbox.options()).unwrap();
    assert_eq!(report, IngestReport::empty());
    assert!(
        !sandbox.dir.path().join("ai-history.db").exists(),
        "a directory is not a transcript: the store is not even opened"
    );

    let file = project.join("not-a-dir.txt");
    fs::write(&file, "x").unwrap();
    let below_a_file = file.join("transcript.jsonl");
    assert!(ingest_claude_transcript_path(&mut ledger, &below_a_file, &sandbox.options()).is_err());
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

#[test]
fn summary_counts_subagents_from_the_store_the_ledger_was_ingested_from() {
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
    sandbox.ingest(&mut ledger);
    drop(ledger);

    let handle =
        crate::Ledger::open(crate::LedgerOpenOptions::with_home(sandbox.ledger_home())).unwrap();
    let crate::SummaryReport::Grouped(report) = handle
        .summary_report(crate::SummaryReportOptions::default())
        .unwrap()
    else {
        panic!("expected a grouped summary");
    };
    assert_eq!(report.subagents.paired + report.subagents.orphan, 2);
}

const ROLLOUT_SESSION: &str = "sess_tail_1";

fn rollout_line(ts: &str, kind: &str, payload: serde_json::Value) -> String {
    format!(
        "{}\n",
        serde_json::json!({"timestamp": ts, "type": kind, "payload": payload})
    )
}

fn token_count(ts: &str, input: u64, cached: u64, output: u64) -> String {
    rollout_line(
        ts,
        "event_msg",
        serde_json::json!({"type": "token_count", "info": {"total_token_usage": {
            "input_tokens": input, "cached_input_tokens": cached, "output_tokens": output
        }}}),
    )
}

/// `(message id, input + cache read + output)` per billed turn.
fn billed(ledger: &Ledger, session_id: &str) -> Vec<(String, u64)> {
    ledger
        .query_turns(&Query::for_session(session_id))
        .unwrap()
        .into_iter()
        .map(|t| {
            let u = &t.turn.usage;
            (t.turn.message_id, u.input + u.cache_read + u.output)
        })
        .collect()
}

#[test]
fn an_open_codex_task_is_billed_once_settled_and_its_continuation_after() {
    let sandbox = Sandbox::new();
    let dir = sandbox.home().join(".codex/sessions/2026/04/20");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!(
        "rollout-2026-04-20T00-00-00-{ROLLOUT_SESSION}.jsonl"
    ));
    let open = [
        rollout_line(
            "2026-04-20T00:00:00.000Z",
            "session_meta",
            serde_json::json!({"id": ROLLOUT_SESSION, "cwd": "/tmp/project",
                               "timestamp": "2026-04-20T00:00:00.000Z", "cli_version": "0.121.0"}),
        ),
        rollout_line(
            "2026-04-20T00:00:00.100Z",
            "turn_context",
            serde_json::json!({"turn_id": "turn_1", "cwd": "/tmp/project", "model": "gpt-5.4"}),
        ),
        rollout_line(
            "2026-04-20T00:00:00.200Z",
            "event_msg",
            serde_json::json!({"type": "task_started", "turn_id": "turn_1"}),
        ),
        token_count("2026-04-20T00:00:05.000Z", 1000, 400, 120),
    ]
    .concat();
    fs::write(&file, &open).unwrap();
    let last_write = crate::util::time::parse_iso_ms("2026-04-20T00:00:05.000Z").unwrap();
    let grace = crate::source::TAIL_GRACE_MS;
    let mut ledger = sandbox.ledger();

    let fresh = ingest_all_at(&mut ledger, &sandbox.options(), last_write + 1_000).unwrap();
    assert_eq!(
        fresh.appended_turns, 0,
        "the open task is still being written"
    );
    assert!(billed(&ledger, ROLLOUT_SESSION).is_empty());

    // Nothing new in the store, yet the pending session is read again.
    let settled = ingest_all_at(&mut ledger, &sandbox.options(), last_write + grace).unwrap();
    assert_eq!(settled.appended_turns, 1);
    let first = billed(&ledger, ROLLOUT_SESSION);
    assert_eq!(first.len(), 1);
    assert!(first[0].0.starts_with("turn_1@"), "{first:?}");
    assert_eq!(first[0].1, 1120);
    assert!(super::watermark::pending(&ledger).is_empty());

    // The session resumes the task after all.
    let grown = [
        open,
        token_count("2026-04-20T00:10:00.000Z", 3000, 1400, 260),
        rollout_line(
            "2026-04-20T00:10:00.100Z",
            "event_msg",
            serde_json::json!({"type": "task_complete", "turn_id": "turn_1"}),
        ),
    ]
    .concat();
    fs::write(&file, grown).unwrap();
    let later = last_write + 3_600_000;
    let resumed = ingest_all_at(&mut ledger, &sandbox.options(), later).unwrap();
    assert_eq!(resumed.appended_turns, 1);
    let turns = billed(&ledger, ROLLOUT_SESSION);
    assert_eq!(turns[0], first[0], "the billed segment is untouched");
    assert_eq!(turns[1], ("turn_1".to_string(), 2140));
    let total: u64 = turns.iter().map(|(_, tokens)| tokens).sum();
    assert_eq!(total, 3000 + 260, "the task is billed exactly once");

    let again = ingest_all_at(&mut ledger, &sandbox.options(), later).unwrap();
    assert_eq!(again.appended_turns, 0);
}

#[test]
fn a_subagent_that_grows_alone_is_billed_with_the_session_that_spawned_it() {
    let sandbox = Sandbox::new();
    let project = ".claude/projects/-tmp-project";
    sandbox.place(
        "claude-sidecars/sidecar-session.jsonl",
        &format!("{project}/sidecar-session.jsonl"),
    );
    let sidecars = sandbox.place(
        "claude-sidecars/sidecar-session",
        &format!("{project}/sidecar-session"),
    );
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    let before = turn_ids(&ledger, "sidecar-session").len();

    // Only the subagent's transcript changes.
    let sidecar = sidecars.join("subagents/agent-a1.jsonl");
    let mut text = fs::read_to_string(&sidecar).unwrap();
    text.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "parentUuid": "s1-a2", "isSidechain": true, "type": "assistant", "uuid": "s1-a3",
            "timestamp": "2026-07-01T00:00:09.000Z", "cwd": "/tmp/project",
            "sessionId": "sidecar-session", "version": "2.1.120", "agentId": "a1",
            "requestId": "req_msg_sub1_3",
            "message": {
                "model": "claude-haiku-4-5", "id": "msg_sub1_3", "type": "message",
                "role": "assistant", "content": [{"type": "text", "text": "one more"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 2,
                          "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
            }
        })
    ));
    fs::write(&sidecar, text).unwrap();
    let report = sandbox.ingest(&mut ledger);
    assert_eq!(report.appended_turns, 1, "{report:?}");
    let turns = turn_ids(&ledger, "sidecar-session");
    assert_eq!(turns.len(), before + 1);
    assert!(turns.contains(&"msg_sub1_3".to_string()), "{turns:?}");
}

#[test]
fn a_session_that_fails_to_land_is_read_again_next_time() {
    let sandbox = Sandbox::new();
    let project = sandbox.claude_project();
    fs::write(
        project.join(format!("{SIMPLE_SESSION}.jsonl")),
        claude_transcript(SIMPLE_SESSION, "/tmp/project", &["hi"]),
    )
    .unwrap();
    let mut ledger = sandbox.ledger();
    let burn_db = LedgerLayout::under(sandbox.ledger_home()).burn;
    let raw = rusqlite::Connection::open(&burn_db).unwrap();
    raw.execute_batch(&format!(
        "CREATE TRIGGER refuse BEFORE INSERT ON turns WHEN NEW.session_id = '{SIMPLE_SESSION}'
         BEGIN SELECT RAISE(ABORT, 'refused'); END;"
    ))
    .unwrap();
    let failed = sandbox.ingest(&mut ledger);
    assert_eq!(failed.appended_turns, 0);

    raw.execute_batch("DROP TRIGGER refuse;").unwrap();
    let retried = sandbox.ingest(&mut ledger);
    assert_eq!(
        retried.appended_turns, 1,
        "the failed session is read again"
    );
    assert_eq!(turn_ids(&ledger, SIMPLE_SESSION).len(), 1);
}

fn stamp(sandbox: &Sandbox, harness: PendingStampHarness, cwd: &str, spawn: std::time::SystemTime) {
    let mut enrichment = Enrichment::new();
    enrichment.insert("persona".to_string(), "code-reviewer".to_string());
    write_pending_stamp(WriteOptions {
        harness,
        ledger_home: Some(sandbox.ledger_home()),
        cwd: cwd.to_string(),
        enrichment,
        spawn_start_ts: Some(spawn),
        ..Default::default()
    })
    .unwrap();
}

#[test]
fn a_pending_codex_stamp_tags_the_codex_session_it_launched() {
    let sandbox = Sandbox::new();
    let cwd = sandbox.dir.path().join("project");
    fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_string_lossy().into_owned();
    stamp(
        &sandbox,
        PendingStampHarness::Codex,
        &cwd,
        std::time::SystemTime::now(),
    );
    let dir = sandbox.home().join(".codex/sessions/2026/04/20");
    fs::create_dir_all(&dir).unwrap();
    let rollout = [
        rollout_line(
            "2026-04-20T00:00:00.000Z",
            "session_meta",
            serde_json::json!({"id": "sess_stamped", "cwd": cwd,
                               "timestamp": "2026-04-20T00:00:00.000Z", "cli_version": "0.121.0"}),
        ),
        rollout_line(
            "2026-04-20T00:00:00.200Z",
            "event_msg",
            serde_json::json!({"type": "task_started", "turn_id": "turn_1"}),
        ),
        token_count("2026-04-20T00:00:05.000Z", 1000, 400, 120),
        rollout_line(
            "2026-04-20T00:00:05.100Z",
            "event_msg",
            serde_json::json!({"type": "task_complete", "turn_id": "turn_1"}),
        ),
    ]
    .concat();
    fs::write(
        dir.join("rollout-2026-04-20T00-00-00-sess_stamped.jsonl"),
        rollout,
    )
    .unwrap();
    let mut ledger = sandbox.ledger();
    let report = sandbox.ingest(&mut ledger);
    assert_eq!(report.applied_pending_stamps, 1, "{report:?}");
}

#[test]
fn a_stamp_written_after_the_session_was_last_written_is_not_its_launcher() {
    let sandbox = Sandbox::new();
    let cwd = sandbox.dir.path().join("project");
    fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_string_lossy().into_owned();
    let file = sandbox
        .claude_project()
        .join(format!("{SIMPLE_SESSION}.jsonl"));
    fs::write(&file, claude_transcript(SIMPLE_SESSION, &cwd, &["hi"])).unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    stamp(&sandbox, PendingStampHarness::Claude, &cwd, later);
    let mut ledger = sandbox.ledger();
    let report = sandbox.ingest(&mut ledger);
    assert_eq!(report.appended_turns, 1);
    assert_eq!(report.applied_pending_stamps, 0);
}

#[test]
fn ingest_reports_each_phase_to_its_progress_sink() {
    let sandbox = Sandbox::new();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    let opts = IngestOptions {
        on_progress: Some(Box::new(move |phase: &str| {
            sink.lock().unwrap().push(phase.to_string())
        })),
        ..sandbox.options()
    };
    let mut ledger = sandbox.ledger();
    ingest_all(&mut ledger, &opts).unwrap();
    let seen = seen.lock().unwrap();
    for phase in ["syncing session history", "importing sessions"] {
        assert!(seen.iter().any(|p| p == phase), "{phase}: {seen:?}");
    }
}
