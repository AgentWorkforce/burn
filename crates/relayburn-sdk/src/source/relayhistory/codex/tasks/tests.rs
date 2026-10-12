//! Task accounting over `usage_snapshot` markers.

use ai_hist::SessionEvidence;
use serde_json::{json, Value};

use super::super::events::stream;
use super::{Derived, Tasks};
use crate::reader::types::StopReason;
use crate::source::{Billing, TAIL_GRACE_MS};

fn evidence(markers: Vec<Value>, messages: Vec<Value>) -> SessionEvidence {
    serde_json::from_value(json!({
        "session": {
            "source": "codex", "session_id": "sess", "cwd": "/tmp/project",
            "git_branch": null, "first_activity_ms": 0, "last_activity_ms": 0,
            "first_prompt": null, "last_assistant_text": null, "models": [],
            "originator": null, "agent_version": null, "repo_url": null,
            "initial_commit": null, "workspace_roots": [], "project_key": null,
            "project_key_method": null, "raw_path": null, "source_stamp": null,
            "discovery_state": "full", "locations": ["local"]
        },
        "prompts": [], "messages": messages, "tool_calls": [], "tool_results": [],
        "file_edits": [], "markers": markers, "relationships": [], "requests": [],
        "usage": null, "user_turns": [], "coverage": [], "loaded": [],
        "include_text": true, "diagnostics": []
    }))
    .unwrap()
}

fn marker(line: u64, kind: &str, turn: Option<&str>, payload: Value) -> Value {
    json!({
        "marker_uid": format!("{line}:marker"), "ts_ms": line * 1000,
        "message_id": null, "parent_id": null, "turn_id": turn,
        "kind": kind, "subkind": kind, "text": null,
        "payload": if kind == "usage_snapshot" { Value::Null } else { payload.clone() },
        "usage_snapshot": if kind == "usage_snapshot" { payload } else { Value::Null }
    })
}

fn task(line: u64, turn: &str) -> [Value; 2] {
    [
        marker(line, "task_started", Some(turn), Value::Null),
        marker(line + 9, "task_complete", Some(turn), Value::Null),
    ]
}

fn snapshot(line: u64, input: u64, cached: u64, output: u64) -> Value {
    marker(
        line,
        "usage_snapshot",
        None,
        json!({"total_token_usage": {
            "input_tokens": input, "cached_input_tokens": cached, "output_tokens": output
        }}),
    )
}

fn derive(markers: Vec<Value>) -> Derived {
    derive_with(markers, Vec::new())
}

fn derive_with(markers: Vec<Value>, messages: Vec<Value>) -> Derived {
    derive_at(markers, messages, &Billing::settled())
}

fn derive_at(markers: Vec<Value>, messages: Vec<Value>, billing: &Billing) -> Derived {
    let ev = evidence(markers, messages);
    Tasks::new(&ev, billing).run(&stream(&ev))
}

/// `(input, cache_read, output, usage known)` per turn.
fn usage(derived: &Derived) -> Vec<(u64, u64, u64, bool)> {
    derived
        .turns
        .iter()
        .map(|t| {
            let known = t.fidelity.as_ref().unwrap().coverage.has_input_tokens;
            (t.usage.input, t.usage.cache_read, t.usage.output, known)
        })
        .collect()
}

#[test]
fn tasks_spend_the_difference_of_running_totals() {
    let [s1, c1] = task(10, "t1");
    let [s2, c2] = task(30, "t2");
    let derived = derive(vec![
        s1,
        // `info: null`, as Codex writes before any spend.
        marker(11, "usage_snapshot", None, Value::Null),
        snapshot(12, 1000, 400, 120),
        c1,
        // Between tasks: counted toward the next task's baseline.
        snapshot(20, 1500, 400, 150),
        s2,
        snapshot(31, 4000, 1400, 300),
        c2,
    ]);
    assert_eq!(
        usage(&derived),
        vec![(600, 400, 120, true), (1500, 1000, 150, true)]
    );
    assert_eq!(derived.turns[1].turn_index, 1);
}

#[test]
fn task_without_snapshot_has_unknown_usage() {
    let [s1, c1] = task(10, "t1");
    assert_eq!(usage(&derive(vec![s1, c1])), vec![(0, 0, 0, false)]);
}

