//! Codex parser conformance tests. The fixtures live at the repo root
//! (`tests/fixtures/codex/*.jsonl`) and are shared with the TS test suite —
//! mirroring the expectations in `packages/reader/src/codex.test.ts`.

use std::io::Write;
use std::path::PathBuf;

use serde_json::json;
use tempfile::tempdir;

use super::*;
use crate::types::{
    ContentKind, ContentRole, FidelityClass, RelationshipType, SourceKind, ToolResultEventSource,
    ToolResultStatus, UsageGranularity, UserTurnBlockKind,
};
use crate::user_turn::UserTurnTokenizer;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/relayburn-reader/ -> repo root
    p.pop();
    p.pop();
    p.push("tests/fixtures/codex");
    p.push(name);
    p
}

fn parse(name: &str) -> ParseCodexResult {
    parse_codex_session(fixture(name), &ParseCodexOptions::default()).unwrap()
}

fn parse_with(name: &str, opts: ParseCodexOptions) -> ParseCodexResult {
    parse_codex_session(fixture(name), &opts).unwrap()
}

fn write_jsonl(dir: &std::path::Path, name: &str, lines: &[serde_json::Value]) -> PathBuf {
    let path = dir.join(name);
    let mut s = String::new();
    for v in lines {
        s.push_str(&serde_json::to_string(v).unwrap());
        s.push('\n');
    }
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(s.as_bytes()).unwrap();
    path
}

// ---------------------------------------------------------------------------
// readCodexSessionIdHint
// ---------------------------------------------------------------------------

#[test]
fn read_session_id_hint_from_session_meta() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("rollout.jsonl");
    std::fs::write(
        &path,
        b"{\"timestamp\":\"2026-04-20T00:00:00.000Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"sess_hint_1\",\"cwd\":\"/tmp\"}}\n",
    )
    .unwrap();
    assert_eq!(
        read_codex_session_id_hint(&path),
        Some("sess_hint_1".to_string()),
    );
}

#[test]
fn read_session_id_hint_returns_none_for_malformed() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("rollout.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"session_meta\",\"payload\":\n{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\"}}\n",
    )
    .unwrap();
    assert_eq!(read_codex_session_id_hint(&path), None);
}

#[test]
fn read_session_id_hint_returns_none_for_empty() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("empty.jsonl");
    std::fs::write(&path, b"").unwrap();
    assert_eq!(read_codex_session_id_hint(&path), None);
}

// ---------------------------------------------------------------------------
// parseCodexSession — basic shapes
// ---------------------------------------------------------------------------

#[test]
fn parses_simple_one_turn_session() {
    let r = parse("simple-turn.jsonl");
    assert_eq!(r.turns.len(), 1);
    let t = &r.turns[0];
    assert_eq!(t.v, 1);
    assert!(matches!(t.source, SourceKind::Codex));
    assert_eq!(t.session_id, "sess_simple_1");
    assert_eq!(t.message_id, "turn_simple_1");
    assert_eq!(t.turn_index, 0);
    assert_eq!(t.model, "gpt-5.4");
    assert_eq!(t.ts, "2026-04-20T00:00:00.200Z");
    assert_eq!(t.usage.input, 600);
    assert_eq!(t.usage.output, 120);
    assert_eq!(t.usage.reasoning, 30);
    assert_eq!(t.usage.cache_read, 400);
    assert_eq!(t.usage.cache_create_5m, 0);
    assert_eq!(t.tool_calls.len(), 0);
    assert!(t.files_touched.is_none());
}

#[test]
fn extracts_function_and_custom_tool_calls_with_files_touched() {
    let r = parse("with-tool-call.jsonl");
    assert_eq!(r.turns.len(), 1);
    let t = &r.turns[0];
    assert_eq!(t.model, "gpt-5.3-codex");
    assert_eq!(t.usage.input, 3000);
    assert_eq!(t.usage.output, 800);
    assert_eq!(t.usage.reasoning, 200);
    assert_eq!(t.usage.cache_read, 2000);
    assert_eq!(t.tool_calls.len(), 3);
    let exec = &t.tool_calls[0];
    assert_eq!(exec.name, "exec_command");
    assert_eq!(exec.target.as_deref(), Some("git status"));
    let p1 = &t.tool_calls[1];
    assert_eq!(p1.name, "apply_patch");
    assert_eq!(p1.target.as_deref(), Some("/tmp/project/README.md"));
    let p2 = &t.tool_calls[2];
    assert_eq!(p2.name, "apply_patch");
    assert_eq!(p2.target.as_deref(), Some("/tmp/project/NEW.md"));
    let mut files = t.files_touched.clone().unwrap();
    files.sort();
    assert_eq!(files, vec!["/tmp/project/NEW.md", "/tmp/project/README.md"]);
}

#[test]
fn computes_per_turn_usage_as_delta_of_cumulative_totals() {
    let r = parse("multi-turn.jsonl");
    assert_eq!(r.turns.len(), 2);
    let t1 = &r.turns[0];
    assert_eq!(t1.message_id, "turn_multi_1");
    assert_eq!(t1.turn_index, 0);
    assert_eq!(t1.model, "gpt-5.4");
    assert_eq!(t1.usage.input, 2000);
    assert_eq!(t1.usage.output, 200);
    assert_eq!(t1.usage.reasoning, 50);
    assert_eq!(t1.usage.cache_read, 1000);
    let t2 = &r.turns[1];
    assert_eq!(t2.message_id, "turn_multi_2");
    assert_eq!(t2.turn_index, 1);
    assert_eq!(t2.model, "gpt-5.3-codex");
    assert_eq!(t2.usage.input, 2500);
    assert_eq!(t2.usage.output, 500);
    assert_eq!(t2.usage.reasoning, 50);
    assert_eq!(t2.usage.cache_read, 2500);
    assert_eq!(t2.tool_calls.len(), 1);
    assert_eq!(t2.tool_calls[0].name, "exec_command");
    assert_eq!(t2.tool_calls[0].target.as_deref(), Some("ls"));
}

