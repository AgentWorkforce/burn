//! A Codex paginated fork copies its parent's turns into its own rollout.
//! A ledger that billed that copy under the fork is cleared and appended
//! again from the fork's own turns; every other session keeps what it
//! holds.

use rusqlite::Connection;

use super::*;

const PARENT: &str = "019da82f-d400-7000-8000-00000000000a";
const CHILD: &str = "019da830-be60-7000-8000-00000000000c";
const CHILD_TASK: &str = "019da830-d1e8-7000-8000-0000000000c1";
const DAY: &str = ".codex/sessions/2026/04/20";

fn rollout(sandbox: &Sandbox, id: &str) -> PathBuf {
    sandbox
        .home()
        .join(format!("{DAY}/rollout-2026-04-20T00-00-00-{id}.jsonl"))
}

fn place_fork(sandbox: &Sandbox) {
    for (fixture, id) in [("parent", PARENT), ("child", CHILD)] {
        let dest = rollout(sandbox, id);
        sandbox.place(
            &format!("codex-fork-paginated/{fixture}.jsonl"),
            dest.strip_prefix(sandbox.home()).unwrap().to_str().unwrap(),
        );
    }
}

/// Append one completed task to `id`'s rollout, its cumulative usage at
/// `input` / `output` tokens.
fn append_task(sandbox: &Sandbox, id: &str, turn_id: &str, input: u64, output: u64) {
    let ts = "2026-04-20T00:05:00.000Z";
    let lines = [
        serde_json::json!({"timestamp": ts, "type": "event_msg",
            "payload": {"type": "task_started", "turn_id": turn_id}}),
        serde_json::json!({"timestamp": ts, "type": "turn_context",
            "payload": {"turn_id": turn_id, "cwd": "/tmp/project", "model": "gpt-5.4"}}),
        serde_json::json!({"timestamp": ts, "type": "event_msg",
            "payload": {"type": "user_message", "message": "one more"}}),
        serde_json::json!({"timestamp": ts, "type": "event_msg",
            "payload": {"type": "agent_message", "message": "done"}}),
        serde_json::json!({"timestamp": ts, "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": {
                "input_tokens": input, "cached_input_tokens": 0, "output_tokens": output,
                "reasoning_output_tokens": 0, "total_tokens": input + output}}}}),
        serde_json::json!({"timestamp": ts, "type": "event_msg",
            "payload": {"type": "task_complete", "turn_id": turn_id}}),
    ];
    let mut text = fs::read_to_string(rollout(sandbox, id)).unwrap();
    for line in lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    fs::write(rollout(sandbox, id), text).unwrap();
}

/// Bill `from`'s turns, inferences and content under `to` as well, the
/// way a ledger written before relayhistory set a fork's copy aside holds
/// them.
fn bill_under(sandbox: &Sandbox, from: &str, to: &str) {
    let layout = LedgerLayout::under(sandbox.ledger_home());
    let burn = Connection::open(&layout.burn).unwrap();
    burn.execute(
        "INSERT INTO turns (source, session_id, message_id, ts, project, project_key,
             record_json, content_fingerprint, stop_reason, subagent_id, request_count)
         SELECT source, ?2, message_id, ts, project, project_key,
             json_set(record_json, '$.sessionId', ?2), content_fingerprint || ?2,
             stop_reason, subagent_id, request_count
         FROM turns WHERE session_id = ?1",
        [from, to],
    )
    .unwrap();
    burn.execute(
        "INSERT INTO inferences SELECT source, ?2, request_id, request_id_source, turn_id,
             model, kind, start_ts, end_ts, record_json
         FROM inferences WHERE session_id = ?1",
        [from, to],
    )
    .unwrap();
    let content = Connection::open(&layout.content).unwrap();
    content
        .execute(
            "INSERT INTO content (source, session_id, message_id, content_hash, body,
                 byte_length, created_at)
             SELECT source, ?2, message_id, content_hash, body, byte_length, created_at
             FROM content WHERE session_id = ?1",
            [from, to],
        )
        .unwrap();
}

/// File a `kind` edge from `session_id` to its parent, stamped `ts`.
fn file_edge(sandbox: &Sandbox, session_id: &str, kind: &str, ts: &str) {
    let burn = Connection::open(LedgerLayout::under(sandbox.ledger_home()).burn).unwrap();
    burn.execute(
        "INSERT INTO relationships (id_fingerprint, source, session_id, related_session_id,
             relationship_type, ts, record_json)
         VALUES (?1 || ?3 || ?4, 'codex', ?1, ?2, ?3, ?4, '{}')",
        [session_id, PARENT, kind, ts],
    )
    .unwrap();
}

