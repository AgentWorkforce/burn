//! Copilot CLI journals: tool calls are read from the message that made
//! them, and shutdown totals are not model calls.

use super::*;

const SID: &str = "7a1c3e5f-2b4d-4f6a-8c0e-1d3f5a7b9c2e";

/// A journal: `session.start`, then one prompt and one assistant message
/// per `(tool, call id, path)`, each followed by its tool execution.
fn journal(calls: &[(&str, &str, &str)]) -> String {
    let mut lines = Vec::new();
    let mut parent = serde_json::Value::Null;
    let mut push = |kind: &str, data: serde_json::Value, lines: &mut Vec<String>| {
        let n = lines.len();
        let id = format!("00000000-0000-4000-8000-{n:012}");
        lines.push(
            serde_json::json!({
                "type": kind, "data": data, "id": id, "parentId": parent,
                "timestamp": format!("2026-09-18T21:30:{:02}.000Z", n),
            })
            .to_string(),
        );
        parent = serde_json::json!(id);
    };
    push(
        "session.start",
        serde_json::json!({"sessionId": SID, "version": 1, "producer": "copilot-agent",
            "copilotVersion": "1.0.86", "context": {"cwd": "/tmp/project"}}),
        &mut lines,
    );
    for (i, (tool, call, path)) in calls.iter().enumerate() {
        push(
            "user.message",
            serde_json::json!({"content": format!("step {i}"), "turnId": i.to_string()}),
            &mut lines,
        );
        push(
            "assistant.message",
            serde_json::json!({"messageId": format!("msg-{i}"), "model": "gpt-5.4",
                "content": "on it", "turnId": i.to_string(), "outputTokens": 10,
                "toolRequests": [{"toolCallId": call, "name": tool,
                    "arguments": {"path": path}, "type": "function"}]}),
            &mut lines,
        );
        push(
            "tool.execution_start",
            serde_json::json!({"toolCallId": call, "toolName": tool,
                "arguments": {"path": path}, "turnId": i.to_string()}),
            &mut lines,
        );
        push(
            "tool.execution_complete",
            serde_json::json!({"toolCallId": call, "success": true,
                "turnId": i.to_string(), "result": {"content": "ok"}}),
            &mut lines,
        );
    }
    lines.join("\n") + "\n"
}

