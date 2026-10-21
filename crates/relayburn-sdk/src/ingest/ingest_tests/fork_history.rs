//! A Codex paginated fork copies its parent's turns into its own rollout.
//! A ledger that billed that copy under the fork retires exactly the
//! copied turns (held by the parent under the same id, absent from the
//! fork's records) with every row derived from them; every turn not proven
//! a copy stays billed.

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

fn burn_db(sandbox: &Sandbox) -> Connection {
    Connection::open(LedgerLayout::under(sandbox.ledger_home()).burn).unwrap()
}

fn content_db(sandbox: &Sandbox) -> Connection {
    Connection::open(LedgerLayout::under(sandbox.ledger_home()).content).unwrap()
}

/// Bill `from`'s turns and every row derived from them under `to` as
/// well, the way a ledger written before relayhistory set a fork's copy
/// aside holds them.
fn bill_under(sandbox: &Sandbox, from: &str, to: &str) {
    let burn = burn_db(sandbox);
    for sql in [
        "INSERT INTO turns (source, session_id, message_id, ts, project, project_key,
             record_json, content_fingerprint, stop_reason, subagent_id, request_count)
         SELECT source, ?2, message_id, ts, project, project_key,
             json_set(record_json, '$.sessionId', ?2), content_fingerprint || ?2,
             stop_reason, subagent_id, request_count
         FROM turns WHERE session_id = ?1",
        "INSERT INTO inferences SELECT source, ?2, request_id, request_id_source, turn_id,
             model, kind, start_ts, end_ts, record_json
         FROM inferences WHERE session_id = ?1",
        "INSERT INTO user_turns SELECT id_fingerprint || ?2, source, ?2, user_uuid, ts,
             record_json
         FROM user_turns WHERE session_id = ?1",
        "INSERT INTO tool_result_events SELECT id_fingerprint || ?2, source, ?2, tool_use_id,
             event_index, ts, record_json, output_bytes, output_truncated
         FROM tool_result_events WHERE session_id = ?1",
    ] {
        burn.execute(sql, [from, to]).unwrap();
    }
    content_db(sandbox)
        .execute(
            "INSERT INTO content (source, session_id, message_id, content_hash, body,
                 byte_length, created_at)
             SELECT source, ?2, message_id, content_hash, body, byte_length, created_at
             FROM content WHERE session_id = ?1",
            [from, to],
        )
        .unwrap();
}

/// Record that `session_id`'s turn `message_id` made tool call `call`.
fn give_call(sandbox: &Sandbox, session_id: &str, message_id: &str, call: &str) {
    burn_db(sandbox)
        .execute(
            "UPDATE turns SET record_json = json_set(record_json, '$.toolCalls',
                 json_array(json_object('id', ?3, 'name', 'spawn_agent', 'argsHash', 'x')))
             WHERE session_id = ?1 AND message_id = ?2",
            [session_id, message_id, call],
        )
        .unwrap();
}

/// File a subagent edge: `child` spawned by `spawner` through tool call
/// `call`.
fn file_spawn(sandbox: &Sandbox, child: &str, spawner: &str, call: &str) {
    let record = serde_json::json!({
        "v": 1, "source": "codex", "sessionId": child, "relatedSessionId": spawner,
        "relationshipType": "subagent", "parentToolUseId": call, "agentId": child,
    });
    burn_db(sandbox)
        .execute(
            "INSERT INTO relationships (id_fingerprint, source, session_id, related_session_id,
                 relationship_type, ts, record_json)
             VALUES (?1 || ?3, 'codex', ?1, ?2, 'subagent', NULL, ?4)",
            [child, spawner, call, &record.to_string()],
        )
        .unwrap();
}

