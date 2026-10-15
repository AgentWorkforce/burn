//! The content records of a Claude turn: its kinds, and its order.

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

fn records_of(transcript: &str) -> crate::source::SessionRecords {
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

/// A prompt, then one response Claude wrote as two records with a tool
/// result between them.
fn split_turn() -> String {
    [
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
    .concat()
}

/// A tool result Claude wrote between two records of one response does
/// not split the turn's content.
#[test]
fn a_turn_split_by_a_tool_result_keeps_its_content_together() {
    let transcript = split_turn();
    let records = records_of(&transcript);
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

/// The tool result a user record carries names the call it answers.
#[test]
fn a_tool_result_block_names_its_call() {
    let records = records_of(&split_turn());
    let results: Vec<Option<&str>> = records
        .user_turns
        .iter()
        .flat_map(|u| &u.blocks)
        .filter(|b| b.kind == crate::reader::UserTurnBlockKind::ToolResult)
        .map(|b| b.tool_use_id.as_deref())
        .collect();
    assert_eq!(results, [Some("toolu_1")]);
    let texts: Vec<Option<&str>> = records
        .user_turns
        .iter()
        .flat_map(|u| &u.blocks)
        .filter(|b| b.kind == crate::reader::UserTurnBlockKind::Text)
        .map(|b| b.tool_use_id.as_deref())
        .collect();
    assert!(!texts.is_empty() && texts.iter().all(Option::is_none));
}

#[test]
fn a_thinking_block_with_text_is_assistant_thinking_content() {
    let transcript = [
        line(json!({
            "parentUuid": null, "isSidechain": false, "type": "user", "uuid": "u1",
            "timestamp": "2026-04-20T00:00:00.000Z", "cwd": "/tmp/project", "sessionId": SESSION,
            "message": {"role": "user", "content": "think"}
        })),
        assistant(
            "a1",
            "u1",
            "2026-04-20T00:00:01.000Z",
            json!({"type": "thinking", "thinking": "weighing options", "signature": "sig"}),
        ),
    ]
    .concat();
    let records = records_of(&transcript);
    let thinking: Vec<_> = records
        .content
        .iter()
        .filter(|c| c.kind == ContentKind::Thinking)
        .map(|c| (c.role, c.text.as_deref()))
        .collect();
    assert_eq!(
        thinking,
        [(
            crate::reader::ContentRole::Assistant,
            Some("weighing options")
        )]
    );
}

/// An empty tool result is still a block of its user turn; empty text is
/// not.
#[test]
fn an_empty_tool_result_is_a_block_and_empty_text_is_not() {
    let transcript = [
        line(json!({
            "parentUuid": null, "isSidechain": false, "type": "user", "uuid": "u1",
            "timestamp": "2026-04-20T00:00:00.000Z", "cwd": "/tmp/project", "sessionId": SESSION,
            "message": {"role": "user", "content": "run it"}
        })),
        assistant(
            "a1",
            "u1",
            "2026-04-20T00:00:01.000Z",
            json!({"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "true"}}),
        ),
        line(json!({
            "parentUuid": "a1", "isSidechain": false, "type": "user", "uuid": "r1",
            "timestamp": "2026-04-20T00:00:02.000Z", "cwd": "/tmp/project", "sessionId": SESSION,
            "message": {"role": "user", "content": [
                {"type": "text", "text": ""},
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": ""}
            ]}
        })),
    ]
    .concat();
    let records = records_of(&transcript);
    let result_turn = records
        .user_turns
        .iter()
        .find(|u| u.user_uuid == "r1")
        .expect("the tool result's user turn");
    let blocks: Vec<_> = result_turn
        .blocks
        .iter()
        .map(|b| (b.kind, b.byte_len, b.tool_use_id.as_deref()))
        .collect();
    assert_eq!(
        blocks,
        [(
            crate::reader::UserTurnBlockKind::ToolResult,
            0,
            Some("toolu_1")
        )]
    );
}
