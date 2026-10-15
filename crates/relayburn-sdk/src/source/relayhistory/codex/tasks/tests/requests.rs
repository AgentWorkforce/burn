//! Request-level accounting inside Codex tasks: every snapshot that moves
//! the running total is one model request.

use serde_json::{json, Value};

use super::{
    aborted, billed_after, derive, derive_at, evidence, live, marker, open_task, task,
    turn_context, usage,
};
use crate::reader::types::StopReason;
use crate::reader::{InferenceKeySource, InferenceKind};
use crate::source::relayhistory::codex::events::stream;
use crate::source::relayhistory::codex::tasks::{Derived, Tasks};
use crate::source::Billing;

fn snapshot(line: u64, input: u64, cached: u64, output: u64, reasoning: u64) -> Value {
    marker(
        line,
        "usage_snapshot",
        None,
        json!({"total_token_usage": {
            "input_tokens": input, "cached_input_tokens": cached,
            "output_tokens": output, "reasoning_output_tokens": reasoning
        }}),
    )
}

/// `(request id, turn id, (input, cache read, output, reasoning))`.
type Request = (String, String, (u64, u64, u64, u64));

/// Each inference as a [`Request`].
fn requests(derived: &Derived) -> Vec<Request> {
    derived
        .inferences
        .iter()
        .map(|i| {
            let u = &i.usage;
            (
                i.request_id.clone(),
                i.turn_id.clone(),
                (u.input, u.cache_read, u.output, u.reasoning),
            )
        })
        .collect()
}

fn request_counts(derived: &Derived) -> Vec<Option<u64>> {
    derived.turns.iter().map(|t| t.request_count).collect()
}

/// Each turn's usage equals the sum of its inferences'.
fn assert_requests_sum_to_turns(derived: &Derived) {
    for turn in &derived.turns {
        let mut sum = crate::reader::types::Usage::default();
        for i in derived
            .inferences
            .iter()
            .filter(|i| i.turn_id == turn.message_id)
        {
            sum.input += i.usage.input;
            sum.output += i.usage.output;
            sum.reasoning += i.usage.reasoning;
            sum.cache_read += i.usage.cache_read;
        }
        assert_eq!(sum, turn.usage, "{}", turn.message_id);
    }
}

#[test]
fn each_snapshot_that_moves_the_total_is_one_request() {
    let [s1, c1] = task(10, "t1");
    let derived = derive(vec![
        s1,
        turn_context(11, "t1", "gpt-5.5", "high"),
        snapshot(12, 1000, 400, 120, 20),
        // A re-report of the same running total is not a request.
        snapshot(13, 1000, 400, 120, 20),
        snapshot(14, 1600, 500, 200, 30),
        c1,
    ]);
    assert_eq!(request_counts(&derived), vec![Some(2)]);
    assert_eq!(
        requests(&derived),
        vec![
            ("t1:request:000001".into(), "t1".into(), (600, 400, 120, 20)),
            ("t1:request:000002".into(), "t1".into(), (500, 100, 80, 10)),
        ]
    );
    assert_requests_sum_to_turns(&derived);
    let first = &derived.inferences[0];
    assert_eq!(first.session_id, "sess");
    assert_eq!(first.model, "gpt-5.5");
    assert_eq!(first.request_id_source, InferenceKeySource::RowSynthetic);
    assert_eq!(first.kind, InferenceKind::Message);
    assert_eq!(first.tool_uses, []);
}

#[test]
fn snapshots_between_tasks_belong_to_no_request() {
    let [s1, c1] = task(10, "t1");
    let [s2, c2] = task(30, "t2");
    let derived = derive(vec![
        s1,
        snapshot(11, 1000, 400, 120, 0),
        c1,
        snapshot(20, 1500, 400, 150, 0),
        s2,
        snapshot(31, 4000, 1400, 300, 0),
        c2,
    ]);
    assert_eq!(request_counts(&derived), vec![Some(1), Some(1)]);
    assert_eq!(
        requests(&derived),
        vec![
            ("t1:request:000001".into(), "t1".into(), (600, 400, 120, 0)),
            (
                "t2:request:000001".into(),
                "t2".into(),
                (1500, 1000, 150, 0)
            ),
        ]
    );
}

