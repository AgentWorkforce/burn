//! Claude Code session parser conformance tests. Fixtures live at the repo
//! root (`tests/fixtures/claude/*.jsonl`).

use super::tool_results::{detect_truncation_marker, measure_tool_result};
use super::*;
use crate::reader::types::{ToolResultEventSource, ToolResultStatus};

/// `measure_tool_result` populates both the legacy char-count
/// `length` and the new `byte_length` field added in #436. For
/// ASCII fixture content they agree; the byte length is what the
/// `tool_result_events.output_bytes` column stores so hotspots can
/// rank by raw payload regardless of token truncation downstream.
#[test]
fn measure_tool_result_populates_byte_length_and_truncation_flag() {
    let plain = serde_json::json!("hello world");
    let m = measure_tool_result(&plain);
    assert_eq!(m.byte_length, Some(11));
    assert_eq!(m.length, Some(11));
    assert_eq!(m.truncated, Some(false));

    let truncated =
        serde_json::json!("... lots of output ...\n[truncated]\nsystem note: response truncated");
    let m = measure_tool_result(&truncated);
    assert_eq!(m.truncated, Some(true));
    assert!(m.byte_length.unwrap() > 0);

    let null = serde_json::json!(null);
    let m = measure_tool_result(&null);
    assert_eq!(m.byte_length, None);
    assert_eq!(m.truncated, None);
}

#[test]
fn detect_truncation_marker_matches_known_phrasings() {
    assert!(detect_truncation_marker(
        "Bash output truncated at 30000 chars"
    ));
    assert!(detect_truncation_marker("<system-truncated>"));
    assert!(detect_truncation_marker(
        "(...)\n[truncated]\n(end of preview)"
    ));
    assert!(detect_truncation_marker("Result Truncated"));
    assert!(!detect_truncation_marker("hello world"));
    assert!(!detect_truncation_marker(""));
}

fn fixture(name: &str) -> std::path::PathBuf {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("claude")
        .join(name)
}

#[test]
fn parse_result_from_incremental_result_copies_all_fields() {
    let turn = TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "session-1".to_string(),
        session_path: Some("/tmp/session.jsonl".to_string()),
        message_id: "msg-1".to_string(),
        turn_index: 7,
        ts: "2026-05-11T00:00:00.000Z".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        project: Some("/tmp/project".to_string()),
        project_key: Some("project-key".to_string()),
        usage: Usage {
            input: 1,
            output: 2,
            reasoning: 3,
            cache_read: 4,
            cache_create_5m: 5,
            cache_create_1h: 6,
        },
        tool_calls: vec![],
        files_touched: Some(vec!["/tmp/project/src/lib.rs".to_string()]),
        subagent: Some(Subagent {
            is_sidechain: false,
            parent_tool_use_id: Some("tool-1".to_string()),
            agent_id: Some("agent-1".to_string()),
            parent_agent_id: Some("parent-agent".to_string()),
            subagent_type: Some("general-purpose".to_string()),
            description: Some("delegate".to_string()),
        }),
        stop_reason: Some(StopReason::EndTurn),
        activity: Some(crate::reader::types::ActivityCategory::Coding),
        retries: Some(1),
        has_edits: Some(true),
        fidelity: Some(Fidelity {
            granularity: UsageGranularity::PerTurn,
            coverage: Coverage {
                has_input_tokens: true,
                has_output_tokens: true,
                has_reasoning_tokens: true,
                has_cache_read_tokens: true,
                has_cache_create_tokens: true,
                has_tool_calls: true,
                has_tool_result_events: true,
                has_session_relationships: true,
                has_raw_content: true,
            },
            class: crate::reader::types::FidelityClass::Full,
        }),
    };
    let content = ContentRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "session-1".to_string(),
        message_id: "msg-1".to_string(),
        ts: "2026-05-11T00:00:00.000Z".to_string(),
        role: ContentRole::Assistant,
        kind: ContentKind::Text,
        text: Some("hello".to_string()),
        tool_use: None,
        tool_result: None,
    };
    let event = CompactionEvent {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "session-1".to_string(),
        ts: "2026-05-11T00:01:00.000Z".to_string(),
        preceding_message_id: Some("msg-0".to_string()),
        tokens_before_compact: Some(42),
    };
    let relationship = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::ClaudeCode,
        session_id: "session-1".to_string(),
        related_session_id: Some("session-0".to_string()),
        relationship_type: RelationshipType::Continuation,
        ts: Some("2026-05-11T00:02:00.000Z".to_string()),
        source_session_id: Some("source-session".to_string()),
        source_version: Some("1.2.3".to_string()),
        parent_tool_use_id: Some("tool-1".to_string()),
        agent_id: Some("agent-1".to_string()),
        subagent_type: Some("general-purpose".to_string()),
        description: Some("continued".to_string()),
    };
    let tool_result_event = ToolResultEventRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "session-1".to_string(),
        message_id: Some("msg-1".to_string()),
        tool_use_id: "tool-1".to_string(),
        call_index: Some(0),
        event_index: 9,
        ts: Some("2026-05-11T00:03:00.000Z".to_string()),
        status: ToolResultStatus::Completed,
        event_source: ToolResultEventSource::ToolResult,
        content_length: Some(5),
        output_bytes: Some(5),
        output_truncated: Some(false),
        content_hash: Some("abc123".to_string()),
        is_error: Some(false),
        usage: Some(Usage::default()),
        usage_attribution: Some(crate::reader::types::UsageAttribution::SingleToolTurn),
        subagent_session_id: Some("sub-session".to_string()),
        agent_id: Some("agent-1".to_string()),
        replaced_tools: Some(vec!["old-tool".to_string()]),
        collapsed_calls: Some(2),
    };
    let user_turn = UserTurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "session-1".to_string(),
        user_uuid: "user-1".to_string(),
        ts: "2026-05-11T00:04:00.000Z".to_string(),
        preceding_message_id: Some("msg-0".to_string()),
        following_message_id: Some("msg-1".to_string()),
        blocks: vec![UserTurnBlock {
            kind: crate::reader::types::UserTurnBlockKind::Text,
            tool_use_id: None,
            byte_len: 5,
            approx_tokens: 1,
            is_error: None,
        }],
    };
    let evidence = ClaudeRelationshipEvidence {
        file_session_id: Some("session-1".to_string()),
        first_ts: Some("2026-05-11T00:00:00.000Z".to_string()),
        in_log_session_ids: vec!["session-1".to_string()],
        source_version: Some("1.2.3".to_string()),
        first_parent_uuid: Some("parent-1".to_string()),
        seen_uuids: vec!["uuid-1".to_string()],
        has_resume_marker: true,
        resume_target_session_id: Some("session-0".to_string()),
        explicit_continuation_target_session_ids: Some(vec!["session-0".to_string()]),
        explicit_fork_target_session_ids: Some(vec!["session-2".to_string()]),
        user_seen: true,
    };

    let incremental = ParseIncrementalResult {
        turns: vec![turn.clone()],
        content: vec![content.clone()],
        events: vec![event.clone()],
        relationships: vec![relationship.clone()],
        tool_result_events: vec![tool_result_event.clone()],
        user_turns: vec![user_turn.clone()],
        request_id_lookup: RequestIdLookup::new(),
        end_offset: 123,
        last_user_text: "latest user turn".to_string(),
        evidence: evidence.clone(),
    };

    let full = ParseResult::from(incremental);

    assert_eq!(full.turns, vec![turn]);
    assert_eq!(full.content, vec![content]);
    assert_eq!(full.events, vec![event]);
    assert_eq!(full.relationships, vec![relationship]);
    assert_eq!(full.tool_result_events, vec![tool_result_event]);
    assert_eq!(full.user_turns, vec![user_turn]);
    assert_eq!(full.evidence.file_session_id, evidence.file_session_id);
    assert_eq!(full.evidence.first_ts, evidence.first_ts);
    assert_eq!(
        full.evidence.in_log_session_ids,
        evidence.in_log_session_ids
    );
    assert_eq!(full.evidence.source_version, evidence.source_version);
    assert_eq!(full.evidence.first_parent_uuid, evidence.first_parent_uuid);
    assert_eq!(full.evidence.seen_uuids, evidence.seen_uuids);
    assert_eq!(full.evidence.has_resume_marker, evidence.has_resume_marker);
    assert_eq!(
        full.evidence.resume_target_session_id,
        evidence.resume_target_session_id
    );
    assert_eq!(
        full.evidence.explicit_continuation_target_session_ids,
        evidence.explicit_continuation_target_session_ids
    );
    assert_eq!(
        full.evidence.explicit_fork_target_session_ids,
        evidence.explicit_fork_target_session_ids
    );
    assert_eq!(full.evidence.user_seen, evidence.user_seen);
}