#[test]
fn regressed_total_leaves_usage_unknown_and_rebases() {
    let [s1, c1] = task(10, "t1");
    let [s2, c2] = task(30, "t2");
    let derived = derive(vec![
        snapshot(1, 5000, 0, 500),
        s1,
        snapshot(11, 4000, 0, 400),
        c1,
        s2,
        snapshot(31, 4500, 0, 450),
        c2,
    ]);
    assert_eq!(usage(&derived), vec![(0, 0, 0, false), (500, 0, 50, true)]);
}

#[test]
fn malformed_snapshot_leaves_usage_unknown() {
    let [s1, c1] = task(10, "t1");
    let bad = marker(
        11,
        "usage_snapshot",
        None,
        json!({"total_token_usage": {"input_tokens": -1}}),
    );
    assert_eq!(usage(&derive(vec![s1, bad, c1])), vec![(0, 0, 0, false)]);
}

/// Read while the rollout's last line (written at `line` seconds) is fresh.
fn live(line: u64) -> Billing {
    Billing::at(line as i64 * 1000)
}

#[test]
fn nothing_is_emitted_before_a_task_commits() {
    let [s1, c1] = task(10, "t1");
    let [s2, _] = task(30, "t2");
    let derived = derive_at(vec![s1, c1, s2], Vec::new(), &live(30));
    assert!(derived.committed);
    assert!(derived.pending);
    assert_eq!(derived.turns.len(), 1);
    let [open, _] = task(10, "t1");
    let derived = derive_at(vec![open], Vec::new(), &live(10));
    assert!(!derived.committed);
    assert!(derived.pending);
    assert!(derived.turns.is_empty());
}

fn turn_context(line: u64, turn: &str, model: &str, effort: &str) -> Value {
    marker(
        line,
        "turn_context",
        Some(turn),
        json!({"turn_id": turn, "model": model, "cwd": "/tmp/project",
               "effort": effort, "summary": "detailed"}),
    )
}

/// `(model, effort)` per turn.
fn configuration(derived: &Derived) -> Vec<(String, Option<String>)> {
    derived
        .turns
        .iter()
        .map(|t| {
            let effort = t.reasoning.as_ref().and_then(|r| r.effort.clone());
            (t.model.clone(), effort)
        })
        .collect()
}

#[test]
fn turn_context_carries_forward_until_it_changes() {
    let [s1, c1] = task(10, "t1");
    let [s2, c2] = task(30, "t2");
    let [s3, c3] = task(50, "t3");
    let derived = derive(vec![
        s1,
        turn_context(11, "t1", "gpt-5.4", "low"),
        c1,
        // t2 repeats t1's configuration, so relayhistory stored no record.
        s2,
        c2,
        s3,
        turn_context(51, "t3", "gpt-5.5", "high"),
        c3,
    ]);
    assert_eq!(
        configuration(&derived),
        vec![
            ("gpt-5.4".to_string(), Some("low".to_string())),
            ("gpt-5.4".to_string(), Some("low".to_string())),
            ("gpt-5.5".to_string(), Some("high".to_string())),
        ]
    );
    let reasoning = derived.turns[0].reasoning.as_ref().unwrap();
    assert_eq!(reasoning.summary.as_deref(), Some("detailed"));
}

#[test]
fn turn_context_before_a_task_applies_to_it() {
    let [s1, c1] = task(10, "t1");
    let derived = derive(vec![turn_context(5, "t1", "gpt-5.4", "medium"), s1, c1]);
    assert_eq!(
        configuration(&derived),
        vec![("gpt-5.4".to_string(), Some("medium".to_string()))]
    );
}

#[test]
fn turns_without_turn_context_record_no_reasoning() {
    let [s1, c1] = task(10, "t1");
    let derived = derive(vec![s1, c1]);
    assert_eq!(derived.turns[0].reasoning, None);
    let json = serde_json::to_string(&derived.turns[0]).unwrap();
    assert!(!json.contains("\"reasoning\":{"), "{json}");
}

fn id_less_context(line: u64, model: &str, effort: &str) -> Value {
    marker(
        line,
        "turn_context",
        None,
        json!({"model": model, "effort": effort}),
    )
}