/// Every edge filed under `session_id`, as `(type, ts)`.
fn edges(sandbox: &Sandbox, session_id: &str) -> Vec<(String, String)> {
    let burn = Connection::open(LedgerLayout::under(sandbox.ledger_home()).burn).unwrap();
    let mut stmt = burn
        .prepare(
            "SELECT relationship_type, ts FROM relationships WHERE session_id = ?
             ORDER BY relationship_type, ts",
        )
        .unwrap();
    stmt.query_map([session_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn sorted_turn_ids(ledger: &Ledger, session_id: &str) -> Vec<String> {
    let mut ids = turn_ids(ledger, session_id);
    ids.sort();
    ids
}

fn turn_tokens(ledger: &Ledger, session_id: &str) -> u64 {
    ledger
        .query_turns(&Query::for_session(session_id))
        .unwrap()
        .iter()
        .map(|t| t.turn.usage.input + t.turn.usage.output)
        .sum()
}

fn inference_turns(ledger: &Ledger, session_id: &str) -> Vec<String> {
    let mut ids: Vec<String> = ledger
        .query_inferences(&Query::for_session(session_id))
        .unwrap()
        .into_iter()
        .map(|i| i.turn_id)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn content_messages(ledger: &Ledger, session_id: &str) -> Vec<String> {
    let mut ids: Vec<String> = ledger
        .query_content(&Query::for_session(session_id))
        .unwrap()
        .into_iter()
        .map(|c| c.message_id)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

#[test]
fn a_fork_billed_for_its_copied_parent_history_is_billed_for_its_own_turns_only() {
    let sandbox = Sandbox::new();
    place_fork(&sandbox);
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    assert_eq!(turn_ids(&ledger, CHILD), [CHILD_TASK]);
    assert_eq!(turn_tokens(&ledger, CHILD), 600);
    let parent_turns = sorted_turn_ids(&ledger, PARENT);
    assert_eq!(parent_turns.len(), 2);

    let own_edges = edges(&sandbox, CHILD);
    bill_under(&sandbox, PARENT, CHILD);
    file_edge(&sandbox, CHILD, "root", "2026-04-20T00:00:00.000Z");
    file_edge(&sandbox, CHILD, "subagent", "2026-04-20T00:00:30.000Z");
    assert_eq!(turn_ids(&ledger, CHILD).len(), 3);
    assert_eq!(turn_tokens(&ledger, CHILD), 600 + 2_500);

    let next = "019da831-0000-7000-8000-0000000000c2";
    append_task(&sandbox, CHILD, next, 3_000, 900);
    let report = sandbox.ingest(&mut ledger);

    let mut own = vec![CHILD_TASK.to_string(), next.to_string()];
    own.sort();
    assert_eq!(report.appended_turns, 2, "the fork is appended again whole");
    assert_eq!(sorted_turn_ids(&ledger, CHILD), own);
    assert_eq!(turn_tokens(&ledger, CHILD), 600 + 800);
    assert_eq!(inference_turns(&ledger, CHILD), own);
    assert!(content_messages(&ledger, CHILD)
        .iter()
        .all(|id| own.contains(id)));
    assert!(!content_messages(&ledger, CHILD).is_empty());
    let mut kept_edges = own_edges.clone();
    kept_edges.push(("subagent".into(), "2026-04-20T00:00:30.000Z".into()));
    kept_edges.sort();
    assert_eq!(
        edges(&sandbox, CHILD),
        kept_edges,
        "the fork's own edges again, and the edge its spawner filed"
    );
    assert_eq!(sorted_turn_ids(&ledger, PARENT), parent_turns);
    assert_eq!(turn_tokens(&ledger, PARENT), 2_500);

    assert_eq!(sandbox.ingest(&mut ledger), IngestReport::empty());
}

#[test]
fn a_fork_billed_for_its_own_turns_only_is_appended_to() {
    let sandbox = Sandbox::new();
    place_fork(&sandbox);
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);

    append_task(
        &sandbox,
        CHILD,
        "019da831-0000-7000-8000-0000000000c2",
        3_000,
        900,
    );
    let report = sandbox.ingest(&mut ledger);

    assert_eq!(report.appended_turns, 1, "only the new task lands");
    assert_eq!(turn_ids(&ledger, CHILD).len(), 2);
    assert_eq!(turn_tokens(&ledger, CHILD), 600 + 800);
}

/// A turn the ledger holds beyond what a session's rollout still shows
/// (history Codex pruned, say) stays billed unless the session copied it.
#[test]
fn a_session_that_copied_nothing_keeps_every_turn_it_holds() {
    let sandbox = Sandbox::new();
    place_fork(&sandbox);
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    bill_under(&sandbox, CHILD, PARENT);
    let held = sorted_turn_ids(&ledger, PARENT);
    assert_eq!(held.len(), 3);

    append_task(
        &sandbox,
        PARENT,
        "019da832-0000-7000-8000-000000000003",
        5_000,
        1_000,
    );
    let report = sandbox.ingest(&mut ledger);

    assert_eq!(report.appended_turns, 1);
    let ids = sorted_turn_ids(&ledger, PARENT);
    assert_eq!(ids.len(), 4);
    assert!(held.iter().all(|id| ids.contains(id)));
}