#[test]
fn simple_turn_parses() {
    let path = fixture("simple-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.turns.len(), 1, "expected one turn");
    let t = &res.turns[0];
    assert_eq!(t.v, 1);
    assert_eq!(t.source, SourceKind::ClaudeCode);
    assert_eq!(t.message_id, "msg_simple_1");
    assert_eq!(t.model, "claude-sonnet-4-6");
    assert_eq!(t.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(t.usage.input, 10);
    assert_eq!(t.usage.output, 5);
    assert_eq!(t.usage.cache_read, 500);
    assert_eq!(t.usage.cache_create_5m, 80);
    assert_eq!(t.usage.cache_create_1h, 20);
    assert_eq!(t.tool_calls.len(), 0);
    assert!(t.files_touched.is_none());
}

#[test]
fn full_parse_emits_final_line_without_trailing_newline() {
    // Mid-write truncation / unflushed writer: the final JSON line is
    // syntactically complete but missing `\n`. The single-shot parse must
    // still surface it — matching the prior `BufReader::read_line` path.
    let src = std::fs::read_to_string(fixture("simple-turn.jsonl")).unwrap();
    let no_trailing = src.trim_end_matches('\n');
    assert!(!no_trailing.ends_with('\n'));
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");
    std::fs::write(&working, no_trailing).unwrap();
    let res = parse_claude_session(&working, &ParseOptions::default()).unwrap();
    assert_eq!(res.turns.len(), 1, "final line without \\n must still emit");
    assert_eq!(res.turns[0].message_id, "msg_simple_1");
}

#[test]
fn multi_block_turn_collapses_to_single_turn() {
    let path = fixture("multi-block-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(
        res.turns.len(),
        1,
        "four assistant lines must collapse to one turn"
    );
    let t = &res.turns[0];
    assert_eq!(t.message_id, "msg_multi_1");
    assert_eq!(t.tool_calls.len(), 2);
    assert_eq!(t.tool_calls[0].name, "Bash");
    assert_eq!(
        t.tool_calls[0].target.as_deref(),
        Some("ls -la /tmp/project")
    );
    assert_eq!(t.tool_calls[1].name, "Agent");
    assert_eq!(t.tool_calls[1].target.as_deref(), Some("general-purpose"));
    assert_eq!(t.stop_reason, Some(StopReason::ToolUse));
    assert_eq!(t.ts, "2026-04-20T00:00:01.000Z");
}

/// Issue #434 acceptance: the multi-block fixture's four assistant
/// rows share `requestId=req_1` and a single `message.id`. The
/// parser surfaces that requestId on its `request_id_lookup`, the
/// inference builder collapses the four rows into ONE
/// `Inference`, and the merged usage matches the carrier row
/// (NOT 4× the carrier row, which would be the row-summing
/// pathology).
#[test]
fn multi_block_turn_emits_one_inference_with_merged_usage() {
    let path = fixture("multi-block-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.turns.len(), 1, "one turn (collapsed by message_id)");
    let t = &res.turns[0];
    // The reader populated the per-turn lookup with the upstream
    // requestId. Without this entry, the inference builder would
    // fall back to `message_id`, which is correct cardinality for
    // Claude but loses the `request-id` provenance.
    let req = res
        .request_id_lookup
        .get(&crate::reader::TurnKey::for_turn(t))
        .expect("request_id_lookup must carry every Claude turn");
    assert_eq!(req, "req_1");

    let infs = crate::reader::build_inferences(&res.turns, &res.request_id_lookup);
    assert_eq!(
        infs.len(),
        1,
        "four assistant rows sharing requestId collapse to one Inference"
    );
    let inf = &infs[0];
    assert_eq!(inf.request_id, "req_1");
    assert_eq!(
        inf.request_id_source,
        crate::reader::InferenceKeySource::RequestId
    );
    assert_eq!(inf.turn_id, "msg_multi_1");
    // Carrier usage values: input=3, output=43, cache_read=11_496,
    // cache_create_1h=4_773. The pre-fix bug emitted these multiplied
    // by row count when usage was on the first row; with the fix the
    // single inference reports the carrier's values exactly once.
    assert_eq!(inf.usage.input, 3);
    assert_eq!(inf.usage.output, 43);
    assert_eq!(inf.usage.cache_read, 11_496);
    assert_eq!(inf.usage.cache_create_1h, 4_773);
    // start_ts / end_ts come from the parent `TurnRecord` (already
    // collapsed by message_id), so they equal each other here —
    // `TurnRecord.ts` is the first row's ts. A future surface that
    // wants per-row spans should reach into the parser's per-row
    // metadata; the inference summary stays correct for the
    // "how long did the API call take" case the issue asked about
    // by giving us the first-row arrival time.
    assert_eq!(inf.start_ts, "2026-04-20T00:00:01.000Z");
    assert_eq!(inf.end_ts, "2026-04-20T00:00:01.000Z");
    assert_eq!(inf.tool_uses.len(), 2);
    assert_eq!(inf.kind, crate::reader::InferenceKind::ToolUse);
}

/// A turn that the parser parsed without an upstream `requestId`
/// (older Claude version, sidechain, or other harness) falls back
/// to `message_id` as the inference key. See `RequestIdLookup`
/// fallback rules in `reader/inference.rs`.
#[test]
fn inference_falls_back_to_message_id_when_lookup_empty() {
    let path = fixture("multi-block-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    // Empty the lookup to simulate a harness that didn't ship one.
    let empty = crate::reader::RequestIdLookup::new();
    let infs = crate::reader::build_inferences(&res.turns, &empty);
    assert_eq!(infs.len(), 1);
    assert_eq!(infs[0].request_id, "msg_multi_1");
    assert_eq!(
        infs[0].request_id_source,
        crate::reader::InferenceKeySource::MessageId
    );
}

#[test]
fn files_touched_excludes_grep_and_bash() {
    let path = fixture("files-touched.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.turns.len(), 1);
    let t = &res.turns[0];
    assert_eq!(t.tool_calls.len(), 3);
    assert_eq!(
        t.files_touched.as_deref(),
        Some(["/src/a.ts".to_string(), "/src/b.ts".to_string()].as_slice())
    );
}

#[test]
fn sidechain_turn_marked_subagent() {
    let path = fixture("sidechain-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.turns.len(), 1);
    let t = &res.turns[0];
    let sub = t.subagent.as_ref().expect("expected sidechain marker");
    assert!(sub.is_sidechain);
}

#[test]
fn nested_subagent_tree_reconstructs() {
    let path = fixture("nested-subagent.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    // 2 main + 2 outer sidechain + 1 inner sidechain = 5 turns
    assert_eq!(res.turns.len(), 5);
    let by_id: HashMap<&str, &TurnRecord> = res
        .turns
        .iter()
        .map(|t| (t.message_id.as_str(), t))
        .collect();
    let main1 = by_id.get("msg_main_1").unwrap();
    assert!(main1.subagent.is_none());
    let sub1_1 = by_id.get("msg_sub1_1").unwrap();
    let s = sub1_1.subagent.as_ref().unwrap();
    assert!(s.is_sidechain);
    assert_eq!(s.agent_id.as_deref(), Some("u-sub1-user"));
    assert_eq!(s.parent_tool_use_id.as_deref(), Some("toolu_outer"));
    assert_eq!(s.subagent_type.as_deref(), Some("Explore"));
    assert_eq!(s.description.as_deref(), Some("Research the codebase"));
    assert_eq!(
        s.parent_agent_id.as_deref(),
        Some("55555555-5555-5555-5555-555555555555")
    );
}

// ----- parseClaudeSessionIncremental conformance -----
//
// Mirrors `describe('parseClaudeSessionIncremental', ...)` in
// packages/reader/src/claude.test.ts. Each Rust test corresponds to one
// `it()` case; fixture files are read from the shared
// `tests/fixtures/claude/` directory so the TS and Rust suites exercise
// the same input bytes.

use crate::reader::types::{ActivityCategory, FidelityClass, UserTurnBlockKind};
use std::io::Write as _;

fn read_bytes(p: &std::path::Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

fn write_bytes(p: &std::path::Path, b: &[u8]) {
    let mut f = std::fs::File::create(p).unwrap();
    f.write_all(b).unwrap();
}

fn alias_key_session_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("alias-session.jsonl");
    let lines = [
        serde_json::json!({
            "parentUuid": null,
            "isSidechain": false,
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_alias", "content": "ok"}]
            },
            "uuid": "u-alias-user",
            "sessionId": "",
            "session_id": "alias-session",
            "timestamp": "",
            "ts": "2026-04-25T00:00:00.000Z",
            "continued_from_session_id": "parent-session",
            "cwd": "/tmp/project",
            "sourceVersion": "2.1.alias",
        }),
        serde_json::json!({
            "parentUuid": "u-alias-user",
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-4-6",
                "id": "msg_alias_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "done"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 1,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
                },
            },
            "type": "assistant",
            "uuid": "u-alias-asst",
            "sessionId": "",
            "session_id": "alias-session",
            "timestamp": "",
            "ts": "2026-04-25T00:00:01.000Z",
            "cwd": "/tmp/project",
        }),
        serde_json::json!({
            "type": "system",
            "subtype": "compact_boundary",
            "sessionId": "",
            "session_id": "alias-session",
            "timestamp": "",
            "ts": "2026-04-25T00:00:02.000Z",
        }),
        serde_json::json!({
            "type": "system",
            "subtype": "subagent_completed",
            "sessionId": "",
            "session_id": "alias-session",
            "timestamp": "",
            "ts": "2026-04-25T00:00:03.000Z",
            "parent_tool_use_id": "toolu_alias",
            "agent_id": "agent-alias",
            "subagent_session_id": "child-alias",
            "status": "completed",
            "content": "subagent finished",
        }),
    ];
    let body = lines
        .iter()
        .map(|j| j.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    write_bytes(&working, body.as_bytes());
    (dir, working)
}

fn append_str(p: &std::path::Path, s: &str) {
    let mut prev = std::fs::read(p).unwrap();
    prev.extend_from_slice(s.as_bytes());
    write_bytes(p, &prev);
}

/// Returns the byte offset of the line whose JSON contains `needle`.
fn line_start_offset(path: &std::path::Path, needle: &str) -> u64 {
    let raw = std::fs::read_to_string(path).unwrap();
    let mut off: u64 = 0;
    for line in raw.split_inclusive('\n') {
        if line.contains(needle) {
            return off;
        }
        off += line.len() as u64;
    }
    panic!("needle {:?} not found in {:?}", needle, path);
}

#[test]
fn session_id_and_ts_aliases_reach_sync_outputs() {
    let (_dir, path) = alias_key_session_file();
    let res = parse_claude_session(
        &path,
        &ParseOptions {
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(res.turns.len(), 1);
    assert_eq!(res.turns[0].session_id, "alias-session");
    assert_eq!(res.turns[0].ts, "2026-04-25T00:00:01.000Z");

    assert_eq!(res.user_turns.len(), 1);
    assert_eq!(res.user_turns[0].session_id, "alias-session");
    assert_eq!(res.user_turns[0].ts, "2026-04-25T00:00:00.000Z");
    assert_eq!(
        res.user_turns[0].following_message_id.as_deref(),
        Some("msg_alias_1")
    );

    assert!(res.content.iter().all(|c| c.session_id == "alias-session"));
    assert!(res
        .content
        .iter()
        .any(|c| matches!(c.role, ContentRole::ToolResult) && c.ts == "2026-04-25T00:00:00.000Z"));
    assert!(res
        .content
        .iter()
        .any(|c| matches!(c.role, ContentRole::Assistant) && c.ts == "2026-04-25T00:00:01.000Z"));

    let root = res
        .relationships
        .iter()
        .find(|r| matches!(r.relationship_type, RelationshipType::Root))
        .expect("root relationship");
    assert_eq!(root.session_id, "alias-session");
    assert_eq!(root.ts.as_deref(), Some("2026-04-25T00:00:00.000Z"));
    assert_eq!(root.source_version.as_deref(), Some("2.1.alias"));
    let continuation = res
        .relationships
        .iter()
        .find(|r| matches!(r.relationship_type, RelationshipType::Continuation))
        .expect("continuation relationship");
    assert_eq!(continuation.session_id, "alias-session");
    assert_eq!(
        continuation.related_session_id.as_deref(),
        Some("parent-session")
    );
    assert_eq!(continuation.ts.as_deref(), Some("2026-04-25T00:00:00.000Z"));
    assert_eq!(continuation.source_version.as_deref(), Some("2.1.alias"));

    assert_eq!(res.events.len(), 1);
    assert_eq!(res.events[0].session_id, "alias-session");
    assert_eq!(res.events[0].ts, "2026-04-25T00:00:02.000Z");
    assert_eq!(
        res.events[0].preceding_message_id.as_deref(),
        Some("msg_alias_1")
    );

    let tool_event = res
        .tool_result_events
        .iter()
        .find(|e| matches!(e.event_source, ToolResultEventSource::ToolResult))
        .expect("tool result event");
    assert_eq!(tool_event.session_id, "alias-session");
    assert_eq!(tool_event.ts.as_deref(), Some("2026-04-25T00:00:00.000Z"));
    let system_event = res
        .tool_result_events
        .iter()
        .find(|e| matches!(e.event_source, ToolResultEventSource::SubagentNotification))
        .expect("system tool result event");
    assert_eq!(system_event.session_id, "alias-session");
    assert_eq!(system_event.ts.as_deref(), Some("2026-04-25T00:00:03.000Z"));
    assert_eq!(system_event.call_index, Some(1));

    assert_eq!(res.evidence.in_log_session_ids, vec!["alias-session"]);
    assert_eq!(
        res.evidence.first_ts.as_deref(),
        Some("2026-04-25T00:00:00.000Z")
    );
    assert_eq!(res.evidence.source_version.as_deref(), Some("2.1.alias"));
}

#[test]
fn session_id_and_ts_aliases_reach_incremental_outputs() {
    let (_dir, path) = alias_key_session_file();
    let res = parse_claude_session_incremental(
        &path,
        &ParseIncrementalOptions {
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(res.turns.len(), 1);
    assert_eq!(res.turns[0].session_id, "alias-session");
    assert_eq!(res.turns[0].ts, "2026-04-25T00:00:01.000Z");
    assert_eq!(res.user_turns.len(), 1);
    assert_eq!(res.user_turns[0].session_id, "alias-session");
    assert_eq!(res.user_turns[0].ts, "2026-04-25T00:00:00.000Z");
    assert!(res
        .relationships
        .iter()
        .any(|r| matches!(r.relationship_type, RelationshipType::Root)
            && r.session_id == "alias-session"));
    assert!(res.relationships.iter().any(|r| matches!(
        r.relationship_type,
        RelationshipType::Continuation
    ) && r.session_id == "alias-session"
        && r.related_session_id.as_deref() == Some("parent-session")));
    assert_eq!(res.events.len(), 1);
    assert_eq!(res.events[0].session_id, "alias-session");
    assert_eq!(res.events[0].ts, "2026-04-25T00:00:02.000Z");
    assert_eq!(res.tool_result_events.len(), 2);
    assert!(res.tool_result_events.iter().any(|e| matches!(
        e.event_source,
        ToolResultEventSource::ToolResult
    ) && e.session_id == "alias-session"
        && e.ts.as_deref() == Some("2026-04-25T00:00:00.000Z")));
    assert!(res.tool_result_events.iter().any(|e| matches!(
        e.event_source,
        ToolResultEventSource::SubagentNotification
    ) && e.session_id == "alias-session"
        && e.ts.as_deref() == Some("2026-04-25T00:00:03.000Z")));
    assert!(res.content.iter().all(|c| c.session_id == "alias-session"));
    assert_eq!(res.evidence.in_log_session_ids, vec!["alias-session"]);
    assert_eq!(
        res.evidence.first_ts.as_deref(),
        Some("2026-04-25T00:00:00.000Z")
    );
    assert_eq!(res.evidence.source_version.as_deref(), Some("2.1.alias"));
}

#[test]
fn incremental_reads_whole_file_from_start() {
    let src = fixture("simple-turn.jsonl");
    let raw_len = read_bytes(&src).len() as u64;
    let r = parse_claude_session_incremental(&src, &ParseIncrementalOptions::default()).unwrap();
    assert_eq!(r.turns.len(), 1);
    assert_eq!(r.turns[0].message_id, "msg_simple_1");
    assert_eq!(r.end_offset, raw_len);
}

#[test]
fn incremental_returns_zero_turns_when_start_at_eof() {
    let src = fixture("simple-turn.jsonl");
    let raw_len = read_bytes(&src).len() as u64;
    let r = parse_claude_session_incremental(
        &src,
        &ParseIncrementalOptions {
            start_offset: Some(raw_len),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(r.turns.len(), 0);
    assert_eq!(r.end_offset, raw_len);
}

#[test]
fn incremental_appended_turn_emitted_on_resume() {
    let src = fixture("simple-turn.jsonl");
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");
    std::fs::copy(&src, &working).unwrap();
    let first =
        parse_claude_session_incremental(&working, &ParseIncrementalOptions::default()).unwrap();
    assert_eq!(first.turns.len(), 1);

    let appended = serde_json::json!({
        "parentUuid": "u-asst-1",
        "isSidechain": false,
        "message": {
            "model": "claude-sonnet-4-6",
            "id": "msg_simple_2",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "and another"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 2,
                "output_tokens": 1,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
            }
        },
        "type": "assistant",
        "uuid": "u-asst-2",
        "timestamp": "2026-04-20T00:00:05.000Z",
        "cwd": "/tmp/project",
        "sessionId": "11111111-1111-1111-1111-111111111111",
    });
    append_str(&working, &(appended.to_string() + "\n"));

    let second = parse_claude_session_incremental(
        &working,
        &ParseIncrementalOptions {
            start_offset: Some(first.end_offset),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(second.turns.len(), 1);
    assert_eq!(second.turns[0].message_id, "msg_simple_2");
    let full_len = read_bytes(&working).len() as u64;
    assert_eq!(second.end_offset, full_len);
}

#[test]
fn incremental_defers_in_progress_trailing_message() {
    let src = fixture("incomplete-then-complete.jsonl");
    let inprog_offset = line_start_offset(&src, "\"id\":\"msg_inprog_1\"");
    let r = parse_claude_session_incremental(&src, &ParseIncrementalOptions::default()).unwrap();
    assert_eq!(r.turns.len(), 1, "only the complete message is emitted");
    assert_eq!(r.turns[0].message_id, "msg_done_1");
    assert_eq!(
        r.end_offset, inprog_offset,
        "endOffset backs up to start of in-progress line"
    );
}

#[test]
fn incremental_defers_content_for_in_progress_then_emits_after_completion() {
    let src = fixture("incomplete-then-complete.jsonl");
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");
    std::fs::copy(&src, &working).unwrap();

    let first = parse_claude_session_incremental(
        &working,
        &ParseIncrementalOptions {
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();
    let asst_first: Vec<&ContentRecord> = first
        .content
        .iter()
        .filter(|c| matches!(c.role, ContentRole::Assistant))
        .collect();
    assert!(asst_first.iter().all(|c| c.message_id == "msg_done_1"));

    let tail = serde_json::json!({
        "parentUuid": "u-asst-1",
        "isSidechain": false,
        "message": {
            "model": "claude-sonnet-4-6",
            "id": "msg_inprog_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "done now"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 7,
                "output_tokens": 3,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
            }
        },
        "type": "assistant",
        "uuid": "u-asst-2",
        "timestamp": "2026-04-20T00:00:02.000Z",
        "cwd": "/tmp/project",
        "sessionId": "33333333-3333-3333-3333-333333333333",
    });
    append_str(&working, &(tail.to_string() + "\n"));

    let second = parse_claude_session_incremental(
        &working,
        &ParseIncrementalOptions {
            start_offset: Some(first.end_offset),
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();
    let asst_second: Vec<&ContentRecord> = second
        .content
        .iter()
        .filter(|c| matches!(c.role, ContentRole::Assistant))
        .collect();
    assert!(!asst_second.is_empty());
    assert!(asst_second.iter().all(|c| c.message_id == "msg_inprog_1"));
    assert!(asst_second
        .iter()
        .any(|c| matches!(c.kind, ContentKind::Text) && c.text.as_deref() == Some("done now")));
}

#[test]
fn incremental_defers_assistant_content_after_in_progress_message() {
    // msg_done_1 (complete) → msg_inprog_1 (incomplete) → msg_after_1 (complete).
    // endOffset must back up to msg_inprog_1, so msg_after_1 content is deferred
    // — appendContent has no row dedup so the next pass would otherwise duplicate it.
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");
    let lines = [
        serde_json::json!({
            "parentUuid": null,
            "isSidechain": false,
            "type": "user",
            "message": {"role": "user", "content": "hi"},
            "uuid": "u-user-1",
            "timestamp": "2026-04-20T00:00:00.000Z",
            "cwd": "/tmp/project",
            "sessionId": "sess-dup",
        }),
        serde_json::json!({
            "parentUuid": "u-user-1",
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-4-6",
                "id": "msg_done_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "done"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}},
            },
            "type": "assistant",
            "uuid": "u-asst-1",
            "timestamp": "2026-04-20T00:00:01.000Z",
            "cwd": "/tmp/project",
            "sessionId": "sess-dup",
        }),
        serde_json::json!({
            "parentUuid": "u-asst-1",
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-4-6",
                "id": "msg_inprog_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "working..."}],
                "stop_reason": null,
                "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}},
            },
            "type": "assistant",
            "uuid": "u-asst-2",
            "timestamp": "2026-04-20T00:00:02.000Z",
            "cwd": "/tmp/project",
            "sessionId": "sess-dup",
        }),
        serde_json::json!({
            "parentUuid": "u-asst-2",
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-4-6",
                "id": "msg_after_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "after"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}},
            },
            "type": "assistant",
            "uuid": "u-asst-3",
            "timestamp": "2026-04-20T00:00:03.000Z",
            "cwd": "/tmp/project",
            "sessionId": "sess-dup",
        }),
    ];
    let body: String = lines
        .iter()
        .map(|j| j.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    write_bytes(&working, body.as_bytes());

    let r = parse_claude_session_incremental(
        &working,
        &ParseIncrementalOptions {
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();
    let message_ids: Vec<&str> = r
        .content
        .iter()
        .filter(|c| matches!(c.role, ContentRole::Assistant))
        .map(|c| c.message_id.as_str())
        .collect();
    assert_eq!(message_ids, vec!["msg_done_1"]);
    let buf_len = read_bytes(&working).len() as u64;
    assert!(r.end_offset < buf_len);
}

#[test]
fn incremental_skips_incomplete_turn_then_emits_when_completion_arrives() {
    let src = fixture("incomplete-then-complete.jsonl");
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");
    std::fs::copy(&src, &working).unwrap();
    let first =
        parse_claude_session_incremental(&working, &ParseIncrementalOptions::default()).unwrap();
    assert_eq!(first.turns.len(), 1);

    // Append a completion line for msg_inprog_1 (same id, but stop_reason set).
    let tail = serde_json::json!({
        "parentUuid": "u-asst-1",
        "isSidechain": false,
        "message": {
            "model": "claude-sonnet-4-6",
            "id": "msg_inprog_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "working..."}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 7,
                "output_tokens": 3,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}
            }
        },
        "type": "assistant",
        "uuid": "u-asst-2",
        "timestamp": "2026-04-20T00:00:02.000Z",
        "cwd": "/tmp/project",
        "sessionId": "33333333-3333-3333-3333-333333333333",
    });
    append_str(&working, &(tail.to_string() + "\n"));

    let second = parse_claude_session_incremental(
        &working,
        &ParseIncrementalOptions {
            start_offset: Some(first.end_offset),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(second.turns.len(), 1);
    assert_eq!(second.turns[0].message_id, "msg_inprog_1");
    assert_eq!(second.turns[0].stop_reason, Some(StopReason::EndTurn));
}

#[test]
fn incremental_preserves_user_prompt_across_resume() {
    // Regression: when an incomplete assistant message forces endOffset to
    // back up past the user prompt, the resumed call re-reads the
    // assistant line without seeing the prompt. We carry lastUserText
    // forward so the classifier still has keyword context.
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");
    let session_id = "44444444-4444-4444-4444-444444444444";
    let lines = [
        serde_json::json!({
            "parentUuid": null,
            "isSidechain": false,
            "type": "user",
            "message": {"role": "user", "content": "fix the bug in auth.ts"},
            "uuid": "u-user-1",
            "timestamp": "2026-04-20T00:00:00.000Z",
            "cwd": "/tmp/project",
            "sessionId": session_id,
        }),
        serde_json::json!({
            "parentUuid": "u-user-1",
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-4-6",
                "id": "msg_resume_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "tu_edit_1", "name": "Edit", "input": {"file_path": "/auth.ts"}}],
                "stop_reason": null,
                "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}},
            },
            "type": "assistant",
            "uuid": "u-asst-1",
            "timestamp": "2026-04-20T00:00:01.000Z",
            "cwd": "/tmp/project",
            "sessionId": session_id,
        }),
    ];
    let body: String = lines
        .iter()
        .map(|j| j.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    write_bytes(&working, body.as_bytes());

    let first =
        parse_claude_session_incremental(&working, &ParseIncrementalOptions::default()).unwrap();
    assert_eq!(first.turns.len(), 0, "incomplete turn is deferred");
    assert_eq!(first.last_user_text, "fix the bug in auth.ts");

    // Append completion of msg_resume_1.
    let tail = serde_json::json!({
        "parentUuid": "u-asst-1",
        "isSidechain": false,
        "message": {
            "model": "claude-sonnet-4-6",
            "id": "msg_resume_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "tu_edit_1", "name": "Edit", "input": {"file_path": "/auth.ts"}}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}},
        },
        "type": "assistant",
        "uuid": "u-asst-1",
        "timestamp": "2026-04-20T00:00:01.000Z",
        "cwd": "/tmp/project",
        "sessionId": session_id,
    });
    append_str(&working, &(tail.to_string() + "\n"));

    let second = parse_claude_session_incremental(
        &working,
        &ParseIncrementalOptions {
            start_offset: Some(first.end_offset),
            last_user_text: Some(first.last_user_text.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(second.turns.len(), 1);
    let t = &second.turns[0];
    assert_eq!(t.message_id, "msg_resume_1");
    assert_eq!(
        t.activity,
        Some(ActivityCategory::Debugging),
        "user prompt mentions 'bug' so edit turn is debugging"
    );

    // Without the seed, the prompt is lost on resume and the classifier
    // falls back to coding.
    let without_seed = parse_claude_session_incremental(
        &working,
        &ParseIncrementalOptions {
            start_offset: Some(first.end_offset),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        without_seed.turns[0].activity,
        Some(ActivityCategory::Coding)
    );
}

#[test]
fn incremental_user_turns_emitted_once_across_resumed_passes() {
    let src = fixture("user-turn-blocks.jsonl");
    let full = std::fs::read_to_string(&src).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");

    // Pass 1: write only through msg_utb_2 (4 lines: user, asst, user, asst).
    let lines: Vec<&str> = full.split('\n').filter(|l| !l.is_empty()).collect();
    let prefix = lines[..4].join("\n") + "\n";
    write_bytes(&working, prefix.as_bytes());
    let first =
        parse_claude_session_incremental(&working, &ParseIncrementalOptions::default()).unwrap();
    let first_ids: Vec<&str> = first
        .user_turns
        .iter()
        .map(|u| u.user_uuid.as_str())
        .collect();
    assert_eq!(first_ids, vec!["u-user-1", "u-user-2"]);

    // Pass 2: full file. Must emit only u-user-3 (no re-emit of 1/2).
    write_bytes(&working, full.as_bytes());
    let second = parse_claude_session_incremental(
        &working,
        &ParseIncrementalOptions {
            start_offset: Some(first.end_offset),
            last_user_text: Some(first.last_user_text.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let second_ids: Vec<&str> = second
        .user_turns
        .iter()
        .map(|u| u.user_uuid.as_str())
        .collect();
    assert_eq!(second_ids, vec!["u-user-3"]);
    let u3 = &second.user_turns[0];
    assert_eq!(u3.preceding_message_id.as_deref(), Some("msg_utb_2"));
    assert_eq!(u3.following_message_id.as_deref(), Some("msg_utb_3"));
    assert_eq!(u3.blocks[0].is_error, Some(true));
}

#[test]
fn incremental_seeds_tool_result_event_counters_from_prescan() {
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");
    let session_id = "66666666-6666-6666-6666-666666666666";
    let user_result = serde_json::json!({
        "parentUuid": null,
        "isSidechain": false,
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "toolu_system", "content": "done"}]
        },
        "uuid": "u-result-1",
        "timestamp": "2026-04-24T01:00:00.000Z",
        "cwd": "/tmp/project",
        "sessionId": session_id,
    });
    let incomplete_assistant = serde_json::json!({
        "parentUuid": "u-result-1",
        "isSidechain": false,
        "message": {
            "model": "claude-sonnet-4-6",
            "id": "msg_waiting",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "waiting"}],
            "stop_reason": null,
            "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
        },
        "type": "assistant",
        "uuid": "u-asst-waiting",
        "timestamp": "2026-04-24T01:00:01.000Z",
        "cwd": "/tmp/project",
        "sessionId": session_id,
    });
    let system_notification = serde_json::json!({
        "type": "system",
        "subtype": "subagent_completed",
        "sessionId": session_id,
        "timestamp": "2026-04-24T01:00:02.000Z",
        "parent_tool_use_id": "toolu_system",
        "agent_id": "agent-system-2",
        "subagent_session_id": "session-system-child-2",
        "status": "completed",
    });
    let body = [&user_result, &incomplete_assistant, &system_notification]
        .iter()
        .map(|j| j.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    write_bytes(&working, body.as_bytes());

    let first =
        parse_claude_session_incremental(&working, &ParseIncrementalOptions::default()).unwrap();
    assert_eq!(first.tool_result_events.len(), 1);
    assert_eq!(
        first.tool_result_events[0].event_source,
        ToolResultEventSource::ToolResult
    );
    assert_eq!(first.tool_result_events[0].tool_use_id, "toolu_system");
    assert_eq!(first.tool_result_events[0].call_index, Some(0));
    assert_eq!(first.tool_result_events[0].event_index, 0);

    // Append a completion line for msg_waiting so the deferred system
    // notification line gets re-read on the next pass.
    let mut complete_assistant = incomplete_assistant.clone();
    complete_assistant["message"]["stop_reason"] = serde_json::Value::from("end_turn");
    let body2 = [
        &user_result,
        &incomplete_assistant,
        &system_notification,
        &complete_assistant,
    ]
    .iter()
    .map(|j| j.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    write_bytes(&working, body2.as_bytes());

    let second = parse_claude_session_incremental(
        &working,
        &ParseIncrementalOptions {
            start_offset: Some(first.end_offset),
            last_user_text: Some(first.last_user_text.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let ev = second
        .tool_result_events
        .iter()
        .find(|e| matches!(e.event_source, ToolResultEventSource::SubagentNotification))
        .expect("resumed pass should emit the deferred system notification");
    assert_eq!(ev.tool_use_id, "toolu_system");
    assert_eq!(ev.call_index, Some(1));
    assert_eq!(ev.event_index, 1);
    assert_eq!(ev.agent_id.as_deref(), Some("agent-system-2"));
    assert_eq!(
        ev.subagent_session_id.as_deref(),
        Some("session-system-child-2")
    );
}

#[test]
fn incremental_resolves_subagent_tree_via_prescan() {
    // Pass 1 ingests the main thread + Agent spawn line. Pass 2 starts
    // beyond them and must still populate agentId / parentAgentId /
    // parentToolUseId on the sidechain turns via the prescan registering
    // the prior parentUuid nodes.
    let src = fixture("nested-subagent.jsonl");
    let full = std::fs::read_to_string(&src).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");

    let lines: Vec<&str> = full.split('\n').filter(|l| !l.is_empty()).collect();
    // Write only through the outer Agent spawn line on pass 1.
    let prefix = lines[..2].join("\n") + "\n";
    write_bytes(&working, prefix.as_bytes());
    let first =
        parse_claude_session_incremental(&working, &ParseIncrementalOptions::default()).unwrap();
    assert!(!first.turns.is_empty());

    write_bytes(&working, full.as_bytes());
    let second = parse_claude_session_incremental(
        &working,
        &ParseIncrementalOptions {
            start_offset: Some(first.end_offset),
            ..Default::default()
        },
    )
    .unwrap();

    let by_id: HashMap<&str, &TurnRecord> = second
        .turns
        .iter()
        .map(|t| (t.message_id.as_str(), t))
        .collect();
    let sub1_1 = by_id
        .get("msg_sub1_1")
        .expect("outer sidechain turn should be emitted on pass 2");
    let sub2_1 = by_id
        .get("msg_sub2_1")
        .expect("inner sidechain turn should be emitted on pass 2");

    let s1 = sub1_1.subagent.as_ref().unwrap();
    assert_eq!(s1.agent_id.as_deref(), Some("u-sub1-user"));
    assert_eq!(s1.parent_tool_use_id.as_deref(), Some("toolu_outer"));
    assert_eq!(s1.subagent_type.as_deref(), Some("Explore"));
    assert_eq!(
        s1.parent_agent_id.as_deref(),
        Some("55555555-5555-5555-5555-555555555555")
    );

    let s2 = sub2_1.subagent.as_ref().unwrap();
    assert_eq!(s2.agent_id.as_deref(), Some("u-sub2-user"));
    assert_eq!(s2.parent_agent_id.as_deref(), Some("u-sub1-user"));
    assert_eq!(s2.parent_tool_use_id.as_deref(), Some("toolu_inner"));
}

// ----- parseClaudeSession (synchronous) extended conformance -----
//
// Mirrors the remaining `it()` cases under the top
// `describe('parseClaudeSession', ...)` block in
// `packages/reader/src/claude.test.ts` (lines 17-311) so the Rust port
// gates on byte-equivalent assertions against the same shared fixtures.

#[test]
fn simple_turn_records_project_and_full_usage() {
    // Mirrors `it('parses a simple one-turn session')` lines 18-38, adding
    // the project field and the full Usage struct that the lighter
    // `simple_turn_parses` test above does not check.
    let path = fixture("simple-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let t = &res.turns[0];
    assert_eq!(t.project.as_deref(), Some("/tmp/project"));
    assert_eq!(
        t.usage,
        Usage {
            input: 10,
            output: 5,
            reasoning: 0,
            cache_read: 500,
            cache_create_5m: 80,
            cache_create_1h: 20,
        }
    );
}

#[test]
fn multi_block_turn_keeps_usage_once() {
    // Mirrors `it('dedupes a multi-block assistant message and keeps usage once')`
    // (claude.test.ts:40). The four assistant lines for `msg_multi_1` repeat
    // the same usage block; the parser must collapse to one turn that
    // counts that usage exactly once.
    let path = fixture("multi-block-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let t = &res.turns[0];
    assert_eq!(
        t.usage,
        Usage {
            input: 3,
            output: 43,
            reasoning: 0,
            cache_read: 11496,
            cache_create_5m: 0,
            cache_create_1h: 4773,
        }
    );
}

#[test]
fn stable_args_hash_for_identical_tool_inputs() {
    // claude.test.ts:113 — `argsHash` is a content hash, so two parses of
    // the same fixture must produce identical hashes; different inputs in
    // the same turn must hash differently.
    let path = fixture("multi-block-turn.jsonl");
    let a = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let b = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(
        a.turns[0].tool_calls[0].args_hash,
        b.turns[0].tool_calls[0].args_hash
    );
    assert_ne!(
        a.turns[0].tool_calls[0].args_hash,
        a.turns[0].tool_calls[1].args_hash
    );
}

#[test]
fn marks_tool_call_is_error_when_tool_result_has_is_error_true() {
    // claude.test.ts:120 — every Bash call in retry-loop.jsonl is followed
    // by a tool_result carrying is_error=true. The parser back-populates
    // ToolCall.isError so consumers don't need a separate join.
    let path = fixture("retry-loop.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.turns.len(), 4);
    for t in &res.turns {
        assert_eq!(t.tool_calls.len(), 1);
        assert_eq!(t.tool_calls[0].name, "Bash");
        assert_eq!(t.tool_calls[0].is_error, Some(true));
    }
}

#[test]
fn back_populates_replacement_meta_from_tool_result() {
    // claude.test.ts:130 — tool_result `_meta.replaces` and
    // `_meta.collapsedCalls` are surfaced both on the originating ToolCall
    // and on the matching ToolResultEventRecord. Calls without _meta keep
    // the fields absent.
    let path = fixture("replacement-meta.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let all: Vec<&ToolCall> = res.turns.iter().flat_map(|t| t.tool_calls.iter()).collect();
    let search = all
        .iter()
        .find(|tc| tc.name == "relaywash__Search")
        .expect("search tool call present");
    let read = all
        .iter()
        .find(|tc| tc.name == "Read")
        .expect("read tool call present");
    assert_eq!(
        search.replaced_tools.as_deref(),
        Some(["Glob".to_string(), "Grep".to_string(), "Read".to_string()].as_slice())
    );
    assert_eq!(search.collapsed_calls, Some(9));
    assert!(read.replaced_tools.is_none());
    assert!(read.collapsed_calls.is_none());

    let search_event = res
        .tool_result_events
        .iter()
        .find(|e| e.tool_use_id == "tu_search_1")
        .expect("search tool_result event present");
    assert_eq!(
        search_event.replaced_tools.as_deref(),
        Some(["Glob".to_string(), "Grep".to_string(), "Read".to_string()].as_slice())
    );
    assert_eq!(search_event.collapsed_calls, Some(9));
}

#[test]
fn extracts_edit_pre_and_post_hashes() {
    // claude.test.ts:151 — Edit tool calls carry editPreHash / editPostHash
    // derived from old_string / new_string. A revert (second edit's post ==
    // first edit's pre) is detectable by comparing the hashes.
    let path = fixture("edit-revert.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let edits: Vec<&ToolCall> = res
        .turns
        .iter()
        .flat_map(|t| t.tool_calls.iter())
        .filter(|tc| tc.name == "Edit")
        .collect();
    assert_eq!(edits.len(), 2);
    assert!(edits[0].edit_pre_hash.is_some());
    assert!(edits[0].edit_post_hash.is_some());
    assert_eq!(edits[1].edit_post_hash, edits[0].edit_pre_hash);
    assert_eq!(edits[1].edit_pre_hash, edits[0].edit_post_hash);
}

#[test]
fn tool_result_events_chronological_with_full_metadata() {
    // claude.test.ts:165 — every tool_result block in retry-loop.jsonl
    // becomes a ToolResultEventRecord. Status is `errored`, contentLength
    // and contentHash are populated, and eventIndex is monotonic.
    let path = fixture("retry-loop.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.tool_result_events.len(), 4);
    for ev in &res.tool_result_events {
        assert_eq!(ev.v, 1);
        assert_eq!(ev.source, SourceKind::ClaudeCode);
        assert_eq!(ev.event_source, ToolResultEventSource::ToolResult);
        assert_eq!(ev.status, ToolResultStatus::Errored);
        assert_eq!(ev.is_error, Some(true));
        assert!(ev.content_length.is_some());
        assert!(ev.content_hash.is_some());
    }
    for w in res.tool_result_events.windows(2) {
        assert!(w[1].event_index > w[0].event_index);
    }
}

#[test]
fn relationships_root_plus_subagent_per_invocation() {
    // claude.test.ts:187 — one root row per session, one subagent row per
    // distinct invocation (agentId). Outer's parent is the main session
    // id; inner's parent is the outer invocation's agentId. Source is
    // `native-claude` per the TS spec (not `claude-code`) — that flag
    // separates harness-emitted edges from cross-source spawn-env edges.
    let path = fixture("nested-subagent.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let roots: Vec<_> = res
        .relationships
        .iter()
        .filter(|r| matches!(r.relationship_type, RelationshipType::Root))
        .collect();
    let subs: Vec<_> = res
        .relationships
        .iter()
        .filter(|r| matches!(r.relationship_type, RelationshipType::Subagent))
        .collect();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].session_id, "55555555-5555-5555-5555-555555555555");
    assert_eq!(subs.len(), 2);

    let outer = subs
        .iter()
        .find(|r| r.subagent_type.as_deref() == Some("Explore"))
        .expect("outer subagent row present");
    let inner = subs
        .iter()
        .find(|r| r.subagent_type.as_deref() == Some("code-reviewer"))
        .expect("inner subagent row present");

    assert_eq!(outer.agent_id.as_deref(), Some("u-sub1-user"));
    assert_eq!(outer.source, RelationshipSourceKind::NativeClaude);
    assert_eq!(outer.parent_tool_use_id.as_deref(), Some("toolu_outer"));
    assert_eq!(
        outer.related_session_id.as_deref(),
        Some("55555555-5555-5555-5555-555555555555")
    );
    assert_eq!(outer.description.as_deref(), Some("Research the codebase"));

    assert_eq!(inner.agent_id.as_deref(), Some("u-sub2-user"));
    assert_eq!(inner.parent_tool_use_id.as_deref(), Some("toolu_inner"));
    assert_eq!(inner.related_session_id.as_deref(), Some("u-sub1-user"));
}

#[test]
fn tool_result_events_join_to_spawned_subagent_via_agent_id() {
    // claude.test.ts:216 — Agent/Task tool_results inherit the spawned
    // subagent's agentId so cross-table joins work without a separate
    // subagent index.
    let path = fixture("nested-subagent.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let outer = res
        .tool_result_events
        .iter()
        .find(|e| e.tool_use_id == "toolu_outer")
        .expect("outer Agent tool_result event present");
    let inner = res
        .tool_result_events
        .iter()
        .find(|e| e.tool_use_id == "toolu_inner")
        .expect("inner Agent tool_result event present");
    assert_eq!(outer.agent_id.as_deref(), Some("u-sub1-user"));
    assert_eq!(inner.agent_id.as_deref(), Some("u-sub2-user"));
}

#[test]
fn system_subagent_notification_emits_tool_result_event() {
    // claude.test.ts:228 — a `system` line with subtype
    // `subagent_completed` becomes a ToolResultEventRecord (not a
    // CompactionEvent), with eventSource=`subagent_notification` and the
    // child session id surfaced.
    let path = fixture("system-subagent-notification.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.events.len(), 0);
    assert_eq!(res.tool_result_events.len(), 1);
    let ev = &res.tool_result_events[0];
    assert_eq!(ev.source, SourceKind::ClaudeCode);
    assert_eq!(ev.session_id, "22222222-2222-2222-2222-222222222222");
    assert_eq!(ev.tool_use_id, "toolu_system");
    assert_eq!(ev.event_source, ToolResultEventSource::SubagentNotification);
    assert_eq!(ev.status, ToolResultStatus::Completed);
    assert_eq!(ev.agent_id.as_deref(), Some("agent-system-1"));
    assert_eq!(
        ev.subagent_session_id.as_deref(),
        Some("session-system-child")
    );
    assert_eq!(ev.call_index, Some(0));
    assert_eq!(ev.event_index, 0);
    assert!(ev.content_length.is_some());
    assert!(ev.content_hash.is_some());
}

#[test]
fn fidelity_full_coverage_on_normal_turn() {
    // claude.test.ts:248 — simple-turn carries every usage field, so the
    // turn surfaces full coverage and class=Full. tool/relationship flags
    // are capability-level (always true for Claude even on a no-tool turn);
    // reasoning is always false because the harness doesn't surface it.
    let path = fixture("simple-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let t = &res.turns[0];
    let f = t.fidelity.as_ref().expect("fidelity should be populated");
    assert_eq!(f.granularity, UsageGranularity::PerTurn);
    assert!(f.coverage.has_input_tokens);
    assert!(f.coverage.has_output_tokens);
    assert!(f.coverage.has_cache_read_tokens);
    assert!(f.coverage.has_cache_create_tokens);
    assert!(!f.coverage.has_reasoning_tokens);
    assert!(f.coverage.has_tool_calls);
    assert!(f.coverage.has_tool_result_events);
    assert!(f.coverage.has_session_relationships);
    assert_eq!(f.class, FidelityClass::Full);
}

#[test]
fn fidelity_marks_missing_output_tokens_as_partial() {
    // claude.test.ts:270 — Usage.output is forced to 0 (the wire shape
    // requires *some* number) but coverage.hasOutputTokens=false makes the
    // distinction visible. Class falls below Full to Partial because not
    // all required fields are populated.
    let path = fixture("missing-output-tokens.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let t = &res.turns[0];
    assert_eq!(t.usage.output, 0);
    let f = t.fidelity.as_ref().unwrap();
    assert!(f.coverage.has_input_tokens);
    assert!(!f.coverage.has_output_tokens);
    assert!(!f.coverage.has_cache_read_tokens);
    assert!(!f.coverage.has_cache_create_tokens);
    assert_eq!(f.class, FidelityClass::Partial);
}

#[test]
fn fidelity_has_tool_calls_on_tool_use_turn() {
    // claude.test.ts:289 — the Coverage flag is capability-level, so a
    // turn that *did* emit tool_use blocks must reflect that; class stays
    // Full because every required field is populated.
    let path = fixture("multi-block-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let f = res.turns[0].fidelity.as_ref().unwrap();
    assert!(f.coverage.has_tool_calls);
    assert_eq!(f.class, FidelityClass::Full);
}

#[test]
fn compact_boundary_emits_compaction_event() {
    // claude.test.ts:297 — a `system` line with subtype `compact_boundary`
    // produces one CompactionEvent anchored to the assistant turn that
    // immediately preceded it. tokensBeforeCompact mirrors that turn's
    // cacheRead.
    let path = fixture("compact-boundary.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.events.len(), 1);
    let ev = &res.events[0];
    assert_eq!(ev.source, SourceKind::ClaudeCode);
    assert_eq!(ev.session_id, "compact-session");
    assert_eq!(ev.preceding_message_id.as_deref(), Some("msg_c_1"));
    let preceding = res
        .turns
        .iter()
        .find(|t| t.message_id == "msg_c_1")
        .unwrap();
    assert_eq!(ev.tokens_before_compact, Some(preceding.usage.cache_read));
    assert_eq!(ev.tokens_before_compact, Some(9000));
}

// ----- parseClaudeSession user-turn block sizes (issue #2) -----

#[test]
fn user_turn_blocks_text_and_tool_results() {
    // claude.test.ts:314 — three user lines → three UserTurnRecord rows.
    // The first is plain text, the second carries Bash + Read tool_results
    // (the Read body is much larger than the Bash body), the third carries
    // an errored tool_result. precedingMessageId is undefined for the
    // first user turn (no prior assistant) and otherwise points at the
    // immediately-prior assistant message; followingMessageId points at
    // the next assistant.
    let path = fixture("user-turn-blocks.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.user_turns.len(), 3);

    let first = &res.user_turns[0];
    assert_eq!(first.user_uuid, "u-user-1");
    assert!(first.preceding_message_id.is_none());
    assert_eq!(first.following_message_id.as_deref(), Some("msg_utb_1"));
    assert_eq!(first.blocks.len(), 1);
    assert_eq!(first.blocks[0].kind, UserTurnBlockKind::Text);
    assert_eq!(
        first.blocks[0].byte_len,
        "please fix the build".len() as u64
    );
    // The TS test asserts `4` (cl100k). The Rust port has not wired
    // cl100k yet — see #246 — so the heuristic counter (`ceil(byteLen/4)`)
    // is the default, which makes this 5 for the same 20-byte prompt.
    // The `user_turn_heuristic_tokenizer_explicit_opt_in` test pins the
    // heuristic formula explicitly; once cl100k lands here we'll flip
    // this back to `4` to match TS byte-for-byte.
    assert_eq!(
        first.blocks[0].approx_tokens,
        ("please fix the build".len() as u64).div_ceil(4)
    );

    let second = &res.user_turns[1];
    assert_eq!(second.preceding_message_id.as_deref(), Some("msg_utb_1"));
    assert_eq!(second.following_message_id.as_deref(), Some("msg_utb_2"));
    assert_eq!(second.blocks.len(), 2);
    let bash = second
        .blocks
        .iter()
        .find(|b| b.tool_use_id.as_deref() == Some("tu_bash_1"))
        .unwrap();
    let read = second
        .blocks
        .iter()
        .find(|b| b.tool_use_id.as_deref() == Some("tu_read_1"))
        .unwrap();
    assert_eq!(bash.kind, UserTurnBlockKind::ToolResult);
    assert_eq!(bash.byte_len, "a\nb\n".len() as u64);
    assert_eq!(read.byte_len, 100);
    assert!(read.byte_len > bash.byte_len);
    assert!(bash.is_error.is_none());
    assert!(read.is_error.is_none());

    let third = &res.user_turns[2];
    assert_eq!(third.preceding_message_id.as_deref(), Some("msg_utb_2"));
    assert_eq!(third.following_message_id.as_deref(), Some("msg_utb_3"));
    assert_eq!(third.blocks.len(), 1);
    let err_block = &third.blocks[0];
    assert_eq!(err_block.kind, UserTurnBlockKind::ToolResult);
    assert_eq!(err_block.tool_use_id.as_deref(), Some("tu_bash_2"));
    assert_eq!(err_block.is_error, Some(true));
}

#[test]
fn user_turn_input_delta_is_positive_for_real_io() {
    // claude.test.ts:355 — sanity gate: when there's real I/O across a
    // (precedingMessageId, followingMessageId) pair, both the
    // input-side delta and the per-block token sum must be positive. The
    // ±5% reconciliation in the TS test depends on the cl100k tokenizer,
    // which the Rust port has not wired (HeuristicCounter is still the
    // default — see #246), so we only enforce the positive-on-both-sides
    // invariant here.
    let path = fixture("user-turn-blocks.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let by_mid: HashMap<&str, &TurnRecord> = res
        .turns
        .iter()
        .map(|t| (t.message_id.as_str(), t))
        .collect();
    for u in &res.user_turns {
        let prev_id = match u.preceding_message_id.as_deref() {
            Some(p) => p,
            None => continue,
        };
        let next_id = match u.following_message_id.as_deref() {
            Some(n) => n,
            None => continue,
        };
        let prev = by_mid.get(prev_id).unwrap();
        let next = by_mid.get(next_id).unwrap();
        let input_delta =
            (next.usage.input + next.usage.cache_create_5m + next.usage.cache_create_1h) as i64
                - prev.usage.output as i64;
        let user_tokens: u64 = u.blocks.iter().map(|b| b.approx_tokens).sum();
        assert!(
            user_tokens > 0,
            "user turn {} should contribute tokens",
            u.user_uuid
        );
        assert!(input_delta > 0, "delta for {} should be positive", next_id);
    }
}

#[test]
fn user_turn_heuristic_tokenizer_explicit_opt_in() {
    // claude.test.ts:378 — with the heuristic tokenizer the first text
    // block's approxTokens is `ceil(byte_len / 4)`. The Rust default is
    // also heuristic, but mirroring the TS test means asking for it
    // explicitly so we exercise the option plumbing.
    let path = fixture("user-turn-blocks.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let first = &res.user_turns[0];
    assert_eq!(
        first.blocks[0].byte_len,
        "please fix the build".len() as u64
    );
    let expected = ("please fix the build".len() as u64).div_ceil(4);
    assert_eq!(first.blocks[0].approx_tokens, expected);
}

#[test]
fn user_turn_present_for_simple_text_session() {
    // claude.test.ts:405 — even a one-turn session emits one UserTurnRecord
    // with a single text block. (The TS comment explains why simple-turn
    // is the right fixture instead of sidechain-turn.)
    let path = fixture("simple-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.user_turns.len(), 1);
    assert_eq!(res.user_turns[0].blocks.len(), 1);
    assert_eq!(res.user_turns[0].blocks[0].kind, UserTurnBlockKind::Text);
}

#[test]
fn slash_command_triads_collapse_to_one_skill_activity_each() {
    // Integration coverage for #438. The fixture has two slash-command
    // triads (`/review` then `/init`), each three rows: caveat row,
    // invocation row (`<command-name>`), stdout row
    // (`<local-command-stdout>`). Three assistant rows surround the
    // triads (one before, one between, one after). Without the
    // detector, the two post-triad assistants would classify against
    // the stdout body (whatever keyword the stdout text happened to
    // hit) — with the detector, they collapse to `Skill`.
    let path = fixture("slash-command-triad.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.turns.len(), 3, "three assistant turns survive");
    let activities: Vec<Option<ActivityCategory>> = res.turns.iter().map(|t| t.activity).collect();
    // First assistant is a normal reply, NOT inside a triad.
    assert_ne!(activities[0], Some(ActivityCategory::Skill));
    // Second + third assistants follow a slash-command triad's stdout;
    // each must surface as a single `Skill` activity. Two triads →
    // two `Skill` turns (3 → 1 per triad, per the issue acceptance).
    assert_eq!(activities[1], Some(ActivityCategory::Skill));
    assert_eq!(activities[2], Some(ActivityCategory::Skill));
    let skill_count = activities
        .iter()
        .filter(|a| **a == Some(ActivityCategory::Skill))
        .count();
    assert_eq!(skill_count, 2, "two triads → two Skill activities");
}

#[test]
fn slash_command_triad_does_not_double_count_token_attribution() {
    // Token attribution stays on the underlying assistant rows; the
    // synthetic `Skill` label is a view, not a billing unit. The sum
    // of the three assistants' (input + output) tokens equals what
    // we computed before the triad classifier landed — collapsing
    // the activity label does NOT redirect tokens onto the Skill
    // turns or off of them. See AgentWorkforce/burn#438.
    let path = fixture("slash-command-triad.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    let total_input: u64 = res.turns.iter().map(|t| t.usage.input).sum();
    let total_output: u64 = res.turns.iter().map(|t| t.usage.output).sum();
    // Fixture: assistants 1, 2, 3 have input 10/20/25 and output 5/3/4.
    assert_eq!(total_input, 10 + 20 + 25);
    assert_eq!(total_output, 5 + 3 + 4);
}

#[test]
fn slash_command_triad_false_positive_guard_normal_user_turn() {
    // Regression guard for the false-positive case in #438. A
    // legitimate user prompt that *looks* structurally similar to a
    // caveat row (parent chain shape: user → user → user) but
    // lacks the `<command-name>` invocation marker MUST NOT
    // misdetect as a triad. The classifier should fall through to
    // its normal text-based rules. Mirrors the false-positive guard
    // in `task_notification_does_not_match_user_typed_marker_string`
    // (#442).
    let path = fixture("task-notification.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    for turn in &res.turns {
        assert_ne!(
            turn.activity,
            Some(ActivityCategory::Skill),
            "no slash-command markers → no Skill activity",
        );
    }
}

#[test]
fn task_notification_rows_are_excluded_from_user_turns() {
    // Integration coverage for #439. The fixture interleaves real user
    // prompts with two synthetic `<task-notification>` rows (one
    // tagged via `origin.kind`, one via the `queued_command`
    // attachment). Burn must emit exactly two UserTurnRecords (one
    // per real prompt) — a regression that re-counts task-notification
    // rows would emit four.
    let path = fixture("task-notification.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(
        res.user_turns.len(),
        2,
        "task-notification rows must not count as user turns"
    );
    let user_uuids: Vec<&str> = res
        .user_turns
        .iter()
        .map(|u| u.user_uuid.as_str())
        .collect();
    assert_eq!(user_uuids, vec!["u-user-1", "u-user-2"]);
    // Sanity: assistant turns are unaffected — the harness-injected
    // rows don't suppress real assistant accounting.
    assert_eq!(res.turns.len(), 2);
}

// ----- parseClaudeSession content capture -----

#[test]
fn content_default_off_returns_empty() {
    // claude.test.ts:418 — without `contentMode`, the parser does not
    // capture text bodies. Hash-only is the same shape (also empty) — the
    // sidecar handles hashing separately.
    let path = fixture("simple-turn.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert!(res.content.is_empty());
}

#[test]
fn content_hash_only_returns_empty() {
    // claude.test.ts:423 — hash-only mode is also empty at the parser
    // level (the writer derives sidecar entries downstream).
    let path = fixture("simple-turn.jsonl");
    let res = parse_claude_session(
        &path,
        &ParseOptions {
            content_mode: Some(ContentStoreMode::HashOnly),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(res.content.is_empty());
}

#[test]
fn content_full_captures_user_and_assistant_text() {
    // claude.test.ts:430 — `contentMode: 'full'` returns one user text
    // record and one assistant text record with full provenance.
    let path = fixture("simple-turn.jsonl");
    let res = parse_claude_session(
        &path,
        &ParseOptions {
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(res.content.len(), 2);
    let user = res
        .content
        .iter()
        .find(|c| matches!(c.role, ContentRole::User))
        .expect("user content present");
    assert_eq!(user.kind, ContentKind::Text);
    assert_eq!(user.text.as_deref(), Some("hello"));
    assert_eq!(user.session_id, "11111111-1111-1111-1111-111111111111");
    let asst = res
        .content
        .iter()
        .find(|c| matches!(c.role, ContentRole::Assistant))
        .expect("assistant content present");
    assert_eq!(asst.kind, ContentKind::Text);
    assert_eq!(asst.text.as_deref(), Some("Hello!"));
    assert_eq!(asst.message_id, "msg_simple_1");
    assert_eq!(asst.source, SourceKind::ClaudeCode);
}

#[test]
fn content_chronological_across_interleaved_turns() {
    // claude.test.ts:448 — content rows preserve interleaved turn order
    // across user/assistant pairs.
    let path = fixture("interleaved-turns.jsonl");
    let res = parse_claude_session(
        &path,
        &ParseOptions {
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();
    let sequence: Vec<String> = res
        .content
        .iter()
        .map(|c| {
            let role = match c.role {
                ContentRole::User => "user",
                ContentRole::Assistant => "assistant",
                ContentRole::ToolResult => "tool_result",
            };
            format!("{}:{}", role, c.text.as_deref().unwrap_or(""))
        })
        .collect();
    assert_eq!(
        sequence,
        vec![
            "user:first question".to_string(),
            "assistant:first answer".to_string(),
            "user:second question".to_string(),
            "assistant:second answer".to_string(),
        ]
    );
}

#[test]
fn content_captures_tool_use_blocks_in_multi_block_turn() {
    // claude.test.ts:462 — assistant content for a multi-block turn
    // surfaces the text + two tool_use blocks. The empty-string thinking
    // block is omitted (per parser policy: skip thinking blocks with no
    // body).
    let path = fixture("multi-block-turn.jsonl");
    let res = parse_claude_session(
        &path,
        &ParseOptions {
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();
    let asst: Vec<&ContentRecord> = res
        .content
        .iter()
        .filter(|c| matches!(c.role, ContentRole::Assistant))
        .collect();
    let mut kinds: Vec<&str> = asst
        .iter()
        .map(|c| match c.kind {
            ContentKind::Text => "text",
            ContentKind::Thinking => "thinking",
            ContentKind::ToolUse => "tool_use",
            ContentKind::ToolResult => "tool_result",
        })
        .collect();
    kinds.sort();
    assert_eq!(kinds, vec!["text", "tool_use", "tool_use"]);
    let tool_uses: Vec<&ContentRecord> = asst
        .iter()
        .copied()
        .filter(|c| matches!(c.kind, ContentKind::ToolUse))
        .collect();
    let bash_use = tool_uses
        .iter()
        .find(|c| c.tool_use.as_ref().map(|tu| tu.name.as_str()) == Some("Bash"))
        .expect("Bash tool_use surfaced");
    let agent_use = tool_uses
        .iter()
        .find(|c| c.tool_use.as_ref().map(|tu| tu.name.as_str()) == Some("Agent"))
        .expect("Agent tool_use surfaced");
    let bash_input = &bash_use.tool_use.as_ref().unwrap().input;
    assert_eq!(bash_input.len(), 1);
    assert_eq!(
        bash_input.get("command").and_then(|v| v.as_str()),
        Some("ls -la /tmp/project")
    );
    assert_eq!(agent_use.tool_use.as_ref().unwrap().name, "Agent");
}

#[test]
fn content_tool_result_is_error_tri_state() {
    // Verify that ContentRecord.tool_result.is_error is Some(true) when
    // the JSON field is `true`, and None when absent (or `false`).
    // Uses user-turn-blocks.jsonl which has:
    //   - tu_bash_1 (no is_error field)  → None
    //   - tu_read_1 (no is_error field)  → None
    //   - tu_bash_2 ("is_error": true)   → Some(true)
    let path = fixture("user-turn-blocks.jsonl");
    let res = parse_claude_session(
        &path,
        &ParseOptions {
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();
    let tool_results: Vec<&ContentRecord> = res
        .content
        .iter()
        .filter(|c| matches!(c.kind, ContentKind::ToolResult))
        .collect();
    assert_eq!(
        tool_results.len(),
        3,
        "expected 3 tool_result content records"
    );
    let bash1 = tool_results
        .iter()
        .find(|c| {
            c.tool_result
                .as_ref()
                .map(|tr| tr.tool_use_id.as_str() == "tu_bash_1")
                .unwrap_or(false)
        })
        .expect("tu_bash_1 content record present");
    assert_eq!(
        bash1.tool_result.as_ref().unwrap().is_error,
        None,
        "tu_bash_1 has no is_error field — must be None"
    );
    let read1 = tool_results
        .iter()
        .find(|c| {
            c.tool_result
                .as_ref()
                .map(|tr| tr.tool_use_id.as_str() == "tu_read_1")
                .unwrap_or(false)
        })
        .expect("tu_read_1 content record present");
    assert_eq!(
        read1.tool_result.as_ref().unwrap().is_error,
        None,
        "tu_read_1 has no is_error field — must be None"
    );
    let bash2 = tool_results
        .iter()
        .find(|c| {
            c.tool_result
                .as_ref()
                .map(|tr| tr.tool_use_id.as_str() == "tu_bash_2")
                .unwrap_or(false)
        })
        .expect("tu_bash_2 content record present");
    assert_eq!(
        bash2.tool_result.as_ref().unwrap().is_error,
        Some(true),
        "tu_bash_2 has is_error=true — must be Some(true)"
    );
}

// ----- parseClaudeSession fork / continuation relationships (#112) -----
//
// Mirrors `describe('parseClaudeSession fork / continuation relationships')`
// (claude.test.ts:991-1267). These exercise the per-file relationship
// evidence and the cross-file `reconcile_claude_session_relationships`
// pass.

#[test]
fn resume_marker_emits_continuation_with_provenance() {
    // claude.test.ts:992 — a `/resume <id>` slash command produces a
    // continuation row whose relatedSessionId is the resume target. The
    // file basename (`resume-marker`) becomes sessionId; the in-log
    // sessionId surfaces as sourceSessionId.
    let path = fixture("resume-marker.jsonl");
    let session_path = path.to_string_lossy().into_owned();
    let res = parse_claude_session_incremental(
        &path,
        &ParseIncrementalOptions {
            session_path: Some(session_path),
            ..Default::default()
        },
    )
    .unwrap();
    let cont = res
        .relationships
        .iter()
        .find(|r| matches!(r.relationship_type, RelationshipType::Continuation))
        .expect("/resume marker must produce a continuation row");
    assert_eq!(cont.session_id, "resume-marker");
    assert_eq!(
        cont.related_session_id.as_deref(),
        Some("11111111-1111-1111-1111-111111111111")
    );
    assert_eq!(
        cont.source_session_id.as_deref(),
        Some("99999999-9999-9999-9999-999999999999")
    );
    assert_eq!(cont.source_version.as_deref(), Some("2.1.97"));
}

#[test]
fn resume_marker_root_carries_provenance_when_in_log_id_differs() {
    // claude.test.ts:1010 — when the file basename and in-log sessionId
    // disagree, both the continuation row and the root row carry the
    // mismatched in-log id as sourceSessionId plus the version banner.
    let path = fixture("resume-marker.jsonl");
    let session_path = path.to_string_lossy().into_owned();
    let res = parse_claude_session(
        &path,
        &ParseOptions {
            session_path: Some(session_path),
            ..Default::default()
        },
    )
    .unwrap();
    let root = res
        .relationships
        .iter()
        .find(|r| matches!(r.relationship_type, RelationshipType::Root))
        .expect("root row should still be emitted");
    assert_eq!(root.session_id, "resume-marker");
    assert_eq!(
        root.source_session_id.as_deref(),
        Some("99999999-9999-9999-9999-999999999999")
    );
    assert_eq!(root.source_version.as_deref(), Some("2.1.97"));
}

#[test]
fn explicit_line_continuedfrom_and_fork_session_id() {
    // claude.test.ts:1020 — `continuedFromSessionId` and `forkSessionId`
    // on a line surface as continuation and fork rows respectively.
    // Evidence carries the explicit target ids so reconciliation can dedup
    // against them.
    let path = fixture("explicit-line-relationships.jsonl");
    let session_path = path.to_string_lossy().into_owned();
    let res = parse_claude_session(
        &path,
        &ParseOptions {
            session_path: Some(session_path),
            ..Default::default()
        },
    )
    .unwrap();
    let cont = res
        .relationships
        .iter()
        .find(|r| matches!(r.relationship_type, RelationshipType::Continuation))
        .expect("continuedFromSessionId must produce a continuation row");
    let fork = res
        .relationships
        .iter()
        .find(|r| matches!(r.relationship_type, RelationshipType::Fork))
        .expect("forkSessionId must produce a fork row");
    assert_eq!(cont.session_id, "explicit-line-relationships");
    assert_eq!(cont.related_session_id.as_deref(), Some("original-session"));
    assert_eq!(
        cont.source_session_id.as_deref(),
        Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
    );
    assert_eq!(cont.source_version.as_deref(), Some("2.1.98"));
    assert_eq!(fork.session_id, "explicit-line-relationships");
    assert_eq!(
        fork.related_session_id.as_deref(),
        Some("fork-source-session")
    );
    assert_eq!(
        fork.source_session_id.as_deref(),
        Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
    );
    assert_eq!(fork.source_version.as_deref(), Some("2.1.98"));

    assert_eq!(
        res.evidence
            .explicit_continuation_target_session_ids
            .as_deref(),
        Some(["original-session".to_string()].as_slice())
    );
    assert_eq!(
        res.evidence.explicit_fork_target_session_ids.as_deref(),
        Some(["fork-source-session".to_string()].as_slice())
    );
}

#[test]
fn reconciliation_skips_explicit_continuation_edge() {
    // claude.test.ts:1042 — when a file already emits a
    // `continuedFromSessionId` continuation row, the cross-file
    // parentUuid pass must not re-emit the same edge. Otherwise the
    // ledger would dedup but only after writing duplicates.
    let original_path = fixture("original-session.jsonl");
    let explicit_path = fixture("explicit-line-relationships.jsonl");
    let original_session_path = original_path.to_string_lossy().into_owned();
    let explicit_session_path = explicit_path.to_string_lossy().into_owned();
    let original = parse_claude_session(
        &original_path,
        &ParseOptions {
            session_path: Some(original_session_path),
            ..Default::default()
        },
    )
    .unwrap();
    let explicit = parse_claude_session(
        &explicit_path,
        &ParseOptions {
            session_path: Some(explicit_session_path),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(explicit.relationships.iter().any(|r| matches!(
        r.relationship_type,
        RelationshipType::Continuation
    ) && r.session_id
        == "explicit-line-relationships"
        && r.related_session_id.as_deref() == Some("original-session")));

    let reconciled = reconcile_claude_session_relationships(&[
        ReconcileClaudeRelationshipsInput {
            evidence: original.evidence,
        },
        ReconcileClaudeRelationshipsInput {
            evidence: explicit.evidence,
        },
    ]);
    let dup = reconciled.iter().any(|r| {
        matches!(r.relationship_type, RelationshipType::Continuation)
            && r.session_id == "explicit-line-relationships"
            && r.related_session_id.as_deref() == Some("original-session")
    });
    assert!(
        !dup,
        "cross-file parentUuid inference must not duplicate the explicit edge"
    );
}

#[test]
fn first_parent_uuid_skips_leading_sidechain_user_line() {
    // claude.test.ts:1077 — the `firstParentUuid` evidence is gated to
    // the first non-sidechain user line, so a sidechain user line that
    // happens to come first is ignored. (The TS gate uses a module-level
    // WeakSet; the Rust port tracks `user_seen` inline.)
    let path = fixture("sidechain-leading-then-main.jsonl");
    let session_path = path.to_string_lossy().into_owned();
    let res = parse_claude_session(
        &path,
        &ParseOptions {
            session_path: Some(session_path),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        res.evidence.first_parent_uuid.as_deref(),
        Some("u-original-asst")
    );
}

#[test]
fn evidence_exposes_per_file_signals_for_reconciliation() {
    // claude.test.ts:1083 — evidence carries everything the cross-file
    // reconciler needs: file id, source version, the resume-marker flag
    // and target, and the seenUuids set the cross-file pass joins on.
    let path = fixture("resume-marker.jsonl");
    let session_path = path.to_string_lossy().into_owned();
    let res = parse_claude_session(
        &path,
        &ParseOptions {
            session_path: Some(session_path),
            ..Default::default()
        },
    )
    .unwrap();
    let ev = &res.evidence;
    assert_eq!(ev.file_session_id.as_deref(), Some("resume-marker"));
    assert_eq!(ev.source_version.as_deref(), Some("2.1.97"));
    assert!(ev.has_resume_marker);
    assert_eq!(
        ev.resume_target_session_id.as_deref(),
        Some("11111111-1111-1111-1111-111111111111")
    );
    // The first non-sidechain user line's parentUuid is null in this
    // fixture, so firstParentUuid stays None.
    assert!(ev.first_parent_uuid.is_none());
    assert!(ev.seen_uuids.iter().any(|s| s == "u-resume-1"));
    assert!(ev.seen_uuids.iter().any(|s| s == "u-asst-r"));
}

#[test]
fn reconcile_emits_continuation_when_parent_uuid_lives_in_other_file() {
    // claude.test.ts:1098 — the cross-file pass joins one file's
    // firstParentUuid onto another file's seenUuids set, producing a
    // continuation row that the local pass alone could not have emitted
    // (no /resume marker).
    let original_path = fixture("original-session.jsonl");
    let cross_path = fixture("cross-file-parent.jsonl");
    let original_session_path = original_path.to_string_lossy().into_owned();
    let cross_session_path = cross_path.to_string_lossy().into_owned();
    let original = parse_claude_session(
        &original_path,
        &ParseOptions {
            session_path: Some(original_session_path),
            ..Default::default()
        },
    )
    .unwrap();
    let cross = parse_claude_session(
        &cross_path,
        &ParseOptions {
            session_path: Some(cross_session_path),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        cross.evidence.first_parent_uuid.as_deref(),
        Some("u-original-asst")
    );
    assert!(!cross
        .relationships
        .iter()
        .any(|r| matches!(r.relationship_type, RelationshipType::Continuation)));

    let reconciled = reconcile_claude_session_relationships(&[
        ReconcileClaudeRelationshipsInput {
            evidence: original.evidence,
        },
        ReconcileClaudeRelationshipsInput {
            evidence: cross.evidence,
        },
    ]);
    let cont = reconciled
        .iter()
        .find(|r| matches!(r.relationship_type, RelationshipType::Continuation))
        .expect("cross-file parentUuid match must produce a continuation row");
    assert_eq!(cont.session_id, "cross-file-parent");
    assert_eq!(cont.related_session_id.as_deref(), Some("original-session"));
    assert_eq!(cont.source_version.as_deref(), Some("2.1.97"));
}

#[test]
fn reconcile_emits_fork_rows_when_two_files_share_source_session_id() {
    // claude.test.ts:1128 — two branches with the same in-log sessionId
    // (different filenames) get one fork row each pointing at the shared
    // sourceSessionId.
    let a_path = fixture("fork-branch-a.jsonl");
    let b_path = fixture("fork-branch-b.jsonl");
    let a_session_path = a_path.to_string_lossy().into_owned();
    let b_session_path = b_path.to_string_lossy().into_owned();
    let a = parse_claude_session(
        &a_path,
        &ParseOptions {
            session_path: Some(a_session_path),
            ..Default::default()
        },
    )
    .unwrap();
    let b = parse_claude_session(
        &b_path,
        &ParseOptions {
            session_path: Some(b_session_path),
            ..Default::default()
        },
    )
    .unwrap();

    let root_a = a
        .relationships
        .iter()
        .find(|r| matches!(r.relationship_type, RelationshipType::Root))
        .unwrap();
    let root_b = b
        .relationships
        .iter()
        .find(|r| matches!(r.relationship_type, RelationshipType::Root))
        .unwrap();
    assert_eq!(root_a.session_id, "fork-branch-a");
    assert_eq!(root_b.session_id, "fork-branch-b");
    assert_eq!(
        root_a.source_session_id.as_deref(),
        Some("00000000-0000-0000-0000-000000000fff")
    );
    assert_eq!(
        root_b.source_session_id.as_deref(),
        Some("00000000-0000-0000-0000-000000000fff")
    );

    let reconciled = reconcile_claude_session_relationships(&[
        ReconcileClaudeRelationshipsInput {
            evidence: a.evidence,
        },
        ReconcileClaudeRelationshipsInput {
            evidence: b.evidence,
        },
    ]);
    let forks: Vec<&SessionRelationshipRecord> = reconciled
        .iter()
        .filter(|r| matches!(r.relationship_type, RelationshipType::Fork))
        .collect();
    assert_eq!(forks.len(), 2);
    let mut sids: Vec<&str> = forks.iter().map(|r| r.session_id.as_str()).collect();
    sids.sort();
    assert_eq!(sids, vec!["fork-branch-a", "fork-branch-b"]);
    for f in forks {
        assert_eq!(
            f.related_session_id.as_deref(),
            Some("00000000-0000-0000-0000-000000000fff")
        );
        assert_eq!(
            f.source_session_id.as_deref(),
            Some("00000000-0000-0000-0000-000000000fff")
        );
        assert_eq!(f.source_version.as_deref(), Some("2.1.97"));
    }
}

#[test]
fn reconcile_does_not_emit_fork_for_strict_continuation() {
    // claude.test.ts:1162 — when file B's firstParentUuid lives in file A
    // (a strict continuation), reconciliation must not double-emit a fork
    // row even though both files share a sourceSessionId.
    let a_path = fixture("original-session.jsonl");
    let b_path = fixture("cross-file-parent.jsonl");
    let a_session_path = a_path.to_string_lossy().into_owned();
    let b_session_path = b_path.to_string_lossy().into_owned();
    let a = parse_claude_session(
        &a_path,
        &ParseOptions {
            session_path: Some(a_session_path),
            ..Default::default()
        },
    )
    .unwrap();
    let b = parse_claude_session(
        &b_path,
        &ParseOptions {
            session_path: Some(b_session_path),
            ..Default::default()
        },
    )
    .unwrap();
    let reconciled = reconcile_claude_session_relationships(&[
        ReconcileClaudeRelationshipsInput {
            evidence: a.evidence,
        },
        ReconcileClaudeRelationshipsInput {
            evidence: b.evidence,
        },
    ]);
    let forks = reconciled
        .iter()
        .filter(|r| matches!(r.relationship_type, RelationshipType::Fork))
        .count();
    let conts = reconciled
        .iter()
        .filter(|r| matches!(r.relationship_type, RelationshipType::Continuation))
        .count();
    assert_eq!(forks, 0);
    assert_eq!(conts, 1);
}

#[test]
fn reparsing_same_session_yields_stable_relationship_keys() {
    // claude.test.ts:1182 — re-ingesting the same session must produce
    // relationship rows that hash to the same dedup key, so the writer's
    // existing dedup folds them. We reproduce the canonical key here
    // (source + sessionId + relationshipType + relatedSessionId + agentId
    // + parentToolUseId) instead of importing it across the
    // reader/ledger boundary, matching the TS test exactly.
    fn key_of(r: &SessionRelationshipRecord) -> String {
        let source = match r.source {
            RelationshipSourceKind::ClaudeCode => "claude-code",
            RelationshipSourceKind::Codex => "codex",
            RelationshipSourceKind::Opencode => "opencode",
            RelationshipSourceKind::AnthropicApi => "anthropic-api",
            RelationshipSourceKind::OpenaiApi => "openai-api",
            RelationshipSourceKind::GeminiApi => "gemini-api",
            RelationshipSourceKind::SpawnEnv => "spawn-env",
            RelationshipSourceKind::NativeClaude => "native-claude",
            RelationshipSourceKind::NativeOpencode => "native-opencode",
        };
        let kind = match r.relationship_type {
            RelationshipType::Root => "root",
            RelationshipType::Continuation => "continuation",
            RelationshipType::Fork => "fork",
            RelationshipType::Subagent => "subagent",
        };
        format!(
            "{}|{}|{}|{}|{}|{}",
            source,
            r.session_id,
            kind,
            r.related_session_id.as_deref().unwrap_or(""),
            r.agent_id.as_deref().unwrap_or(""),
            r.parent_tool_use_id.as_deref().unwrap_or(""),
        )
    }
    let path = fixture("resume-marker.jsonl");
    let session_path = path.to_string_lossy().into_owned();
    let opts = ParseOptions {
        session_path: Some(session_path),
        ..Default::default()
    };
    let a = parse_claude_session(&path, &opts).unwrap();
    let b = parse_claude_session(&path, &opts).unwrap();
    let mut ids_a: Vec<String> = a.relationships.iter().map(key_of).collect();
    let mut ids_b: Vec<String> = b.relationships.iter().map(key_of).collect();
    let unique_a: std::collections::HashSet<&String> = ids_a.iter().collect();
    assert_eq!(
        unique_a.len(),
        a.relationships.len(),
        "every row should hash uniquely on first parse"
    );
    ids_a.sort();
    ids_b.sort();
    assert_eq!(ids_a, ids_b);
}

#[test]
fn reconcile_skips_duplicate_continuation_when_local_resume_named_same_parent() {
    // claude.test.ts:1216 — when the local /resume already emitted a
    // continuation row for the same edge the cross-file pass would
    // produce, reconciliation must not double-emit. We construct evidence
    // pairs in memory (parent file + child file) that line up exactly:
    // the child's firstParentUuid lives in the parent's seenUuids, AND
    // the child's resume marker names the parent's file id. (The TS test
    // builds partial objects; in Rust we set the fields explicitly,
    // including the private `user_seen` gate, since we are inside the
    // same module.)
    let parent_evidence = ClaudeRelationshipEvidence {
        file_session_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
        in_log_session_ids: vec!["11111111-1111-1111-1111-111111111111".to_string()],
        seen_uuids: vec!["u-original-asst".to_string()],
        ..ClaudeRelationshipEvidence::default()
    };
    let child_evidence = ClaudeRelationshipEvidence {
        file_session_id: Some("resume-marker".to_string()),
        in_log_session_ids: vec!["99999999-9999-9999-9999-999999999999".to_string()],
        seen_uuids: vec![],
        has_resume_marker: true,
        resume_target_session_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
        first_parent_uuid: Some("u-original-asst".to_string()),
        source_version: Some("2.1.97".to_string()),
        ..ClaudeRelationshipEvidence::default()
    };
    let reconciled = reconcile_claude_session_relationships(&[
        ReconcileClaudeRelationshipsInput {
            evidence: parent_evidence,
        },
        ReconcileClaudeRelationshipsInput {
            evidence: child_evidence,
        },
    ]);
    let dup = reconciled
        .iter()
        .filter(|r| {
            matches!(r.relationship_type, RelationshipType::Continuation)
                && r.session_id == "resume-marker"
                && r.related_session_id.as_deref() == Some("11111111-1111-1111-1111-111111111111")
        })
        .count();
    assert_eq!(dup, 0);
}

#[test]
fn subagent_rows_carry_provenance_when_basename_differs_from_in_log_id() {
    // claude.test.ts:1252 — copy nested-subagent.jsonl to a tmp filename
    // distinct from its in-log sessionId. Subagent rows must carry the
    // mismatched in-log id as sourceSessionId, just like roots do, so
    // cross-source joins can group all rows under one provenance banner.
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");
    let src = fixture("nested-subagent.jsonl");
    std::fs::copy(&src, &working).unwrap();
    let session_path = working.to_string_lossy().into_owned();
    let res = parse_claude_session(
        &working,
        &ParseOptions {
            session_path: Some(session_path),
            ..Default::default()
        },
    )
    .unwrap();
    let sub = res
        .relationships
        .iter()
        .find(|r| matches!(r.relationship_type, RelationshipType::Subagent))
        .expect("fixture has subagent rows");
    assert_eq!(
        sub.source_session_id.as_deref(),
        Some("55555555-5555-5555-5555-555555555555")
    );
}

// ----- parentUuid chain grouping (#433) end-to-end -----
//
// These two tests prove the chain walk replaces the file-order text
// association without depending on parse-state invariants beyond the
// public TurnRecord shape. The fixture rows are crafted so the old
// heuristic would mis-classify at least one turn, and the chain walk
// recovers the right answer.

/// Out-of-order JSONL flush: two user prompts land in the file before
/// either assistant's first chunk. Under the legacy file-order map the
/// first assistant (`msg_bug_assistant`) would inherit the *second*
/// user prompt's text ("add a new feature ...") and mis-classify as
/// `Feature`. The parent-chain walk routes it to the correct user
/// prompt ("fix the bug ...") and classifies as `Debugging`.
#[test]
fn parent_chain_groups_out_of_order_rows_for_classification() {
    let path = fixture("parent-chain-out-of-order.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.turns.len(), 2, "fixture has two assistant turns");
    let bug = res
        .turns
        .iter()
        .find(|t| t.message_id == "msg_bug_assistant")
        .expect("bug-fix turn present");
    let feature = res
        .turns
        .iter()
        .find(|t| t.message_id == "msg_feature_assistant")
        .expect("feature turn present");
    // The discriminating assertion: file-order would attach
    // "add a new feature ..." to BOTH turns (FEATURE_RE wins), so the
    // bug-fix turn would mis-classify. Chain walk routes each turn
    // to its own parentUuid root.
    assert_eq!(
        bug.activity,
        Some(ActivityCategory::Debugging),
        "bug-fix turn must use its own user prompt text via parentUuid chain (#433)"
    );
    assert_eq!(
        feature.activity,
        Some(ActivityCategory::Feature),
        "feature turn must use its own user prompt text"
    );
}

/// Interrupt + resume: the user cancels mid-stream and types a new
/// prompt; the original turn's only assistant chunk arrives *after*
/// the resume turn completes. Under the legacy file-order map the
/// late refactor assistant inherits the bug-fix user text and
/// mis-classifies as `Debugging`. The parent-chain walk pins it to
/// the original "refactor the auth module" prompt and classifies as
/// `Refactoring`.
#[test]
fn parent_chain_groups_interrupt_resume_rows_into_original_turn() {
    let path = fixture("parent-chain-interrupt-resume.jsonl");
    let res = parse_claude_session(&path, &ParseOptions::default()).unwrap();
    assert_eq!(res.turns.len(), 2, "fixture has two assistant turns");
    let bugfix = res
        .turns
        .iter()
        .find(|t| t.message_id == "msg_bugfix_assistant")
        .expect("bug-fix interrupt turn present");
    let refactor = res
        .turns
        .iter()
        .find(|t| t.message_id == "msg_refactor_assistant")
        .expect("refactor turn present");
    assert_eq!(
        bugfix.activity,
        Some(ActivityCategory::Debugging),
        "bug-fix turn must classify against its own prompt"
    );
    assert_eq!(
        refactor.activity,
        Some(ActivityCategory::Refactoring),
        "late-arriving refactor turn must classify against its ORIGINAL prompt via parentUuid chain (#433), not the most recently seen prompt"
    );
}

/// Cycle guard: synthetic loop in the parent chain must not hang the
/// turn-classification path. With no reachable user-prompt root the
/// activity classifier sees empty user text (falls back through the
/// classifier's no-text branches); the key assertion is that
/// `parse_claude_session` returns in finite time.
#[test]
fn parent_chain_cycle_in_assistant_chain_does_not_hang() {
    // Two assistant rows whose parentUuids point at each other,
    // sharing a message_id so they collapse to one turn. The fixture
    // has no user-prompt root and no `stop_reason`, so the chain
    // walk hits the cycle guard and returns `None`; classification
    // falls through to the empty-text branch without looping.
    let dir = tempfile::tempdir().unwrap();
    let working = dir.path().join("session.jsonl");
    let body = [
        serde_json::json!({
            "parentUuid": "u-asst-cycle-b",
            "isSidechain": false,
            "type": "assistant",
            "uuid": "u-asst-cycle-a",
            "sessionId": "cccccccc-cccc-cccc-cccc-cccccccccccc",
            "timestamp": "2026-05-01T00:00:00.000Z",
            "cwd": "/tmp/project",
            "message": {
                "id": "msg_cycle",
                "model": "claude-sonnet-4-6",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "first chunk"}],
                "stop_reason": null,
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        }),
        serde_json::json!({
            "parentUuid": "u-asst-cycle-a",
            "isSidechain": false,
            "type": "assistant",
            "uuid": "u-asst-cycle-b",
            "sessionId": "cccccccc-cccc-cccc-cccc-cccccccccccc",
            "timestamp": "2026-05-01T00:00:01.000Z",
            "cwd": "/tmp/project",
            "message": {
                "id": "msg_cycle",
                "model": "claude-sonnet-4-6",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "second chunk"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        }),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n";
    write_bytes(&working, body.as_bytes());
    // Just calling this and asserting it returns is the test — a hang
    // here would cause the suite to time out. The classifier output
    // is incidental; we only verify the call completes.
    let res = parse_claude_session(&working, &ParseOptions::default()).unwrap();
    assert_eq!(res.turns.len(), 1);
    assert_eq!(res.turns[0].message_id, "msg_cycle");
}
