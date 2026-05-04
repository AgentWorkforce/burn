//! Conformance tests for the OpenCode streaming ingestor — Rust counterpart
//! to `packages/reader/src/opencode-stream.test.ts` (issue #258). The static
//! file-derived parity test in TS depends on `parseOpencodeSession` (port
//! tracked in #257); when that lands, the corresponding parity case can be
//! lifted in here.

use super::*;
use crate::types::{ContentKind, ToolResultEventSource, UsageAttribution};
use serde_json::json;

fn ingestor() -> OpencodeStreamIngestor {
    create_opencode_stream_ingestor(OpencodeStreamIngestOptions {
        content_mode: Some(ContentStoreMode::Full),
        tokenizer: Some(UserTurnTokenizer::Heuristic),
        cursor: None,
    })
    .expect("ingestor")
}

#[test]
fn normalizes_a_stream_owned_session_into_burn_records_on_idle() {
    let mut ing = ingestor();

    ing.ingest(
        &json!({
            "type": "session.created",
            "properties": { "info": { "id": "ses_stream", "directory": "/tmp/project" } }
        }),
        Some("1"),
    );
    ing.ingest(
        &json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "msg_stream_user",
                    "sessionID": "ses_stream",
                    "role": "user",
                    "time": { "created": 1_777_000_000_000_i64 }
                }
            }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "id": "prt_user_text",
                    "sessionID": "ses_stream",
                    "messageID": "msg_stream_user",
                    "type": "text",
                    "text": "list files"
                }
            }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "msg_stream_asst",
                    "sessionID": "ses_stream",
                    "role": "assistant",
                    "time": { "created": 1_777_000_001_000_i64 },
                    "providerID": "anthropic",
                    "modelID": "claude-sonnet-4-5",
                    "path": { "cwd": "/tmp/project" },
                    "tokens": {
                        "input": 10,
                        "output": 20,
                        "reasoning": 0,
                        "cache": { "read": 5, "write": 7 }
                    }
                }
            }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "id": "prt_asst_tool",
                    "sessionID": "ses_stream",
                    "messageID": "msg_stream_asst",
                    "type": "tool",
                    "callID": "toolu_bash_1",
                    "tool": "bash",
                    "state": {
                        "status": "completed",
                        "input": { "command": "ls" },
                        "output": "a.ts\n"
                    }
                }
            }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "id": "prt_step_finish",
                    "sessionID": "ses_stream",
                    "messageID": "msg_stream_asst",
                    "type": "step-finish",
                    "reason": "end_turn",
                    "tokens": {
                        "input": 10,
                        "output": 20,
                        "reasoning": 0,
                        "cache": { "read": 5, "write": 7 }
                    }
                }
            }
        }),
        None,
    );

    let result = ing.ingest(
        &json!({
            "type": "session.idle",
            "properties": { "sessionID": "ses_stream" }
        }),
        Some("7"),
    );

    assert_eq!(result.turns.len(), 1);
    assert_eq!(result.turns[0].message_id, "msg_stream_asst");
    assert_eq!(result.turns[0].turn_index, 0);
    assert_eq!(result.turns[0].tool_calls[0].name, "bash");
    assert_eq!(result.turns[0].tool_calls[0].target.as_deref(), Some("ls"));
    assert_eq!(
        result.turns[0].usage,
        Usage {
            input: 10,
            output: 20,
            reasoning: 0,
            cache_read: 5,
            cache_create_5m: 7,
            cache_create_1h: 0,
        }
    );

    assert_eq!(result.tool_result_events.len(), 1);
    let event = &result.tool_result_events[0];
    assert_eq!(event.tool_use_id, "toolu_bash_1");
    assert_eq!(event.event_index, 0);
    assert_eq!(event.usage_attribution, Some(UsageAttribution::SingleToolTurn));
    assert_eq!(event.usage.as_ref().map(|u| u.input), Some(10));
    assert_eq!(event.content_length, Some("a.ts\n".len() as u64));
    assert_eq!(event.event_source, ToolResultEventSource::ToolResult);

    let tool_results: Vec<_> = result
        .content
        .iter()
        .filter(|c| matches!(c.kind, ContentKind::ToolResult))
        .collect();
    assert_eq!(tool_results.len(), 1);
    let texts: Vec<_> = result
        .content
        .iter()
        .filter(|c| matches!(c.kind, ContentKind::Text))
        .collect();
    assert_eq!(texts.len(), 1);
    assert_eq!(result.user_turns.len(), 1);
    assert_eq!(
        result.user_turns[0].blocks[0].kind,
        crate::types::UserTurnBlockKind::Text
    );
    assert_eq!(result.cursor.last_event_id.as_deref(), Some("7"));

    let second = ing.ingest(
        &json!({
            "type": "session.idle",
            "properties": { "sessionID": "ses_stream" }
        }),
        None,
    );
    assert_eq!(second.turns.len(), 0);
    assert_eq!(second.tool_result_events.len(), 0);
}