fn pair(model: &str, effort: &str) -> (String, Option<String>) {
    (model.to_string(), Some(effort.to_string()))
}

#[test]
fn markers_apply_in_rollout_line_order_not_evidence_order() {
    let [s1, c1] = task(10, "t1");
    let [s2, c2] = task(30, "t2");
    // Evidence lists the later context first; the line index decides.
    let derived = derive(vec![
        id_less_context(25, "gpt-5.5", "high"),
        c2,
        s2,
        id_less_context(5, "gpt-5.4", "low"),
        c1,
        s1,
    ]);
    assert_eq!(
        configuration(&derived),
        vec![pair("gpt-5.4", "low"), pair("gpt-5.5", "high")]
    );
}

#[test]
fn the_context_naming_the_turn_wins_over_a_later_one_without_a_turn_id() {
    let [s1, c1] = task(10, "t1");
    let [s2, c2] = task(30, "t2");
    let derived = derive(vec![
        s1,
        turn_context(11, "t1", "gpt-5.4", "low"),
        // Inside t1 but naming no turn: not t1's, yet the latest for t2.
        id_less_context(15, "gpt-5.5", "high"),
        c1,
        s2,
        c2,
    ]);
    assert_eq!(
        configuration(&derived),
        vec![pair("gpt-5.4", "low"), pair("gpt-5.5", "high")]
    );
}

#[test]
fn a_context_naming_another_turn_is_not_this_turns_but_carries_forward() {
    let [s1, c1] = task(10, "t1");
    let [s2, c2] = task(30, "t2");
    let derived = derive(vec![
        turn_context(5, "t0", "gpt-5.3", "minimal"),
        s1,
        c1,
        s2,
        turn_context(31, "t2", "gpt-5.5", "high"),
        c2,
    ]);
    assert_eq!(
        configuration(&derived),
        vec![pair("gpt-5.3", "minimal"), pair("gpt-5.5", "high")]
    );
}

fn stamped(line: u64, turn: &str, model: &str, cwd: &str) -> Value {
    json!({
        "message_id": format!("{line}:message"), "role": "assistant", "ts_ms": line * 1000,
        "turn_id": turn, "model": model, "cwd": cwd, "blocks": []
    })
}

/// `(model, project)` per turn.
fn placement(derived: &Derived) -> Vec<(String, Option<String>)> {
    derived
        .turns
        .iter()
        .map(|t| (t.model.clone(), t.project.clone()))
        .collect()
}

#[test]
fn messages_place_a_turn_only_when_no_context_precedes_it() {
    let [s1, c1] = task(10, "t1");
    let [s2, c2] = task(30, "t2");
    let derived = derive_with(
        vec![
            s1,
            c1,
            s2,
            // Names no cwd: t2 is placed by the session, not its messages.
            marker(
                31,
                "turn_context",
                Some("t2"),
                json!({"turn_id": "t2", "model": "gpt-5.5"}),
            ),
            c2,
        ],
        vec![
            stamped(12, "t1", "gpt-5.4", "/tmp/stamped"),
            stamped(32, "t2", "gpt-5.4", "/tmp/stamped"),
        ],
    );
    assert_eq!(
        placement(&derived),
        vec![
            ("gpt-5.4".to_string(), Some("/tmp/stamped".to_string())),
            ("gpt-5.5".to_string(), Some("/tmp/project".to_string())),
        ]
    );
    assert_eq!(derived.turns[0].reasoning, None);
}

#[test]
fn a_fork_child_starts_its_chain_and_its_spend_at_its_own_history() {
    let [s1, c1] = task(10, "t1");
    let boundary = marker(
        1,
        "fork_replay_boundary",
        None,
        json!({
            "inherited_baseline": "applied",
            "inherited_snapshot": {"total_token_usage": {
                "input_tokens": 5000, "cached_input_tokens": 1000, "output_tokens": 400
            }},
        }),
    );
    // The replay wrote no turn_context, so t1 reads only its own.
    let derived = derive(vec![
        boundary,
        s1,
        turn_context(11, "t1", "gpt-5.5", "high"),
        snapshot(12, 5600, 1300, 450),
        c1,
    ]);
    assert_eq!(configuration(&derived), vec![pair("gpt-5.5", "high")]);
    assert_eq!(usage(&derived), vec![(300, 300, 50, true)]);
}