/// Every edge filed under `session_id` or naming it as the parent, as
/// `(session, related, type)`.
fn edges(sandbox: &Sandbox, session_id: &str) -> Vec<(String, Option<String>, String)> {
    let burn = burn_db(sandbox);
    let mut stmt = burn
        .prepare(
            "SELECT session_id, related_session_id, relationship_type FROM relationships
             WHERE session_id = ?1 OR related_session_id = ?1 ORDER BY 1, 2, 3",
        )
        .unwrap();
    stmt.query_map([session_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The distinct `expr` values of `table`'s rows filed under `session_id`.
fn column(sandbox: &Sandbox, table: &str, expr: &str, session_id: &str) -> Vec<String> {
    let conn = if table == "content" {
        content_db(sandbox)
    } else {
        burn_db(sandbox)
    };
    let mut stmt = conn
        .prepare(&format!(
            "SELECT DISTINCT {expr} FROM {table} WHERE session_id = ? ORDER BY 1"
        ))
        .unwrap();
    stmt.query_map([session_id], |r| r.get(0))
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

/// Forget that the ledger was checked for copied history, as a ledger an
/// earlier burn wrote holds it.
fn forget_copies_check(ledger: &mut Ledger) {
    let mut cursors: serde_json::Value =
        serde_json::from_str(&ledger.read_cursors().unwrap()).unwrap();
    assert_eq!(cursors["copiedHistoryChecked"], serde_json::json!(true));
    cursors
        .as_object_mut()
        .unwrap()
        .remove("copiedHistoryChecked");
    ledger.write_cursors(&cursors.to_string()).unwrap();
}

#[test]
fn a_dormant_fork_billed_for_its_copied_parent_history_keeps_only_its_own_turns() {
    let sandbox = Sandbox::new();
    place_fork(&sandbox);
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    assert_eq!(turn_ids(&ledger, CHILD), [CHILD_TASK]);
    assert_eq!(turn_tokens(&ledger, CHILD), 600);
    let parent_turns = sorted_turn_ids(&ledger, PARENT);
    assert_eq!(parent_turns.len(), 2);
    let own_user_turns = column(&sandbox, "user_turns", "user_uuid", CHILD);
    let own_tool_events = column(&sandbox, "tool_result_events", "tool_use_id", CHILD);

    // The ledger an earlier ai-hist left: the copy billed under the fork,
    // one copied task having spawned a subagent.
    bill_under(&sandbox, PARENT, CHILD);
    give_call(&sandbox, CHILD, &parent_turns[0], "call_copied");
    file_spawn(&sandbox, "agent-copied", CHILD, "call_copied");
    file_spawn(&sandbox, "agent-own", CHILD, "call_own");
    file_spawn(&sandbox, CHILD, PARENT, "call_parent");
    let before = edges(&sandbox, CHILD);
    assert_eq!(turn_ids(&ledger, CHILD).len(), 3);
    assert_eq!(turn_tokens(&ledger, CHILD), 600 + 2_500);
    assert!(column(&sandbox, "content", "message_id", CHILD)
        .iter()
        .any(|id| parent_turns.contains(id)));
    assert_ne!(
        column(&sandbox, "user_turns", "user_uuid", CHILD),
        own_user_turns
    );

    // The rollout is untouched, so the change feed never names the fork
    // again: the ledger's one-time check for copies brings it back.
    forget_copies_check(&mut ledger);
    let report = sandbox.ingest(&mut ledger);

    assert_eq!(report.appended_turns, 0);
    assert_eq!(turn_ids(&ledger, CHILD), [CHILD_TASK]);
    assert_eq!(turn_tokens(&ledger, CHILD), 600);
    assert_eq!(
        column(&sandbox, "inferences", "turn_id", CHILD),
        [CHILD_TASK]
    );
    assert_eq!(
        column(&sandbox, "content", "message_id", CHILD),
        [CHILD_TASK]
    );
    assert_eq!(
        column(&sandbox, "user_turns", "user_uuid", CHILD),
        own_user_turns
    );
    assert_eq!(
        column(&sandbox, "tool_result_events", "tool_use_id", CHILD),
        own_tool_events
    );
    let after: Vec<_> = before
        .into_iter()
        .filter(|(session, _, _)| session != "agent-copied")
        .collect();
    assert_eq!(
        edges(&sandbox, CHILD),
        after,
        "the copied spawn's edge goes; the fork's own and its spawner's stay"
    );
    assert_eq!(sorted_turn_ids(&ledger, PARENT), parent_turns);
    assert_eq!(turn_tokens(&ledger, PARENT), 2_500);

    assert_eq!(sandbox.ingest(&mut ledger), IngestReport::empty());
}

/// A task the fork's rollout no longer shows (Codex pruned it, say) is
/// not proven a copy: no session the fork descends from holds it.
#[test]
fn a_forks_own_task_missing_from_its_rollout_stays_billed() {
    let sandbox = Sandbox::new();
    place_fork(&sandbox);
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    let pruned = "019da830-ffff-7000-8000-0000000000c0";
    burn_db(&sandbox)
        .execute(
            "INSERT INTO turns SELECT source, session_id, ?2, ts, project, project_key,
                 json_set(record_json, '$.messageId', ?2), content_fingerprint || ?2,
                 stop_reason, subagent_id, request_count
             FROM turns WHERE session_id = ?1 AND message_id = ?3",
            [CHILD, pruned, CHILD_TASK],
        )
        .unwrap();
    bill_under(&sandbox, PARENT, CHILD);

    let next = "019da831-0000-7000-8000-0000000000c2";
    append_task(&sandbox, CHILD, next, 3_000, 900);
    let report = sandbox.ingest(&mut ledger);

    assert_eq!(report.appended_turns, 1, "only the new task lands");
    let mut want = vec![CHILD_TASK.to_string(), pruned.to_string(), next.to_string()];
    want.sort();
    assert_eq!(
        sorted_turn_ids(&ledger, CHILD),
        want,
        "the copy goes, the pruned task stays"
    );
    assert_eq!(turn_tokens(&ledger, CHILD), 600 + 600 + 800);
}

/// A parent holding its child's turns copied nothing: it descends from no
/// session that holds them.
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