#[test]
fn does_not_emit_direct_records_for_sessions_predating_the_stream() {
    let mut ing = create_opencode_stream_ingestor(OpencodeStreamIngestOptions {
        content_mode: Some(ContentStoreMode::Full),
        tokenizer: None,
        cursor: None,
    })
    .expect("ingestor");
    ing.ingest(
        &json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "msg_existing_asst",
                    "sessionID": "ses_existing",
                    "role": "assistant",
                    "time": { "created": 1 },
                    "tokens": { "input": 1, "output": 1 }
                }
            }
        }),
        None,
    );
    let result = ing.ingest(
        &json!({
            "type": "session.idle",
            "properties": { "sessionID": "ses_existing" }
        }),
        None,
    );
    assert_eq!(result.turns.len(), 0);
}

#[test]
fn does_not_duplicate_tool_events_when_earlier_assistant_finalizes_later() {
    let mut ing = ingestor();

    ing.ingest(
        &json!({
            "type": "session.created",
            "properties": { "info": { "id": "ses_out_of_order", "directory": "/tmp/project" } }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "msg_asst_1",
                    "sessionID": "ses_out_of_order",
                    "role": "assistant",
                    "time": { "created": 100 },
                    "providerID": "anthropic",
                    "modelID": "claude-sonnet-4-5"
                }
            }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "id": "part_tool_1",
                    "sessionID": "ses_out_of_order",
                    "messageID": "msg_asst_1",
                    "type": "tool",
                    "callID": "tool_1",
                    "tool": "bash",
                    "state": { "input": { "command": "one" }, "output": "one\n" }
                }
            }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "msg_asst_2",
                    "sessionID": "ses_out_of_order",
                    "role": "assistant",
                    "time": { "created": 200 },
                    "providerID": "anthropic",
                    "modelID": "claude-sonnet-4-5",
                    "tokens": { "input": 2, "output": 2 }
                }
            }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "id": "part_tool_2",
                    "sessionID": "ses_out_of_order",
                    "messageID": "msg_asst_2",
                    "type": "tool",
                    "callID": "tool_2",
                    "tool": "bash",
                    "state": { "input": { "command": "two" }, "output": "two\n" }
                }
            }
        }),
        None,
    );

    let first = ing.ingest(
        &json!({
            "type": "session.idle",
            "properties": { "sessionID": "ses_out_of_order" }
        }),
        None,
    );
    let actual: Vec<(String, u64)> = first
        .tool_result_events
        .iter()
        .map(|e| (e.tool_use_id.clone(), e.event_index))
        .collect();
    assert_eq!(actual, vec![("tool_2".to_string(), 0)]);

    ing.ingest(
        &json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "id": "part_step_1",
                    "sessionID": "ses_out_of_order",
                    "messageID": "msg_asst_1",
                    "type": "step-finish",
                    "reason": "end_turn",
                    "tokens": { "input": 1, "output": 1 }
                }
            }
        }),
        None,
    );

    let second = ing.ingest(
        &json!({
            "type": "session.idle",
            "properties": { "sessionID": "ses_out_of_order" }
        }),
        None,
    );
    let actual_second: Vec<(String, u64)> = second
        .tool_result_events
        .iter()
        .map(|e| (e.tool_use_id.clone(), e.event_index))
        .collect();
    assert_eq!(actual_second, vec![("tool_1".to_string(), 1)]);
    assert!(
        !second
            .tool_result_events
            .iter()
            .any(|e| e.tool_use_id == "tool_2"),
        "already emitted later assistant tool must not be re-emitted with a shifted eventIndex"
    );
    assert_eq!(
        second
            .cursor
            .next_tool_event_index_by_session
            .as_ref()
            .and_then(|m| m.get("ses_out_of_order"))
            .copied(),
        Some(2)
    );
}

