//! A turn's content stays together where its first record sits, even when
//! Claude wrote a tool result between two records of the same response.

use serde_json::json;

use crate::reader::{ContentKind, Harness};
use crate::source::locate::{load_session, SessionLocator};
use crate::source::relayhistory::records_from_evidence;
use crate::source::Billing;

const SESSION: &str = "33333333-3333-3333-3333-333333333333";

fn line(value: serde_json::Value) -> String {
    format!("{value}\n")
}

fn assistant(uuid: &str, parent: &str, ts: &str, block: serde_json::Value) -> String {
    line(json!({
        "parentUuid": parent, "isSidechain": false, "type": "assistant", "uuid": uuid,
        "timestamp": ts, "cwd": "/tmp/project", "sessionId": SESSION,
        "message": {
            "model": "claude-sonnet-4-6", "id": "msg_split", "type": "message",
            "role": "assistant", "content": [block], "stop_reason": null,
            "usage": {"input_tokens": 3, "output_tokens": 5,
                      "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
        }
    }))
}

#[test]
fn a_turn_split_by_a_tool_result_keeps_its_content_together() {
    let transcript = [
        line(json!({
            "parentUuid": null, "isSidechain": false, "type": "user", "uuid": "u1",
            "timestamp": "2026-04-20T00:00:00.000Z", "cwd": "/tmp/project", "sessionId": SESSION,
            "message": {"role": "user", "content": "list files"}
        })),
        assistant(
            "a1",
            "u1",
            "2026-04-20T00:00:01.000Z",
            json!({"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}),
        ),
        line(json!({
            "parentUuid": "a1", "isSidechain": false, "type": "user", "uuid": "r1",
            "timestamp": "2026-04-20T00:00:02.000Z", "cwd": "/tmp/project", "sessionId": SESSION,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.rs"}
            ]}
        })),
        assistant(
            "a2",
            "r1",
            "2026-04-20T00:00:03.000Z",
            json!({"type": "text", "text": "one file"}),
        ),
    ]
    .concat();
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
    let records = records_from_evidence(&loaded.evidence, &Billing::settled());
    let kinds: Vec<ContentKind> = records.content.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        [
            ContentKind::Text,
            ContentKind::ToolUse,
            ContentKind::Text,
            ContentKind::ToolResult
        ],
        "{:#?}",
        records.content
    );
}