fn ingest_journal(sandbox: &Sandbox, text: &str) -> Vec<crate::reader::TurnRecord> {
    let dir = sandbox.home().join(format!(".copilot/session-state/{SID}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("events.jsonl"), text).unwrap();
    let mut ledger = sandbox.ledger();
    sandbox.ingest(&mut ledger);
    let mut turns: Vec<_> = ledger
        .query_turns(&Query::for_session(SID))
        .unwrap()
        .into_iter()
        .map(|t| t.turn)
        .collect();
    turns.sort_by_key(|t| t.turn_index);
    turns
}

/// Copilot numbers some tool calls per model session (`bash_14`), so two
/// turns can carry one id. A turn's call is read only from the evidence its
/// own message made; relayhistory keeps one call per id, so the earlier
/// turn shows none rather than the later one's.
#[test]
fn a_reused_tool_call_id_is_read_from_its_own_message() {
    let sandbox = Sandbox::new();
    let turns = ingest_journal(
        &sandbox,
        &journal(&[("view", "call_1", "a.rs"), ("view", "call_1", "b.rs")]),
    );
    let calls: Vec<Vec<Option<String>>> = turns
        .iter()
        .map(|t| t.tool_calls.iter().map(|c| c.target.clone()).collect())
        .collect();
    assert_eq!(calls, [vec![], vec![Some("b.rs".to_string())]]);
    let touched: Vec<_> = turns.iter().map(|t| t.files_touched.clone()).collect();
    assert_eq!(touched, [None, Some(vec!["b.rs".to_string()])]);
}

#[test]
fn every_copilot_file_tool_name_touches_its_file() {
    let sandbox = Sandbox::new();
    let tools = ["view", "read", "edit", "create", "write"];
    let calls: Vec<(&str, String, String)> = tools
        .iter()
        .enumerate()
        .map(|(i, tool)| (*tool, format!("call_{i}"), format!("{tool}.txt")))
        .collect();
    let calls: Vec<(&str, &str, &str)> = calls
        .iter()
        .map(|(t, c, p)| (*t, c.as_str(), p.as_str()))
        .collect();
    let turns = ingest_journal(&sandbox, &journal(&calls));
    let touched: Vec<_> = turns.iter().map(|t| t.files_touched.clone()).collect();
    let want: Vec<_> = tools
        .iter()
        .map(|tool| Some(vec![format!("{tool}.txt")]))
        .collect();
    assert_eq!(touched, want);
}

/// The journal of [`journal`] with no model on its messages, and the
/// `session.shutdown` that closes it: gpt-5.4 grew 100 input, 10 output.
fn unnamed_journal() -> (String, String) {
    let text = journal(&[("view", "call_1", "a.rs")]).replace(r#""model":"gpt-5.4","#, "");
    let shutdown = serde_json::json!({"type": "session.shutdown",
        "id": "00000000-0000-4000-8000-0000000000ff", "parentId": null,
        "timestamp": "2026-09-18T21:31:00.000Z",
        "data": {"modelMetrics": {"gpt-5.4": {"usage": {"inputTokens": 100,
            "outputTokens": 10, "cacheReadTokens": 0, "cacheWriteTokens": 0}}}}});
    (text, format!("{shutdown}\n"))
}

/// Model, input, output and dollar cost per turn, in turn order.
fn priced(turns: &[crate::reader::TurnRecord]) -> Vec<(String, u64, u64, f64)> {
    let pricing = crate::analyze::pricing::load_builtin_pricing();
    turns
        .iter()
        .map(|t| {
            let cost = crate::analyze::cost::cost_for_turn(t, &pricing).expect("priced");
            (t.model.clone(), t.usage.input, t.usage.output, cost.total)
        })
        .collect()
}

/// A message that names no model bills nothing, priced at \$0; its output
/// is on the shutdown's aggregate, under the model the shutdown names.
#[test]
fn a_message_naming_no_model_leaves_its_output_to_the_shutdown() {
    let sandbox = Sandbox::new();
    let (text, shutdown) = unnamed_journal();
    let turns = ingest_journal(&sandbox, &(text + &shutdown));
    let pricing = crate::analyze::pricing::load_builtin_pricing();
    let rate = crate::analyze::cost::lookup_model_rate(
        "gpt-5.4",
        Some(crate::reader::SourceKind::CopilotCli),
        &pricing,
    )
    .unwrap();
    let aggregate = (100.0 * rate.input + 10.0 * rate.output) / 1_000_000.0;
    assert_eq!(
        priced(&turns),
        [
            (String::new(), 0, 0, 0.0),
            ("gpt-5.4".to_string(), 100, 10, aggregate)
        ]
    );
    assert!(crate::analyze::findings::unpriced_usage_findings(&turns, &pricing).is_empty());
}

/// Ingested while running and again after it shuts down, a session bills
/// exactly the shutdown's totals: the message turn the ledger already holds
/// bills the same either way.
#[test]
fn a_session_ingested_before_and_after_its_shutdown_bills_its_totals_once() {
    let sandbox = Sandbox::new();
    let (text, shutdown) = unnamed_journal();
    let running = ingest_journal(&sandbox, &text);
    assert_eq!(priced(&running), [(String::new(), 0, 0, 0.0)]);

    let settled = ingest_journal(&sandbox, &(text + &shutdown));
    let billed: Vec<_> = priced(&settled)
        .into_iter()
        .map(|(model, input, output, _)| (model, input, output))
        .collect();
    assert_eq!(
        billed,
        [(String::new(), 0, 0), ("gpt-5.4".to_string(), 100, 10)]
    );
    let mut ledger = sandbox.ledger();
    assert_eq!(sandbox.ingest(&mut ledger), IngestReport::empty());
}

/// Each tool call of one message reads its own arguments.
#[test]
fn each_call_of_a_message_reads_its_own_arguments() {
    let sandbox = Sandbox::new();
    let second =
        r#"{"toolCallId":"call_2","name":"view","arguments":{"path":"b.rs"},"type":"function"}"#;
    let text = journal(&[("view", "call_1", "a.rs")]).replace(
        r#""type":"function"}]"#,
        &format!(r#""type":"function"}},{second}]"#),
    );
    assert!(text.contains("call_2"));
    let turns = ingest_journal(&sandbox, &text);
    let mut targets: Vec<_> = turns[0]
        .tool_calls
        .iter()
        .map(|c| (c.id.clone(), c.target.clone()))
        .collect();
    targets.sort();
    assert_eq!(
        targets,
        [
            ("call_1".to_string(), Some("a.rs".to_string())),
            ("call_2".to_string(), Some("b.rs".to_string()))
        ]
    );
}

/// A message written in its shutdown's millisecond belongs to that
/// shutdown's interval alone: the next shutdown bills its own growth whole.
#[test]
fn a_message_on_a_shutdown_boundary_counts_in_one_interval() {
    let sandbox = Sandbox::new();
    let at = "2026-09-18T21:30:05.000Z";
    let event = |kind: &str, n: u32, ts: &str, data: serde_json::Value| {
        serde_json::json!({"type": kind, "id": format!("00000000-0000-4000-8000-{n:012}"),
            "parentId": null, "timestamp": ts, "data": data})
        .to_string()
    };
    let totals = |output: u64| {
        serde_json::json!({"modelMetrics": {"gpt-5.4": {"usage": {"inputTokens": 100,
            "outputTokens": output, "cacheReadTokens": 0, "cacheWriteTokens": 0}}}})
    };
    let text = [
        event(
            "session.start",
            0,
            "2026-09-18T21:30:00.000Z",
            serde_json::json!({"sessionId": SID, "version": 1, "producer": "copilot-agent",
                "copilotVersion": "1.0.86", "context": {"cwd": "/tmp/project"}}),
        ),
        event(
            "user.message",
            1,
            "2026-09-18T21:30:01.000Z",
            serde_json::json!({"content": "go", "turnId": "0"}),
        ),
        event(
            "assistant.message",
            2,
            at,
            serde_json::json!({"messageId": "msg-0", "model": "gpt-5.4", "content": "done",
                "turnId": "0", "outputTokens": 10}),
        ),
        event("session.shutdown", 3, at, totals(10)),
        event(
            "session.shutdown",
            4,
            "2026-09-18T21:31:00.000Z",
            totals(25),
        ),
    ]
    .join("\n")
        + "\n";
    let turns = ingest_journal(&sandbox, &text);
    let output: u64 = turns.iter().map(|t| t.usage.output).sum();
    assert_eq!(output, 25);
}
