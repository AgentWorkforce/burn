//! Task accounting over `usage_snapshot` markers.

use ai_hist::SessionEvidence;
use serde_json::{json, Value};

use super::super::events::stream;
use super::{Derived, Tasks};

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
    let ev = evidence(markers, messages);
    Tasks::new(&ev).run(&stream(&ev))
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

#[test]
fn nothing_is_emitted_before_a_task_commits() {
    let [s1, c1] = task(10, "t1");
    let [s2, _] = task(30, "t2");
    let derived = derive(vec![s1, c1, s2]);
    assert!(derived.committed);
    assert_eq!(derived.turns.len(), 1);
    let [open, _] = task(10, "t1");
    assert!(!derive(vec![open]).committed);
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