#[test]
fn a_task_whose_total_never_moves_made_no_request() {
    let [s1, c1] = task(10, "t1");
    let [s2, c2] = task(30, "t2");
    let derived = derive(vec![
        snapshot(1, 500, 0, 50, 0),
        s1,
        c1,
        s2,
        snapshot(31, 500, 0, 50, 0),
        c2,
    ]);
    assert_eq!(request_counts(&derived), vec![Some(0), Some(0)]);
    assert!(derived.inferences.is_empty());
}

/// A request spans from its first record after the previous snapshot to
/// its own snapshot; with no record before it, it is the snapshot's instant.
#[test]
fn a_request_spans_its_records_up_to_its_snapshot() {
    let [s1, c1] = task(10, "t1");
    let messages = vec![super::assistant_block(15, "text", "looking")];
    let markers = vec![
        s1,
        snapshot(12, 100, 0, 10, 0),
        snapshot(18, 200, 0, 20, 0),
        c1,
    ];
    let derived = derive_at(markers, messages, &Billing::settled());
    let spans: Vec<(i64, i64)> = derived
        .inferences
        .iter()
        .map(|i| (i.start_ms, i.end_ms))
        .collect();
    assert_eq!(spans, [(12_000, 12_000), (15_000, 18_000)]);
    assert_eq!(derived.inferences[1].start_ts, "1970-01-01T00:00:15.000Z");
    assert_eq!(derived.inferences[1].end_ts, "1970-01-01T00:00:18.000Z");
}

fn call(line: u64, id: &str) -> (Value, Value) {
    (
        json!({
            "message_id": format!("{line}:message"), "role": "assistant",
            "ts_ms": line * 1000, "turn_id": "t1",
            "blocks": [{"event_uid": format!("{line}:function_call"), "role": "assistant",
                        "kind": "tool_use", "tool_use_id": id, "ts_ms": line * 1000}]
        }),
        json!({"tool_use_id": id, "message_id": format!("{line}:message"),
               "name": "exec_command", "args": {"cmd": "ls"}}),
    )
}

/// A request issued the tool calls written since the previous snapshot; a
/// call after the task's last snapshot reported no request.
#[test]
fn a_request_claims_the_tool_calls_written_before_its_snapshot() {
    let [s1, c1] = task(10, "t1");
    let calls = [call(11, "a"), call(12, "b"), call(14, "c"), call(16, "d")];
    let mut ev = evidence(
        vec![
            s1,
            snapshot(13, 100, 0, 10, 5),
            snapshot(15, 200, 0, 20, 5),
            c1,
        ],
        calls.iter().map(|(m, _)| m.clone()).collect(),
    );
    ev.tool_calls =
        serde_json::from_value(Value::Array(calls.iter().map(|(_, c)| c.clone()).collect()))
            .unwrap();
    let derived = Tasks::new(&ev, &Billing::settled()).run(&stream(&ev));
    let claimed: Vec<(Vec<&str>, InferenceKind)> = derived
        .inferences
        .iter()
        .map(|i| {
            let ids = i.tool_uses.iter().map(|t| t.id.as_str()).collect();
            (ids, i.kind)
        })
        .collect();
    assert_eq!(
        claimed,
        [
            (vec!["a", "b"], InferenceKind::Mixed),
            (vec!["c"], InferenceKind::ToolUse),
        ]
    );
    assert_eq!(derived.inferences[0].tool_uses[0].name, "exec_command");
    assert_eq!(derived.turns[0].tool_calls.len(), 4);
}

/// A regressed total leaves the task's usage unknown: its requests still
/// count, and none of them is billed.
#[test]
fn requests_of_a_task_with_unknown_usage_bill_nothing() {
    let [s1, c1] = task(10, "t1");
    let derived = derive(vec![
        snapshot(1, 5000, 0, 500, 0),
        s1,
        snapshot(11, 5600, 0, 550, 0),
        snapshot(12, 4000, 0, 400, 0),
        c1,
    ]);
    assert_eq!(usage(&derived), vec![(0, 0, 0, false)]);
    assert_eq!(request_counts(&derived), vec![Some(2)]);
    assert!(derived
        .inferences
        .iter()
        .all(|i| i.usage == crate::reader::types::Usage::default()));
}