/// relayhistory keeps `turn_aborted` as an unmodelled marker.
fn aborted(line: u64, turn: &str) -> Value {
    let mut marker = marker(line, "unknown", Some(turn), Value::Null);
    marker["subkind"] = json!("turn_aborted");
    marker
}

/// `(message id, stop reason)` per turn.
fn outcomes(derived: &Derived) -> Vec<(String, Option<StopReason>)> {
    derived
        .turns
        .iter()
        .map(|t| (t.message_id.clone(), t.stop_reason))
        .collect()
}

#[test]
fn an_aborted_task_commits_its_spend_through_its_last_snapshot() {
    let [s2, c2] = task(30, "t2");
    let derived = derive_at(
        vec![
            marker(10, "task_started", Some("t1"), Value::Null),
            snapshot(11, 1000, 400, 120),
            aborted(12, "t1"),
            s2,
            snapshot(31, 1500, 400, 150),
            c2,
        ],
        Vec::new(),
        &live(39),
    );
    assert_eq!(
        usage(&derived),
        vec![(600, 400, 120, true), (500, 0, 30, true)]
    );
    assert_eq!(
        outcomes(&derived),
        vec![
            ("t1".to_string(), Some(StopReason::Aborted)),
            ("t2".to_string(), None)
        ]
    );
    assert!(!derived.pending);
}

#[test]
fn a_trailing_aborted_task_is_billed_without_waiting() {
    let derived = derive_at(
        vec![
            marker(10, "task_started", Some("t1"), Value::Null),
            snapshot(11, 1000, 400, 120),
            aborted(12, "t1"),
        ],
        Vec::new(),
        &live(12),
    );
    assert_eq!(usage(&derived), vec![(600, 400, 120, true)]);
    assert_eq!(derived.turns[0].stop_reason, Some(StopReason::Aborted));
    assert!(!derived.pending);
}

#[test]
fn an_abort_naming_another_task_does_not_end_the_open_one() {
    let derived = derive_at(
        vec![
            marker(10, "task_started", Some("t1"), Value::Null),
            snapshot(11, 1000, 400, 120),
            aborted(12, "t0"),
        ],
        Vec::new(),
        &live(12),
    );
    assert!(derived.turns.is_empty());
    assert!(derived.pending);
}

#[test]
fn a_task_the_next_task_start_ends_commits_at_that_start() {
    let derived = derive_at(
        vec![
            marker(10, "task_started", Some("t1"), Value::Null),
            snapshot(11, 1000, 400, 120),
            marker(30, "task_started", Some("t2"), Value::Null),
        ],
        Vec::new(),
        &live(30),
    );
    assert_eq!(usage(&derived), vec![(600, 400, 120, true)]);
    assert_eq!(outcomes(&derived), vec![("t1".to_string(), None)]);
    assert!(derived.pending, "t2 is still open");
}

fn open_task() -> Vec<Value> {
    vec![
        marker(10, "task_started", Some("t1"), Value::Null),
        snapshot(11, 1000, 400, 120),
        snapshot(12, 1600, 400, 200),
    ]
}

#[test]
fn a_trailing_open_task_waits_for_the_rollout_to_settle() {
    let last_write = 12_000;
    let fresh = derive_at(
        open_task(),
        Vec::new(),
        &Billing::at(last_write + TAIL_GRACE_MS - 1),
    );
    assert!(fresh.turns.is_empty());
    assert!(fresh.pending);

    let settled = derive_at(
        open_task(),
        Vec::new(),
        &Billing::at(last_write + TAIL_GRACE_MS),
    );
    assert_eq!(usage(&settled), vec![(1200, 400, 200, true)]);
    assert_eq!(outcomes(&settled), vec![("t1@12".to_string(), None)]);
    assert!(!settled.pending);
}

/// The ledger after a pass billed `derived`'s turns.
fn billed_after(derived: &Derived, now_ms: i64) -> Billing {
    Billing {
        now_ms,
        billed: derived.turns.iter().map(|t| t.message_id.clone()).collect(),
    }
}