#[test]
fn drops_buffered_parts_for_deleted_sessions() {
    let mut ing = create_opencode_stream_ingestor(OpencodeStreamIngestOptions {
        content_mode: None,
        tokenizer: Some(UserTurnTokenizer::Heuristic),
        cursor: None,
    })
    .expect("ingestor");
    ing.ingest(
        &json!({
            "type": "session.created",
            "properties": { "info": { "id": "ses_deleted", "directory": "/tmp/project" } }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "message.part.updated",
            "properties": {
                "part": {
                    "id": "part_old_tool",
                    "sessionID": "ses_deleted",
                    "messageID": "msg_reused",
                    "type": "tool",
                    "callID": "old_tool",
                    "tool": "bash",
                    "state": { "input": { "command": "old" }, "output": "old\n" }
                }
            }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "session.deleted",
            "properties": { "sessionID": "ses_deleted" }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "session.created",
            "properties": { "info": { "id": "ses_deleted", "directory": "/tmp/project" } }
        }),
        None,
    );
    ing.ingest(
        &json!({
            "type": "message.updated",
            "properties": {
                "info": {
                    "id": "msg_reused",
                    "sessionID": "ses_deleted",
                    "role": "assistant",
                    "time": { "created": 300 },
                    "providerID": "anthropic",
                    "modelID": "claude-sonnet-4-5",
                    "tokens": { "input": 1, "output": 1 }
                }
            }
        }),
        None,
    );

    let result = ing.ingest(
        &json!({
            "type": "session.idle",
            "properties": { "sessionID": "ses_deleted" }
        }),
        None,
    );
    assert_eq!(result.turns.len(), 1);
    assert_eq!(result.turns[0].tool_calls.len(), 0);
    assert_eq!(result.tool_result_events.len(), 0);
}

#[test]
fn cursor_round_trips_via_serde() {
    // Resume from a TS-shaped cursor JSON: emittedToolEventIds with numeric
    // suffix → derive next index per session.
    let cursor_json = json!({
        "lastEventId": "42",
        "emittedMessageIds": ["msg_a"],
        "emittedToolEventIds": ["ses_a|msg_a|tool_a|0", "ses_a|msg_a|tool_b|1"],
    });
    let cursor: OpencodeStreamCursorState =
        serde_json::from_value(cursor_json.clone()).expect("deserialize");
    let ing = create_opencode_stream_ingestor(OpencodeStreamIngestOptions {
        content_mode: None,
        tokenizer: None,
        cursor: Some(cursor),
    })
    .expect("ingestor");
    let snapshot = ing.snapshot_cursor();
    // Derived next-index for ses_a should be 2 (max suffix + 1).
    assert_eq!(
        snapshot
            .next_tool_event_index_by_session
            .as_ref()
            .and_then(|m| m.get("ses_a"))
            .copied(),
        Some(2)
    );
    // Re-serialize and verify the round-trip preserves the input fields
    // (lastEventId, emittedMessageIds, emittedToolEventIds).
    let reser = serde_json::to_value(&snapshot).expect("serialize");
    assert_eq!(reser["lastEventId"], cursor_json["lastEventId"]);
    assert_eq!(
        reser["emittedMessageIds"],
        cursor_json["emittedMessageIds"]
    );
    assert_eq!(
        reser["emittedToolEventIds"],
        cursor_json["emittedToolEventIds"]
    );
}

#[test]
fn rejects_cl100k_tokenizer_until_implemented() {
    let result = create_opencode_stream_ingestor(OpencodeStreamIngestOptions {
        content_mode: None,
        tokenizer: Some(UserTurnTokenizer::Cl100k),
        cursor: None,
    });
    let err = match result {
        Ok(_) => panic!("must reject cl100k"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("cl100k"));
}

#[test]
fn iso_timestamp_matches_js_date_to_iso_string() {
    // Cross-checked against `new Date(ms).toISOString()`.
    assert_eq!(unix_ms_to_iso(1_777_000_001_000), "2026-04-24T03:06:41.000Z");
    assert_eq!(unix_ms_to_iso(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(unix_ms_to_iso(1), "1970-01-01T00:00:00.001Z");
    // Pre-epoch (negative ms) — JS Date supports it; rem_euclid gives the
    // correct positive milliseconds.
    assert_eq!(unix_ms_to_iso(-1), "1969-12-31T23:59:59.999Z");
}