/// An aborted task bills the requests it made before the abort.
#[test]
fn an_aborted_task_bills_each_request_it_made() {
    let derived = derive_at(
        vec![
            marker(10, "task_started", Some("t1"), Value::Null),
            snapshot(11, 1000, 400, 120, 0),
            snapshot(12, 1300, 400, 150, 0),
            aborted(13, "t1"),
        ],
        Vec::new(),
        &live(13),
    );
    assert_eq!(derived.turns[0].stop_reason, Some(StopReason::Aborted));
    assert_eq!(request_counts(&derived), vec![Some(2)]);
    assert_eq!(
        requests(&derived),
        vec![
            ("t1:request:000001".into(), "t1".into(), (600, 400, 120, 0)),
            ("t1:request:000002".into(), "t1".into(), (300, 0, 30, 0)),
        ]
    );
}

/// An open trailing task emits no request until it settles, then bills its
/// requests under the segment it is billed as.
#[test]
fn an_unfinished_task_bills_its_requests_once_settled() {
    let pending = derive_at(open_task(), Vec::new(), &live(12));
    assert!(pending.pending);
    assert!(pending.inferences.is_empty());

    let settled = derive_at(open_task(), Vec::new(), &Billing::settled());
    assert_eq!(request_counts(&settled), vec![Some(2)]);
    assert_eq!(
        requests(&settled),
        vec![
            (
                "t1@12:request:000001".into(),
                "t1@12".into(),
                (600, 400, 120, 0)
            ),
            (
                "t1@12:request:000002".into(),
                "t1@12".into(),
                (600, 0, 80, 0)
            ),
        ]
    );
}

/// A task billed open that continues bills its later requests under its
/// own id, numbered from one, and keeps the billed segment's requests as
/// they were; together they are the requests of the whole task.
#[test]
fn a_continued_task_bills_only_its_later_requests() {
    let first = derive_at(open_task(), Vec::new(), &Billing::settled());
    let ledger = billed_after(&first, i64::MAX);
    let mut grown = open_task();
    grown.extend([
        snapshot(40, 3000, 1400, 260, 0),
        marker(41, "task_complete", Some("t1"), Value::Null),
    ]);
    let second = derive_at(grown.clone(), Vec::new(), &ledger);
    assert_eq!(request_counts(&second), vec![Some(2), Some(1)]);
    assert_eq!(&requests(&second)[..2], &requests(&first)[..]);
    assert_eq!(
        requests(&second)[2],
        ("t1:request:000001".into(), "t1".into(), (400, 1000, 60, 0))
    );
    assert_requests_sum_to_turns(&second);

    let whole = derive_at(grown, Vec::new(), &Billing::settled());
    assert_eq!(request_counts(&whole), vec![Some(3)]);
    assert_requests_sum_to_turns(&whole);
}

/// A fork child's inherited running total is its baseline, not a request.
#[test]
fn a_fork_childs_requests_spend_from_its_inherited_total() {
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
    let derived = derive(vec![
        boundary,
        s1,
        snapshot(12, 5600, 1300, 450, 0),
        snapshot(13, 5900, 1300, 470, 0),
        c1,
    ]);
    assert_eq!(request_counts(&derived), vec![Some(2)]);
    assert_eq!(
        requests(&derived),
        vec![
            ("t1:request:000001".into(), "t1".into(), (300, 300, 50, 0)),
            ("t1:request:000002".into(), "t1".into(), (300, 0, 20, 0)),
        ]
    );
}

/// A snapshot with an unreadable counter still ends a request; the task's
/// usage is unknown, so no request is billed, and what each request
/// produced still reads from its own spend.
#[test]
fn an_unreadable_snapshot_still_ends_a_request() {
    let [s1, c1] = task(10, "t1");
    let bad = marker(
        12,
        "usage_snapshot",
        None,
        json!({"total_token_usage": {"input_tokens": -1}}),
    );
    let derived = derive(vec![
        s1,
        snapshot(11, 100, 0, 30, 30),
        bad,
        snapshot(13, 300, 0, 50, 30),
        c1,
    ]);
    assert_eq!(usage(&derived), vec![(0, 0, 0, false)]);
    assert_eq!(request_counts(&derived), vec![Some(3)]);
    let kinds: Vec<InferenceKind> = derived.inferences.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        [
            InferenceKind::Reasoning,
            InferenceKind::Message,
            InferenceKind::Message
        ]
    );
    assert!(derived
        .inferences
        .iter()
        .all(|i| i.usage == crate::reader::types::Usage::default()));
}