#[test]
fn emits_compaction_event_anchored_to_preceding_turn() {
    let r = parse("compaction.jsonl");
    assert_eq!(r.turns.len(), 2);
    assert_eq!(r.events.len(), 1);
    let ev = &r.events[0];
    assert!(matches!(ev.source, SourceKind::Codex));
    assert_eq!(ev.session_id, "sess_codex_compact");
    assert_eq!(ev.ts, "2026-04-20T03:00:03.000Z");
    assert_eq!(ev.preceding_message_id.as_deref(), Some("turn_compact_1"));
    let preceding = r
        .turns
        .iter()
        .find(|t| t.message_id == "turn_compact_1")
        .unwrap();
    assert_eq!(ev.tokens_before_compact, Some(preceding.usage.cache_read));
    assert_eq!(ev.tokens_before_compact, Some(1000));
}

#[test]
fn produces_stable_args_hash_for_identical_inputs() {
    let a = parse("with-tool-call.jsonl");
    let b = parse("with-tool-call.jsonl");
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
fn respects_session_path_option() {
    let path = fixture("simple-turn.jsonl");
    let r = parse_with(
        "simple-turn.jsonl",
        ParseCodexOptions {
            session_path: Some(path.to_string_lossy().into()),
            ..Default::default()
        },
    );
    assert_eq!(
        r.turns[0].session_path.as_deref(),
        Some(path.to_string_lossy().as_ref())
    );
}

#[test]
fn classifies_activity_and_fills_retries_has_edits() {
    let r = parse("with-tool-call.jsonl");
    let t = &r.turns[0];
    // apply_patch normalizes to Edit → has_edits true; .md targets → docs.
    assert_eq!(t.has_edits, Some(true));
    assert!(matches!(
        t.activity,
        Some(crate::types::ActivityCategory::Docs)
    ));
    assert!(t.retries.is_some());
}

#[test]
fn marks_failed_exec_command_as_debugging() {
    let tmp = tempdir().unwrap();
    let path = write_jsonl(
        tmp.path(),
        "fail.jsonl",
        &[
            json!({"timestamp":"2026-04-22T00:00:00.000Z","type":"session_meta","payload":{"id":"sess_fail","cwd":"/tmp/proj","timestamp":"2026-04-22T00:00:00.000Z"}}),
            json!({"timestamp":"2026-04-22T00:00:00.100Z","type":"turn_context","payload":{"turn_id":"turn_fail_1","cwd":"/tmp/proj","model":"gpt-5.4"}}),
            json!({"timestamp":"2026-04-22T00:00:00.200Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"run the tests please"}]}}),
            json!({"timestamp":"2026-04-22T00:00:00.300Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn_fail_1"}}),
            json!({"timestamp":"2026-04-22T00:00:01.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"pytest -q\"}","call_id":"call_fail_1"}}),
            json!({"timestamp":"2026-04-22T00:00:01.500Z","type":"event_msg","payload":{"type":"exec_command_end","call_id":"call_fail_1","turn_id":"turn_fail_1","exit_code":1}}),
            json!({"timestamp":"2026-04-22T00:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":50}}}}),
            json!({"timestamp":"2026-04-22T00:00:02.100Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn_fail_1"}}),
        ],
    );
    let r = parse_codex_session(&path, &ParseCodexOptions::default()).unwrap();
    assert_eq!(r.turns.len(), 1);
    assert!(matches!(
        r.turns[0].activity,
        Some(crate::types::ActivityCategory::Debugging)
    ));
    assert_eq!(r.turns[0].has_edits, Some(false));
}

#[test]
fn user_prompt_drives_keyword_refinement_skipping_boilerplate() {
    let tmp = tempdir().unwrap();
    let path = write_jsonl(
        tmp.path(),
        "kw.jsonl",
        &[
            json!({"timestamp":"2026-04-22T00:00:00.000Z","type":"session_meta","payload":{"id":"sess_kw","cwd":"/tmp/proj","timestamp":"2026-04-22T00:00:00.000Z"}}),
            json!({"timestamp":"2026-04-22T00:00:00.050Z","type":"turn_context","payload":{"turn_id":"turn_kw_1","cwd":"/tmp/proj","model":"gpt-5.4"}}),
            json!({"timestamp":"2026-04-22T00:00:00.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[
                {"type":"input_text","text":"<environment_context><cwd>/tmp/proj</cwd></environment_context>"},
                {"type":"input_text","text":"refactor the auth module to extract the token helper"}
            ]}}),
            json!({"timestamp":"2026-04-22T00:00:00.200Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn_kw_1"}}),
            json!({"timestamp":"2026-04-22T00:00:01.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n*** Update File: /tmp/proj/auth.ts\n@@\n+ok\n*** End Patch\n","call_id":"call_kw_1"}}),
            json!({"timestamp":"2026-04-22T00:00:01.200Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"call_kw_1","turn_id":"turn_kw_1","success":true,"changes":{"/tmp/proj/auth.ts":{"type":"update"}}}}),
            json!({"timestamp":"2026-04-22T00:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":50}}}}),
            json!({"timestamp":"2026-04-22T00:00:02.100Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn_kw_1"}}),
        ],
    );
    let r = parse_codex_session(&path, &ParseCodexOptions::default()).unwrap();
    assert_eq!(r.turns.len(), 1);
    assert!(matches!(
        r.turns[0].activity,
        Some(crate::types::ActivityCategory::Refactoring)
    ));
    assert_eq!(r.turns[0].has_edits, Some(true));
}

// ---------------------------------------------------------------------------
// Incremental
// ---------------------------------------------------------------------------

