//! The invocation a sidechain turn runs under.

use serde_json::{json, Value};

use crate::reader::Harness;
use crate::source::locate::{load_session, SessionLocator};
use crate::source::relayhistory::records_from_evidence;
use crate::source::{Billing, SessionRecords};

const SESSION: &str = "44444444-4444-4444-4444-444444444444";

fn record(uuid: &str, parent: Option<&str>, sidechain: bool, ts: u32, body: Value) -> String {
    let mut line = json!({
        "parentUuid": parent, "isSidechain": sidechain, "uuid": uuid,
        "timestamp": format!("2026-04-20T00:00:{ts:02}.000Z"),
        "cwd": "/tmp/project", "sessionId": SESSION,
    });
    line.as_object_mut()
        .unwrap()
        .extend(body.as_object().unwrap().clone());
    format!("{line}\n")
}

fn assistant(id: &str, content: Value) -> Value {
    json!({"type": "assistant", "message": {
        "model": "claude-sonnet-4-6", "id": id, "type": "message", "role": "assistant",
        "content": content, "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 5,
                  "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
    }})
}

fn records_of(transcript: &str) -> SessionRecords {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("{SESSION}.jsonl"));
    std::fs::write(&path, transcript).unwrap();
    let loaded = load_session(
        &SessionLocator::Path {
            harness: Harness::ClaudeCode,
            path,
        },
        &Default::default(),
    )
    .unwrap();
    records_from_evidence(&loaded.evidence, &Billing::settled())
}

#[test]
fn consecutive_sidechain_responses_run_under_the_prompt_that_started_them() {
    let task = |id: &str| {
        json!([{"type": "tool_use", "id": id, "name": "Task",
                "input": {"subagent_type": "Explore", "description": "map it"}}])
    };
    let transcript = [
        record(
            "u0",
            None,
            false,
            0,
            json!({"type": "user", "message": {"role": "user", "content": "go"}}),
        ),
        record(
            "a0",
            Some("u0"),
            false,
            1,
            assistant("msg_main", task("toolu_main")),
        ),
        record(
            "u1",
            Some("a0"),
            true,
            2,
            json!({"type": "user", "message": {"role": "user", "content": "map it"}}),
        ),
        // A sidechain response that spawns, followed directly by another.
        record(
            "s1",
            Some("u1"),
            true,
            3,
            assistant("msg_s1", task("toolu_nested")),
        ),
        record(
            "s2",
            Some("s1"),
            true,
            4,
            assistant("msg_s2", json!([{"type": "text", "text": "done"}])),
        ),
    ]
    .concat();
    let records = records_of(&transcript);
    let agent = |message: &str| {
        records
            .turns
            .iter()
            .find(|t| t.message_id.contains(message))
            .and_then(|t| t.subagent.as_ref())
            .and_then(|s| s.agent_id.clone())
    };
    assert_eq!(agent("msg_s1"), Some("u1".to_string()));
    assert_eq!(agent("msg_s2"), Some("u1".to_string()));
}