/// The turns a pass adds to a ledger holding `billing.billed`.
fn appended(derived: &Derived, billing: &Billing) -> Vec<(String, u64, u64, u64)> {
    derived
        .turns
        .iter()
        .filter(|t| !billing.billed.contains(&t.message_id))
        .map(|t| {
            (
                t.message_id.clone(),
                t.usage.input,
                t.usage.cache_read,
                t.usage.output,
            )
        })
        .collect()
}

#[test]
fn a_task_billed_open_that_continues_bills_only_the_rest() {
    let first = derive_at(open_task(), Vec::new(), &Billing::settled());
    let ledger = billed_after(&first, i64::MAX);
    let mut grown = open_task();
    grown.extend([
        snapshot(40, 3000, 1400, 260),
        marker(41, "task_complete", Some("t1"), Value::Null),
    ]);
    let second = derive_at(grown.clone(), Vec::new(), &ledger);
    assert_eq!(
        outcomes(&second),
        vec![("t1@12".to_string(), None), ("t1".to_string(), None)]
    );
    assert_eq!(
        usage(&second)[0],
        usage(&first)[0],
        "the billed part is unchanged"
    );
    assert_eq!(
        appended(&second, &ledger),
        vec![("t1".to_string(), 400, 1000, 60)]
    );
    // Billed whole by a ledger that never saw it open, the task spends
    // exactly what the two parts do.
    let whole = derive_at(grown.clone(), Vec::new(), &Billing::settled());
    let parts = usage(&second);
    let summed = (0..3).map(|i| {
        let pick = |u: &(u64, u64, u64, bool)| [u.0, u.1, u.2][i];
        pick(&parts[0]) + pick(&parts[1])
    });
    assert_eq!(summed.collect::<Vec<_>>(), vec![1600, 1400, 260]);
    assert_eq!(usage(&whole), vec![(1600, 1400, 260, true)]);
    // Read again, the ledger gains nothing.
    let ledger = billed_after(&second, i64::MAX);
    assert!(appended(&derive_at(grown, Vec::new(), &ledger), &ledger).is_empty());
}

#[test]
fn a_continued_task_that_settles_open_again_bills_a_second_segment() {
    let first = derive_at(open_task(), Vec::new(), &Billing::settled());
    let mut grown = open_task();
    grown.push(snapshot(40, 2000, 400, 300));
    let ledger = billed_after(&first, 40_000);
    let live_pass = derive_at(grown.clone(), Vec::new(), &ledger);
    assert!(live_pass.pending);
    assert!(appended(&live_pass, &ledger).is_empty());

    let ledger = billed_after(&first, 40_000 + TAIL_GRACE_MS);
    let settled = derive_at(grown, Vec::new(), &ledger);
    assert!(!settled.pending);
    assert_eq!(
        appended(&settled, &ledger),
        vec![("t1@40".to_string(), 400, 0, 100)]
    );
}

#[test]
fn a_resumed_session_bills_its_new_task_and_leaves_the_billed_one_alone() {
    let first = derive_at(open_task(), Vec::new(), &Billing::settled());
    let ledger = billed_after(&first, i64::MAX);
    let [s2, c2] = task(60, "t2");
    let mut resumed = open_task();
    resumed.extend([s2, snapshot(61, 2000, 400, 250), c2]);
    let second = derive_at(resumed, Vec::new(), &ledger);
    assert_eq!(
        outcomes(&second),
        vec![("t1@12".to_string(), None), ("t2".to_string(), None)]
    );
    assert_eq!(
        appended(&second, &ledger),
        vec![("t2".to_string(), 400, 0, 50)]
    );
}

#[test]
fn a_billed_segment_the_rollout_has_not_passed_is_emitted_as_billed() {
    // A ledger whose segment ends past what the store holds now: the task
    // is not billed a second time under its own id.
    let ledger = Billing {
        now_ms: i64::MAX,
        billed: ["t1@50".to_string()].into(),
    };
    let mut ended = open_task();
    ended.push(marker(13, "task_complete", Some("t1"), Value::Null));
    let derived = derive_at(ended, Vec::new(), &ledger);
    assert_eq!(outcomes(&derived), vec![("t1@50".to_string(), None)]);
    assert!(appended(&derived, &ledger).is_empty());
}