#[test]
fn incremental_full_parse_matches_full_parse() {
    let path = fixture("multi-turn.jsonl");
    let expected = parse("multi-turn.jsonl");
    let r =
        parse_codex_session_incremental(&path, &ParseCodexIncrementalOptions::default()).unwrap();
    assert_eq!(r.turns.len(), expected.turns.len());
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(r.end_offset, raw.len() as u64);
}

#[test]
fn splits_at_task_complete_and_resumes_with_cumulative_snapshot() {
    let path = fixture("multi-turn.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = raw.split('\n').collect();
    // Cut after the first task_complete (line index 5, zero-based).
    let cutoff: usize = (lines[..6].join("\n") + "\n").len();

    let tmp = tempdir().unwrap();
    let partial_path = tmp.path().join("partial.jsonl");
    std::fs::write(&partial_path, &raw[..cutoff]).unwrap();
    let partial =
        parse_codex_session_incremental(&partial_path, &ParseCodexIncrementalOptions::default())
            .unwrap();
    assert_eq!(partial.turns.len(), 1);
    assert_eq!(partial.turns[0].message_id, "turn_multi_1");
    assert_eq!(partial.end_offset as usize, cutoff);

    let full_path = tmp.path().join("full.jsonl");
    std::fs::write(&full_path, &raw).unwrap();
    let resumed = parse_codex_session_incremental(
        &full_path,
        &ParseCodexIncrementalOptions {
            start_offset: Some(partial.end_offset),
            resume: Some(partial.resume.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(resumed.turns.len(), 1);
    assert_eq!(resumed.turns[0].message_id, "turn_multi_2");
    let full = parse_codex_session(&full_path, &ParseCodexOptions::default()).unwrap();
    assert_eq!(resumed.turns[0].usage, full.turns[1].usage);
}

#[test]
fn incremental_anchors_resumed_compaction_to_previous_cursor_turn() {
    let path = fixture("compaction.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = raw.split('\n').collect();
    // Cut after first task_complete at index 4 (zero-based).
    let cutoff: usize = (lines[..5].join("\n") + "\n").len();

    let tmp = tempdir().unwrap();
    let partial_path = tmp.path().join("partial.jsonl");
    std::fs::write(&partial_path, &raw[..cutoff]).unwrap();
    let partial =
        parse_codex_session_incremental(&partial_path, &ParseCodexIncrementalOptions::default())
            .unwrap();
    assert_eq!(partial.turns.len(), 1);
    assert_eq!(partial.events.len(), 0);
    assert_eq!(
        partial.resume.last_completed_turn,
        Some(CodexLastCompletedTurn {
            message_id: "turn_compact_1".into(),
            cache_read: 1000,
        })
    );

    let full_path = tmp.path().join("full.jsonl");
    std::fs::write(&full_path, &raw).unwrap();
    let resumed = parse_codex_session_incremental(
        &full_path,
        &ParseCodexIncrementalOptions {
            start_offset: Some(partial.end_offset),
            resume: Some(partial.resume.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(resumed.turns.len(), 1);
    assert_eq!(resumed.turns[0].message_id, "turn_compact_2");
    assert_eq!(resumed.events.len(), 1);
    assert_eq!(
        resumed.events[0].preceding_message_id.as_deref(),
        Some("turn_compact_1")
    );
    assert_eq!(resumed.events[0].tokens_before_compact, Some(1000));
}

#[test]
fn incremental_does_not_advance_end_offset_without_task_complete() {
    let path = fixture("simple-turn.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = raw.split('\n').collect();
    let truncated = lines[..3].join("\n") + "\n";
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("trunc.jsonl");
    std::fs::write(&path, &truncated).unwrap();
    let r =
        parse_codex_session_incremental(&path, &ParseCodexIncrementalOptions::default()).unwrap();
    assert_eq!(r.turns.len(), 0);
    assert_eq!(r.end_offset, 0);
}

// ---------------------------------------------------------------------------
// Content capture
// ---------------------------------------------------------------------------

fn content_fixture_lines() -> Vec<serde_json::Value> {
    vec![
        json!({"timestamp":"2026-04-20T01:00:00.000Z","type":"session_meta","payload":{"id":"sess_content_1","cwd":"/tmp/project","timestamp":"2026-04-20T01:00:00.000Z"}}),
        json!({"timestamp":"2026-04-20T01:00:00.050Z","type":"turn_context","payload":{"turn_id":"turn_content_1","cwd":"/tmp/project","model":"gpt-5.3-codex"}}),
        json!({"timestamp":"2026-04-20T01:00:00.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"list files"}]}}),
        json!({"timestamp":"2026-04-20T01:00:00.200Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn_content_1"}}),
        json!({"timestamp":"2026-04-20T01:00:00.300Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"planning the ls"}],"content":null}}),
        json!({"timestamp":"2026-04-20T01:00:00.400Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}","call_id":"call_fc_1"}}),
        json!({"timestamp":"2026-04-20T01:00:00.500Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_fc_1","output":"README.md\npackage.json\n"}}),
        json!({"timestamp":"2026-04-20T01:00:00.600Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","input":"*** Begin Patch\n*** Add File: /tmp/project/X\n","call_id":"call_ct_1"}}),
        json!({"timestamp":"2026-04-20T01:00:00.700Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_ct_1","output":"{\"success\":true}"}}),
        json!({"timestamp":"2026-04-20T01:00:00.800Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done."}]}}),
        json!({"timestamp":"2026-04-20T01:00:00.900Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":20,"reasoning_output_tokens":10}}}}),
        json!({"timestamp":"2026-04-20T01:00:01.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn_content_1"}}),
    ]
}

#[test]
fn content_default_off_returns_empty() {
    let tmp = tempdir().unwrap();
    let path = write_jsonl(tmp.path(), "session.jsonl", &content_fixture_lines());
    let r = parse_codex_session(&path, &ParseCodexOptions::default()).unwrap();
    assert!(r.content.is_empty());
}

#[test]
fn content_hash_only_returns_empty() {
    let tmp = tempdir().unwrap();
    let path = write_jsonl(tmp.path(), "session.jsonl", &content_fixture_lines());
    let r = parse_codex_session(
        &path,
        &ParseCodexOptions {
            content_mode: Some(ContentStoreMode::HashOnly),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(r.content.is_empty());
}

#[test]
fn content_full_emits_user_assistant_thinking_tool_use_and_tool_result() {
    let tmp = tempdir().unwrap();
    let path = write_jsonl(tmp.path(), "session.jsonl", &content_fixture_lines());
    let r = parse_codex_session(
        &path,
        &ParseCodexOptions {
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(r.turns.len(), 1);
    let user = r
        .content
        .iter()
        .find(|c| matches!(c.role, ContentRole::User) && matches!(c.kind, ContentKind::Text))
        .unwrap();
    assert_eq!(user.text.as_deref(), Some("list files"));
    assert_eq!(user.message_id, "turn_content_1");
    let asst = r
        .content
        .iter()
        .find(|c| matches!(c.role, ContentRole::Assistant) && matches!(c.kind, ContentKind::Text))
        .unwrap();
    assert_eq!(asst.text.as_deref(), Some("done."));
    let thinking = r
        .content
        .iter()
        .find(|c| matches!(c.kind, ContentKind::Thinking))
        .unwrap();
    assert_eq!(thinking.text.as_deref(), Some("planning the ls"));
    let tool_uses: Vec<_> = r
        .content
        .iter()
        .filter(|c| matches!(c.kind, ContentKind::ToolUse))
        .collect();
    assert_eq!(tool_uses.len(), 2);
    let mut names: Vec<&str> = tool_uses
        .iter()
        .map(|c| c.tool_use.as_ref().unwrap().name.as_str())
        .collect();
    names.sort();
    assert_eq!(names, vec!["apply_patch", "shell"]);
    let tr_fc = r
        .content
        .iter()
        .find(|c| {
            matches!(c.kind, ContentKind::ToolResult)
                && c.tool_result.as_ref().map(|t| t.tool_use_id.as_str()) == Some("call_fc_1")
        })
        .unwrap();
    assert_eq!(
        tr_fc.tool_result.as_ref().unwrap().content,
        json!("README.md\npackage.json\n")
    );
    let tr_ct = r
        .content
        .iter()
        .find(|c| {
            matches!(c.kind, ContentKind::ToolResult)
                && c.tool_result.as_ref().map(|t| t.tool_use_id.as_str()) == Some("call_ct_1")
        })
        .unwrap();
    assert_eq!(
        tr_ct.tool_result.as_ref().unwrap().content,
        json!("{\"success\":true}")
    );
}

#[test]
fn drops_content_when_turn_never_commits() {
    let tmp = tempdir().unwrap();
    let path = write_jsonl(
        tmp.path(),
        "uncommitted.jsonl",
        &[
            json!({"timestamp":"2026-04-20T01:00:00.000Z","type":"session_meta","payload":{"id":"sess_u","cwd":"/tmp","timestamp":"2026-04-20T01:00:00.000Z"}}),
            json!({"timestamp":"2026-04-20T01:00:00.050Z","type":"turn_context","payload":{"turn_id":"turn_u","cwd":"/tmp","model":"gpt-5.4"}}),
            json!({"timestamp":"2026-04-20T01:00:00.200Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn_u"}}),
            json!({"timestamp":"2026-04-20T01:00:00.400Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}","call_id":"call_u_1"}}),
            json!({"timestamp":"2026-04-20T01:00:00.500Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_u_1","output":"should-be-dropped"}}),
        ],
    );
    let r = parse_codex_session(
        &path,
        &ParseCodexOptions {
            content_mode: Some(ContentStoreMode::Full),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(r.turns.len(), 0);
    assert_eq!(r.content.len(), 0);
}

// ---------------------------------------------------------------------------
// User-turn block sizes (issue #81)
// ---------------------------------------------------------------------------

#[test]
fn emits_one_user_turn_per_inter_assistant_gap() {
    let r = parse("user-turn-blocks.jsonl");
    assert_eq!(r.turns.len(), 3);
    assert_eq!(r.user_turns.len(), 3);
    for u in &r.user_turns {
        assert_eq!(u.v, 1);
        assert!(matches!(u.source, SourceKind::Codex));
        assert_eq!(u.session_id, "sess_codex_utb");
        assert!(!u.user_uuid.is_empty());
        assert!(!u.ts.is_empty());
        assert!(!u.blocks.is_empty());
    }
    let pre = &r.user_turns[0];
    assert!(pre.preceding_message_id.is_none());
    assert_eq!(pre.following_message_id.as_deref(), Some("turn_utb_1"));
    assert_eq!(pre.blocks.len(), 1);
    assert!(matches!(pre.blocks[0].kind, UserTurnBlockKind::Text));
    assert_eq!(pre.blocks[0].byte_len, "fix the build".len() as u64);
    // bytes/4 ceil for 13 = 4
    assert_eq!(pre.blocks[0].approx_tokens, 4);

    let between12 = &r.user_turns[1];
    assert_eq!(
        between12.preceding_message_id.as_deref(),
        Some("turn_utb_1")
    );
    assert_eq!(
        between12.following_message_id.as_deref(),
        Some("turn_utb_2")
    );
    assert_eq!(between12.blocks.len(), 2);
    let tr1 = between12
        .blocks
        .iter()
        .find(|b| matches!(b.kind, UserTurnBlockKind::ToolResult))
        .unwrap();
    assert_eq!(tr1.tool_use_id.as_deref(), Some("call_b1"));
    assert_eq!(tr1.byte_len, "a\n".len() as u64);
    assert!(tr1.is_error.is_none());

    let between23 = &r.user_turns[2];
    assert_eq!(
        between23.preceding_message_id.as_deref(),
        Some("turn_utb_2")
    );
    assert_eq!(
        between23.following_message_id.as_deref(),
        Some("turn_utb_3")
    );
    assert_eq!(between23.blocks.len(), 2);
    let fail_block = between23
        .blocks
        .iter()
        .find(|b| b.tool_use_id.as_deref() == Some("call_b2"))
        .unwrap();
    assert!(matches!(fail_block.kind, UserTurnBlockKind::ToolResult));
    assert_eq!(fail_block.byte_len, "FAIL: 1 test broke".len() as u64);
    assert_eq!(fail_block.is_error, Some(true));
    let patch_block = between23
        .blocks
        .iter()
        .find(|b| b.tool_use_id.as_deref() == Some("call_p1"))
        .unwrap();
    assert!(matches!(patch_block.kind, UserTurnBlockKind::ToolResult));
    assert_eq!(patch_block.byte_len, "patched".len() as u64);
    assert!(patch_block.is_error.is_none());
}

#[test]
fn heuristic_tokenizer_uses_bytes_div_4_ceil() {
    let r = parse_with(
        "user-turn-blocks.jsonl",
        ParseCodexOptions {
            tokenizer: Some(UserTurnTokenizer::Heuristic),
            ..Default::default()
        },
    );
    let first = &r.user_turns[0];
    assert_eq!(first.blocks[0].byte_len, "fix the build".len() as u64);
    assert_eq!(first.blocks[0].approx_tokens, 13_u64.div_ceil(4));
}

#[test]
fn empty_user_turns_for_sessions_without_user_blocks() {
    let r = parse("simple-turn.jsonl");
    assert!(r.user_turns.is_empty());
}

#[test]
fn cl100k_tokenizer_is_rejected_until_implemented() {
    let err = parse_codex_session(
        fixture("simple-turn.jsonl"),
        &ParseCodexOptions {
            tokenizer: Some(UserTurnTokenizer::Cl100k),
            ..Default::default()
        },
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("cl100k"), "error mentions cl100k: {msg}");
    let err = parse_codex_session_incremental(
        fixture("simple-turn.jsonl"),
        &ParseCodexIncrementalOptions {
            tokenizer: Some(UserTurnTokenizer::Cl100k),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("cl100k"));
}

// ---------------------------------------------------------------------------
// Execution graph (#42 / #87)
// ---------------------------------------------------------------------------

#[test]
fn emits_exactly_one_root_relationship_per_session() {
    let r = parse("simple-turn.jsonl");
    let roots: Vec<_> = r
        .relationships
        .iter()
        .filter(|x| matches!(x.relationship_type, RelationshipType::Root))
        .collect();
    assert_eq!(roots.len(), 1);
    assert!(matches!(roots[0].source, RelationshipSourceKind::Codex));
    assert_eq!(roots[0].session_id, "sess_simple_1");
    assert!(roots[0].related_session_id.is_none());
    assert_eq!(roots[0].ts.as_deref(), Some("2026-04-20T00:00:00.000Z"));
    assert_eq!(roots[0].source_version.as_deref(), Some("0.121.0"));
}

#[test]
fn emits_fork_and_continuation_rows_from_session_meta() {
    let r = parse("session-meta-relationships.jsonl");
    let count = |t: RelationshipType| {
        r.relationships
            .iter()
            .filter(|x| std::mem::discriminant(&x.relationship_type) == std::mem::discriminant(&t))
            .count()
    };
    assert_eq!(count(RelationshipType::Root), 1);
    assert_eq!(count(RelationshipType::Fork), 1);
    assert_eq!(count(RelationshipType::Continuation), 1);

    let fork = r
        .relationships
        .iter()
        .find(|x| matches!(x.relationship_type, RelationshipType::Fork))
        .unwrap();
    let cont = r
        .relationships
        .iter()
        .find(|x| matches!(x.relationship_type, RelationshipType::Continuation))
        .unwrap();
    let root = r
        .relationships
        .iter()
        .find(|x| matches!(x.relationship_type, RelationshipType::Root))
        .unwrap();
    for row in [root, fork, cont] {
        assert!(matches!(row.source, RelationshipSourceKind::Codex));
        assert_eq!(row.session_id, "sess_meta_child");
        assert_eq!(row.source_session_id.as_deref(), Some("sess_original"));
        assert_eq!(row.source_version.as_deref(), Some("0.130.0"));
        assert_eq!(row.ts.as_deref(), Some("2026-04-24T00:00:00.000Z"));
    }
    assert_eq!(fork.related_session_id.as_deref(), Some("sess_fork_base"));
    assert_eq!(cont.related_session_id.as_deref(), Some("sess_previous"));
}

#[test]
fn emits_tool_result_event_per_function_call_output() {
    let r = parse("user-turn-blocks.jsonl");
    let mut by_call: std::collections::HashMap<&str, u32> = Default::default();
    for ev in &r.tool_result_events {
        *by_call.entry(ev.tool_use_id.as_str()).or_insert(0) += 1;
    }
    assert_eq!(by_call.get("call_b1"), Some(&1));
    assert_eq!(by_call.get("call_b2"), Some(&1));
    assert_eq!(by_call.get("call_p1"), Some(&1));
    assert_eq!(by_call.get("call_b3"), Some(&1));
    for ev in &r.tool_result_events {
        assert_eq!(ev.v, 1);
        assert!(matches!(ev.source, SourceKind::Codex));
        assert!(matches!(
            ev.event_source,
            ToolResultEventSource::FunctionCallOutput
        ));
        assert!(ev.content_length.is_some());
        assert!(ev.content_hash.is_some());
    }
}

#[test]
fn event_index_unique_and_session_monotonic() {
    let r = parse("user-turn-blocks.jsonl");
    let indices: Vec<u64> = r.tool_result_events.iter().map(|e| e.event_index).collect();
    let mut sorted = indices.clone();
    sorted.sort();
    assert_eq!(indices, sorted);
    let unique: std::collections::HashSet<_> = indices.iter().collect();
    assert_eq!(unique.len(), indices.len());
}

#[test]
fn tool_result_status_reflects_exit_codes() {
    let r = parse("user-turn-blocks.jsonl");
    let failed = r
        .tool_result_events
        .iter()
        .find(|e| e.tool_use_id == "call_b2")
        .unwrap();
    assert!(matches!(failed.status, ToolResultStatus::Errored));
    assert_eq!(failed.is_error, Some(true));
    let ok = r
        .tool_result_events
        .iter()
        .find(|e| e.tool_use_id == "call_b1")
        .unwrap();
    assert!(matches!(ok.status, ToolResultStatus::Completed));
    assert!(ok.is_error.is_none());
}

#[test]
fn patch_apply_end_success_marks_completed() {
    let r = parse("user-turn-blocks.jsonl");
    let p = r
        .tool_result_events
        .iter()
        .find(|e| e.tool_use_id == "call_p1")
        .unwrap();
    assert!(matches!(p.status, ToolResultStatus::Completed));
    assert!(p.is_error.is_none());
}

#[test]
fn emits_subagent_relationship_with_parent_call_id() {
    let r = parse("with-spawn-agent.jsonl");
    let subs: Vec<_> = r
        .relationships
        .iter()
        .filter(|x| matches!(x.relationship_type, RelationshipType::Subagent))
        .collect();
    assert_eq!(subs.len(), 1);
    let sub = subs[0];
    assert!(matches!(sub.source, RelationshipSourceKind::Codex));
    assert_eq!(sub.session_id, "agent_inv_42");
    assert_eq!(sub.related_session_id.as_deref(), Some("sess_spawn_1"));
    assert_eq!(sub.parent_tool_use_id.as_deref(), Some("call_spawn_1"));
    assert_eq!(sub.agent_id.as_deref(), Some("agent_inv_42"));
    assert_eq!(sub.subagent_type.as_deref(), Some("investigator"));
    assert_eq!(sub.description.as_deref(), Some("find why test_x fails"));
    assert!(sub.ts.is_some());
}

#[test]
fn spawn_function_call_output_carries_agent_id() {
    let r = parse("with-spawn-agent.jsonl");
    let spawn = r
        .tool_result_events
        .iter()
        .find(|e| {
            e.tool_use_id == "call_spawn_1"
                && matches!(e.event_source, ToolResultEventSource::FunctionCallOutput)
        })
        .unwrap();
    assert_eq!(spawn.agent_id.as_deref(), Some("agent_inv_42"));
    assert_eq!(spawn.subagent_session_id.as_deref(), Some("agent_inv_42"));
}

#[test]
fn subagent_notification_emits_tool_result_event_record() {
    let r = parse("with-spawn-agent.jsonl");
    let notif = r
        .tool_result_events
        .iter()
        .find(|e| {
            matches!(e.event_source, ToolResultEventSource::SubagentNotification)
                && e.tool_use_id == "call_spawn_1"
        })
        .unwrap();
    assert!(matches!(notif.status, ToolResultStatus::Completed));
    assert_eq!(notif.agent_id.as_deref(), Some("agent_inv_42"));
    assert_eq!(notif.subagent_session_id.as_deref(), Some("agent_inv_42"));
}

#[test]
fn re_parse_does_not_duplicate_root_relationship() {
    let a = parse("with-spawn-agent.jsonl");
    let b = parse("with-spawn-agent.jsonl");
    let count = |r: &ParseCodexResult| {
        r.relationships
            .iter()
            .filter(|x| matches!(x.relationship_type, RelationshipType::Root))
            .count()
    };
    assert_eq!(count(&a), 1);
    assert_eq!(count(&b), 1);
}

#[test]
fn uncommitted_turn_does_not_emit_relationships_or_events() {
    let tmp = tempdir().unwrap();
    let path = write_jsonl(
        tmp.path(),
        "uncommitted.jsonl",
        &[
            json!({"timestamp":"2026-04-23T00:00:00.000Z","type":"session_meta","payload":{"id":"sess_u","cwd":"/tmp","timestamp":"2026-04-23T00:00:00.000Z"}}),
            json!({"timestamp":"2026-04-23T00:00:00.050Z","type":"turn_context","payload":{"turn_id":"turn_u","cwd":"/tmp","model":"gpt-5.4"}}),
            json!({"timestamp":"2026-04-23T00:00:00.200Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn_u"}}),
            json!({"timestamp":"2026-04-23T00:00:00.400Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent","arguments":"{\"subagent_type\":\"x\"}","call_id":"call_u_1"}}),
            json!({"timestamp":"2026-04-23T00:00:00.500Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_u_1","output":"{\"agent_id\":\"agent_u\"}"}}),
        ],
    );
    let r = parse_codex_session(&path, &ParseCodexOptions::default()).unwrap();
    assert_eq!(r.turns.len(), 0);
    assert_eq!(r.relationships.len(), 0);
    assert_eq!(r.tool_result_events.len(), 0);
}

// ---------------------------------------------------------------------------
// Incremental dedup
// ---------------------------------------------------------------------------

#[test]
fn no_duplicate_event_tuples_after_re_parse() {
    let path = fixture("user-turn-blocks.jsonl");
    let a =
        parse_codex_session_incremental(&path, &ParseCodexIncrementalOptions::default()).unwrap();
    let b =
        parse_codex_session_incremental(&path, &ParseCodexIncrementalOptions::default()).unwrap();
    let key =
        |e: &ToolResultEventRecord| format!("{}|{}|{}", e.session_id, e.tool_use_id, e.event_index);
    let mut a_keys: Vec<String> = a.tool_result_events.iter().map(key).collect();
    let mut b_keys: Vec<String> = b.tool_result_events.iter().map(key).collect();
    a_keys.sort();
    b_keys.sort();
    assert_eq!(a_keys, b_keys);
}

#[test]
fn does_not_double_emit_relationships_or_events_across_resumes() {
    let path = fixture("user-turn-blocks.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = raw.split('\n').collect();
    let cut_idx = lines
        .iter()
        .position(|l| l.contains("\"task_complete\"") && l.contains("\"turn_utb_2\""))
        .unwrap();
    let cutoff: usize = (lines[..=cut_idx].join("\n") + "\n").len();

    let tmp = tempdir().unwrap();
    let partial_path = tmp.path().join("partial.jsonl");
    std::fs::write(&partial_path, &raw[..cutoff]).unwrap();
    let partial =
        parse_codex_session_incremental(&partial_path, &ParseCodexIncrementalOptions::default())
            .unwrap();
    let partial_roots = partial
        .relationships
        .iter()
        .filter(|r| matches!(r.relationship_type, RelationshipType::Root))
        .count();
    assert_eq!(partial_roots, 1);
    let mut partial_event_ids: Vec<&str> = partial
        .tool_result_events
        .iter()
        .map(|e| e.tool_use_id.as_str())
        .collect();
    partial_event_ids.sort();
    assert_eq!(partial_event_ids, vec!["call_b1", "call_b2", "call_p1"]);

    let full_path = tmp.path().join("full.jsonl");
    std::fs::write(&full_path, &raw).unwrap();
    let resumed = parse_codex_session_incremental(
        &full_path,
        &ParseCodexIncrementalOptions {
            start_offset: Some(partial.end_offset),
            resume: Some(partial.resume.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let resumed_roots = resumed
        .relationships
        .iter()
        .filter(|r| matches!(r.relationship_type, RelationshipType::Root))
        .count();
    assert_eq!(resumed_roots, 0);
    let resumed_event_ids: Vec<&str> = resumed
        .tool_result_events
        .iter()
        .map(|e| e.tool_use_id.as_str())
        .collect();
    assert_eq!(resumed_event_ids, vec!["call_b3"]);

    let mut indices: Vec<u64> = partial
        .tool_result_events
        .iter()
        .chain(resumed.tool_result_events.iter())
        .map(|e| e.event_index)
        .collect();
    let sorted: Vec<u64> = {
        let mut s = indices.clone();
        s.sort();
        s
    };
    assert_eq!(indices, sorted);
    indices.sort();
    indices.dedup();
    assert_eq!(
        indices.len(),
        partial.tool_result_events.len() + resumed.tool_result_events.len()
    );
}

#[test]
fn duplicate_session_meta_does_not_re_emit_metadata_edges() {
    let tmp = tempdir().unwrap();
    let first_meta = json!({"timestamp":"2026-04-24T00:00:00.000Z","type":"session_meta","payload":{"id":"sess_meta_resume","cwd":"/tmp/project","timestamp":"2026-04-24T00:00:00.000Z","cli_version":"0.130.0","sourceSessionId":"sess_source","forkSessionId":"sess_fork","continuedFromSessionId":"sess_prev"}});
    let dup_meta = json!({"timestamp":"2026-04-24T00:00:01.000Z","type":"session_meta","payload":{"id":"sess_meta_resume","cwd":"/tmp/project","timestamp":"2026-04-24T00:00:01.000Z","cli_version":"0.130.0","sourceSessionId":"sess_source","forkSessionId":"sess_fork","continuedFromSessionId":"sess_prev"}});
    let first_lines = vec![
        first_meta,
        json!({"timestamp":"2026-04-24T00:00:00.050Z","type":"turn_context","payload":{"turn_id":"turn_meta_a","cwd":"/tmp/project","model":"gpt-5.4"}}),
        json!({"timestamp":"2026-04-24T00:00:00.100Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn_meta_a"}}),
        json!({"timestamp":"2026-04-24T00:00:00.200Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10}}}}),
        json!({"timestamp":"2026-04-24T00:00:00.300Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn_meta_a"}}),
    ];
    let second_lines = vec![
        dup_meta,
        json!({"timestamp":"2026-04-24T00:00:01.050Z","type":"turn_context","payload":{"turn_id":"turn_meta_b","cwd":"/tmp/project","model":"gpt-5.4"}}),
        json!({"timestamp":"2026-04-24T00:00:01.100Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn_meta_b"}}),
        json!({"timestamp":"2026-04-24T00:00:01.200Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":150,"cached_input_tokens":0,"output_tokens":20}}}}),
        json!({"timestamp":"2026-04-24T00:00:01.300Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn_meta_b"}}),
    ];
    let path = write_jsonl(tmp.path(), "session.jsonl", &first_lines);
    let first =
        parse_codex_session_incremental(&path, &ParseCodexIncrementalOptions::default()).unwrap();
    let fork_first = first
        .relationships
        .iter()
        .filter(|r| matches!(r.relationship_type, RelationshipType::Fork))
        .count();
    let cont_first = first
        .relationships
        .iter()
        .filter(|r| matches!(r.relationship_type, RelationshipType::Continuation))
        .count();
    assert_eq!(fork_first, 1);
    assert_eq!(cont_first, 1);

    let mut combined = first_lines.clone();
    combined.extend(second_lines);
    let path = write_jsonl(tmp.path(), "session.jsonl", &combined);
    let second = parse_codex_session_incremental(
        &path,
        &ParseCodexIncrementalOptions {
            start_offset: Some(first.end_offset),
            resume: Some(first.resume.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let any_meta = second.relationships.iter().any(|r| {
        matches!(r.relationship_type, RelationshipType::Fork)
            || matches!(r.relationship_type, RelationshipType::Continuation)
    });
    assert!(!any_meta, "duplicate session_meta should not re-emit edges");
}

// ---------------------------------------------------------------------------
// Fidelity (issue #84)
// ---------------------------------------------------------------------------

#[test]
fn fidelity_full_when_token_count_present() {
    let r = parse("simple-turn.jsonl");
    let f = r.turns[0].fidelity.as_ref().unwrap();
    assert!(matches!(f.granularity, UsageGranularity::PerTurn));
    assert!(f.coverage.has_input_tokens);
    assert!(f.coverage.has_output_tokens);
    assert!(f.coverage.has_reasoning_tokens);
    assert!(f.coverage.has_cache_read_tokens);
    assert!(!f.coverage.has_cache_create_tokens);
    assert!(f.coverage.has_tool_calls);
    assert!(f.coverage.has_tool_result_events);
    assert!(f.coverage.has_session_relationships);
    assert!(f.coverage.has_raw_content);
    assert!(matches!(f.class, FidelityClass::Full));
}

#[test]
fn fidelity_partial_when_token_count_absent() {
    let tmp = tempdir().unwrap();
    let path = write_jsonl(
        tmp.path(),
        "no-tc.jsonl",
        &[
            json!({"timestamp":"2026-04-22T00:00:00.000Z","type":"session_meta","payload":{"id":"sess_fid_no_tc","cwd":"/tmp/proj","timestamp":"2026-04-22T00:00:00.000Z"}}),
            json!({"timestamp":"2026-04-22T00:00:00.050Z","type":"turn_context","payload":{"turn_id":"turn_fid_no_tc","cwd":"/tmp/proj","model":"gpt-5.4"}}),
            json!({"timestamp":"2026-04-22T00:00:00.200Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn_fid_no_tc"}}),
            json!({"timestamp":"2026-04-22T00:00:00.300Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}),
            json!({"timestamp":"2026-04-22T00:00:00.500Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn_fid_no_tc"}}),
        ],
    );
    let r = parse_codex_session(&path, &ParseCodexOptions::default()).unwrap();
    assert_eq!(r.turns.len(), 1);
    let t = &r.turns[0];
    let f = t.fidelity.as_ref().unwrap();
    assert!(matches!(f.granularity, UsageGranularity::PerTurn));
    assert!(matches!(f.class, FidelityClass::Partial));
    assert!(!f.coverage.has_input_tokens);
    assert!(!f.coverage.has_output_tokens);
    assert!(!f.coverage.has_reasoning_tokens);
    assert!(!f.coverage.has_cache_read_tokens);
    assert!(f.coverage.has_tool_calls);
    assert!(f.coverage.has_tool_result_events);
    assert!(f.coverage.has_raw_content);
    assert_eq!(t.usage.input, 0);
    assert_eq!(t.usage.output, 0);
}

#[test]
fn every_turn_carries_fidelity() {
    let r = parse("multi-turn.jsonl");
    assert!(!r.turns.is_empty());
    for t in &r.turns {
        let f = t.fidelity.as_ref().unwrap();
        assert!(matches!(f.granularity, UsageGranularity::PerTurn));
    }
}

#[test]
fn incremental_populates_fidelity() {
    let path = fixture("multi-turn.jsonl");
    let r =
        parse_codex_session_incremental(&path, &ParseCodexIncrementalOptions::default()).unwrap();
    assert!(!r.turns.is_empty());
    for t in &r.turns {
        let f = t.fidelity.as_ref().unwrap();
        assert!(matches!(f.granularity, UsageGranularity::PerTurn));
    }
}

// ---------------------------------------------------------------------------
// User-turn dedup
// ---------------------------------------------------------------------------

#[test]
fn user_turns_emitted_once_across_resumed_passes() {
    let path = fixture("user-turn-blocks.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = raw.split('\n').collect();
    let cut_idx = lines
        .iter()
        .position(|l| l.contains("\"task_complete\"") && l.contains("\"turn_utb_2\""))
        .unwrap();
    let cutoff: usize = (lines[..=cut_idx].join("\n") + "\n").len();

    let tmp = tempdir().unwrap();
    let partial_path = tmp.path().join("partial.jsonl");
    std::fs::write(&partial_path, &raw[..cutoff]).unwrap();
    let partial =
        parse_codex_session_incremental(&partial_path, &ParseCodexIncrementalOptions::default())
            .unwrap();
    assert_eq!(partial.user_turns.len(), 2);
    assert_eq!(partial.end_offset as usize, cutoff);

    let full_path = tmp.path().join("full.jsonl");
    std::fs::write(&full_path, &raw).unwrap();
    let resumed = parse_codex_session_incremental(
        &full_path,
        &ParseCodexIncrementalOptions {
            start_offset: Some(partial.end_offset),
            resume: Some(partial.resume.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(resumed.user_turns.len(), 1);
    let between23 = &resumed.user_turns[0];
    assert_eq!(
        between23.preceding_message_id.as_deref(),
        Some("turn_utb_2")
    );
    assert_eq!(
        between23.following_message_id.as_deref(),
        Some("turn_utb_3")
    );
    assert_eq!(between23.blocks.len(), 2);
    let fail_block = between23
        .blocks
        .iter()
        .find(|b| b.tool_use_id.as_deref() == Some("call_b2"))
        .unwrap();
    assert_eq!(fail_block.is_error, Some(true));

    let full_pass = parse_codex_session(&full_path, &ParseCodexOptions::default()).unwrap();
    let mut combined: Vec<String> = partial
        .user_turns
        .iter()
        .chain(resumed.user_turns.iter())
        .map(|u| u.user_uuid.clone())
        .collect();
    combined.sort();
    let mut full_ids: Vec<String> = full_pass
        .user_turns
        .iter()
        .map(|u| u.user_uuid.clone())
        .collect();
    full_ids.sort();
    assert_eq!(combined, full_ids);
    let unique: std::collections::HashSet<_> = combined.iter().collect();
    assert_eq!(unique.len(), combined.len());
}
