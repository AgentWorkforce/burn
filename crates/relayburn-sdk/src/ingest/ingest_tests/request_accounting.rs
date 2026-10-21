//! A ledger holding Codex turns from before request accounting: ingest
//! re-accounts them in place, without re-billing anything.

use rusqlite::Connection;

use super::*;
use crate::reader::{build_inferences, TurnRecord};

const MANY: &str = "sess_many_requests";

fn place_sessions(sandbox: &Sandbox) {
    sandbox.place(
        "codex/many-requests-one-turn.jsonl",
        &format!(".codex/sessions/2026/07/29/rollout-2026-07-29T15-28-59-{MANY}.jsonl"),
    );
    let file = sandbox
        .claude_project()
        .join(format!("{SIMPLE_SESSION}.jsonl"));
    fs::write(
        &file,
        claude_transcript(SIMPLE_SESSION, "/tmp/project", &["hi"]),
    )
    .unwrap();
}

fn connection(sandbox: &Sandbox) -> Connection {
    Connection::open(LedgerLayout::under(sandbox.ledger_home()).burn).unwrap()
}

/// Rewind the ledger's Codex rows to their pre-accounting shape: no
/// request count, and one inference per task keyed by its message id.
fn rewind_to_task_accounting(sandbox: &Sandbox, ledger: &mut Ledger) {
    let conn = connection(sandbox);
    conn.execute_batch(
        "UPDATE turns SET request_count = NULL,
             record_json = json_remove(record_json, '$.requestCount')
         WHERE source = 'codex';
         DELETE FROM inferences WHERE source = 'codex';",
    )
    .unwrap();
    let turns: Vec<TurnRecord> = ledger
        .query_turns(&Query::for_session(MANY))
        .unwrap()
        .into_iter()
        .map(|t| t.turn)
        .collect();
    assert!(turns.iter().all(|t| t.request_count.is_none()));
    let per_task = build_inferences(&turns, &Default::default());
    ledger.append_inferences(&per_task).unwrap();
}

/// Every turn row as `(source, session, message id, request count,
/// record)`.
fn rows(sandbox: &Sandbox) -> Vec<(String, String, String, Option<i64>, String)> {
    let conn = connection(sandbox);
    let mut stmt = conn
        .prepare(
            "SELECT source, session_id, message_id, request_count, record_json
             FROM turns ORDER BY source, session_id, message_id",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn codex_tokens(ledger: &Ledger) -> u64 {
    ledger
        .query_turns(&Query::for_session(MANY))
        .unwrap()
        .iter()
        .map(|t| {
            let u = &t.turn.usage;
            u.input + u.output + u.reasoning + u.cache_read
        })
        .sum()
}

fn inference_ids(ledger: &Ledger) -> Vec<String> {
    ledger
        .query_inferences(&Query::for_session(MANY))
        .unwrap()
        .into_iter()
        .map(|i| i.request_id)
        .collect()
}

#[test]
fn codex_turns_from_before_request_accounting_are_re_accounted_in_place() {
    let sandbox = Sandbox::new();
    place_sessions(&sandbox);
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    let accounted = rows(&sandbox);
    let tokens = codex_tokens(&ledger);
    let requests = inference_ids(&ledger);
    assert_eq!(requests.len(), 4);

    rewind_to_task_accounting(&sandbox, &mut ledger);
    assert_eq!(inference_ids(&ledger), ["turn_many_requests"]);
    assert_eq!(
        ledger.sessions_awaiting_request_accounting().unwrap(),
        [MANY]
    );

    // Nothing changed upstream; the session is read for its accounting.
    let report = sandbox.ingest(&mut ledger);
    assert_eq!(report.appended_turns, 0, "nothing is billed again");
    assert_eq!(rows(&sandbox), accounted, "every turn row as if fresh");
    assert_eq!(codex_tokens(&ledger), tokens);
    assert_eq!(inference_ids(&ledger), requests);
    assert!(ledger
        .sessions_awaiting_request_accounting()
        .unwrap()
        .is_empty());

    assert_eq!(sandbox.ingest(&mut ledger), IngestReport::empty());
}

/// A session the store no longer holds keeps its rows as they are.
#[test]
fn a_session_missing_from_the_store_keeps_its_task_accounting() {
    let sandbox = Sandbox::new();
    place_sessions(&sandbox);
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    rewind_to_task_accounting(&sandbox, &mut ledger);
    let before = rows(&sandbox);
    let fresh_store = sandbox.dir.path().join("other-ai-history.db");
    let options = IngestOptions {
        ledger_home: Some(sandbox.ledger_home()),
        store: HistoryStoreOptions {
            db_path: Some(fresh_store),
            home: Some(sandbox.dir.path().join("empty-home")),
        },
        ..Default::default()
    };
    ingest_all(&mut ledger, &options).unwrap();
    assert_eq!(rows(&sandbox), before);
    assert_eq!(inference_ids(&ledger), ["turn_many_requests"]);
}
