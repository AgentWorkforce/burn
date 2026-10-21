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
