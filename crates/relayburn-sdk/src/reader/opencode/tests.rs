//! OpenCode parser conformance tests. The bundled fixtures live at the repo
//! root (`tests/fixtures/opencode/*`) and are shared with the TS test suite —
//! mirroring the expectations in `packages/reader/src/opencode.test.ts`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::tempdir;

use super::*;
use crate::reader::types::{
    ContentKind, ContentRole, FidelityClass, RelationshipType, SourceKind, ToolResultEventSource,
    ToolResultStatus, UsageAttribution, UsageGranularity, UserTurnBlockKind,
};

fn fixtures_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/relayburn-reader/ -> repo root
    p.pop();
    p.pop();
    p.push("tests/fixtures/opencode");
    p
}

fn session_file(fixture: &str, session_id: &str) -> PathBuf {
    let mut p = fixtures_root();
    p.push(fixture);
    p.push("storage/session/global");
    p.push(format!("{}.json", session_id));
    p
}

fn parse(fixture: &str, session_id: &str) -> ParseOpencodeResult {
    parse_opencode_session(
        session_file(fixture, session_id),
        &ParseOpencodeOptions::default(),
    )
    .unwrap()
}

fn parse_with(fixture: &str, session_id: &str, opts: ParseOpencodeOptions) -> ParseOpencodeResult {
    parse_opencode_session(session_file(fixture, session_id), &opts).unwrap()
}

// ---------------------------------------------------------------------------
// parseOpencodeSession — basic shapes
// ---------------------------------------------------------------------------

#[test]
fn parses_simple_one_turn_session() {
    let r = parse("simple", "ses_simple");
    assert_eq!(r.turns.len(), 1);
    let t = &r.turns[0];
    assert_eq!(t.v, 1);
    assert!(matches!(t.source, SourceKind::Opencode));
    assert_eq!(t.session_id, "ses_simple");
    assert_eq!(t.message_id, "msg_simple_asst");
    assert_eq!(t.turn_index, 0);
    assert_eq!(t.model, "anthropic/claude-sonnet-4-5");
    assert_eq!(t.project.as_deref(), Some("/tmp/project"));
    assert_eq!(t.ts, "2026-04-24T00:00:02.000Z");
    assert_eq!(t.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(
        t.usage,
        Usage {
            input: 10,
            output: 5,
            reasoning: 0,
            cache_read: 500,
            cache_create_5m: 80,
            cache_create_1h: 0,
        },
    );
    assert_eq!(t.tool_calls.len(), 0);
    assert!(t.files_touched.is_none());
    assert!(t.subagent.is_none());
}

#[test]
fn extracts_tool_calls_and_files_touched_for_file_tools() {
    let r = parse("with-tool", "ses_tool");
    assert_eq!(r.turns.len(), 1);
    let t = &r.turns[0];
    assert_eq!(t.tool_calls.len(), 3);
    let read = &t.tool_calls[0];
    let edit = &t.tool_calls[1];
    let bash = &t.tool_calls[2];
    assert_eq!(read.name, "read");
    assert_eq!(read.target.as_deref(), Some("/src/a.ts"));
    assert_eq!(edit.name, "edit");
    assert_eq!(edit.target.as_deref(), Some("/src/b.ts"));
    assert_eq!(bash.name, "bash");
    assert_eq!(bash.target.as_deref(), Some("ls -la"));
    let mut files = t.files_touched.clone().unwrap();
    files.sort();
    assert_eq!(
        files,
        vec!["/src/a.ts".to_string(), "/src/b.ts".to_string()]
    );
    assert_eq!(t.stop_reason.as_deref(), Some("tool-calls"));
}

#[test]
fn emits_per_turn_usage_across_multiple_turns() {
    let r = parse("multi-turn", "ses_multi");
    assert_eq!(r.turns.len(), 2);
    let t1 = &r.turns[0];
    let t2 = &r.turns[1];
    assert_eq!(t1.message_id, "msg_multi_a1");
    assert_eq!(t1.turn_index, 0);
    assert_eq!(t1.model, "anthropic/claude-sonnet-4-5");
    assert_eq!(
        t1.usage,
        Usage {
            input: 5,
            output: 100,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 15000,
            cache_create_1h: 0,
        },
    );
    assert!(t1.subagent.is_none());

    assert_eq!(t2.message_id, "msg_multi_a2");
    assert_eq!(t2.turn_index, 1);
    assert_eq!(t2.model, "anthropic/claude-opus-4-5");
    assert_eq!(
        t2.usage,
        Usage {
            input: 5,
            output: 200,
            reasoning: 50,
            cache_read: 15000,
            cache_create_5m: 3000,
            cache_create_1h: 0,
        },
    );
    assert_eq!(t2.tool_calls.len(), 1);
    assert_eq!(t2.tool_calls[0].name, "bash");
    assert_eq!(t2.tool_calls[0].target.as_deref(), Some("git status"));
}

#[test]
fn marks_sidechain_for_session_with_parent_id() {
    let r = parse("multi-turn", "ses_child");
    assert_eq!(r.turns.len(), 1);
    let t = &r.turns[0];
    let sub = t.subagent.as_ref().expect("subagent populated");
    assert!(sub.is_sidechain);
    assert_eq!(t.model, "anthropic/claude-haiku-4-5");
}

#[test]
fn produces_stable_args_hash_for_identical_inputs() {
    let a = parse("with-tool", "ses_tool");
    let b = parse("with-tool", "ses_tool");
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
    let path = session_file("simple", "ses_simple");
    let path_str = path.to_string_lossy().to_string();
    let r = parse_with(
        "simple",
        "ses_simple",
        ParseOpencodeOptions {
            session_path: Some(path_str.clone()),
            ..ParseOpencodeOptions::default()
        },
    );
    assert_eq!(r.turns[0].session_path.as_deref(), Some(path_str.as_str()));
}

#[test]
fn classifies_activity_via_aliased_tool_names() {
    let r = parse("with-tool", "ses_tool");
    let t = &r.turns[0];
    assert_eq!(t.has_edits, Some(true));
    assert_eq!(t.activity.unwrap(), crate::reader::types::ActivityCategory::Coding);
}

// ---------------------------------------------------------------------------
// parseOpencodeSessionIncremental
// ---------------------------------------------------------------------------

#[test]
fn incremental_returns_all_turns_when_seen_empty() {
    let path = session_file("multi-turn", "ses_multi");
    let r = parse_opencode_session_incremental(&path, &ParseOpencodeIncrementalOptions::default())
        .unwrap();
    assert_eq!(r.turns.len(), 2);
    assert!(r.seen_message_ids.contains("msg_multi_a1"));
    assert!(r.seen_message_ids.contains("msg_multi_a2"));
}

#[test]
fn incremental_filters_already_seen_message_ids() {
    let path = session_file("multi-turn", "ses_multi");
    let mut seen = BTreeSet::new();
    seen.insert("msg_multi_a1".to_string());
    let r = parse_opencode_session_incremental(
        &path,
        &ParseOpencodeIncrementalOptions {
            seen_message_ids: Some(seen),
            ..ParseOpencodeIncrementalOptions::default()
        },
    )
    .unwrap();
    assert_eq!(r.turns.len(), 1);
    assert_eq!(r.turns[0].message_id, "msg_multi_a2");
    assert!(r.seen_message_ids.contains("msg_multi_a1"));
    assert!(r.seen_message_ids.contains("msg_multi_a2"));
}

#[test]
fn incremental_yields_zero_turns_when_all_seen() {
    let path = session_file("multi-turn", "ses_multi");
    let mut seen = BTreeSet::new();
    seen.insert("msg_multi_a1".to_string());
    seen.insert("msg_multi_a2".to_string());
    let r = parse_opencode_session_incremental(
        &path,
        &ParseOpencodeIncrementalOptions {
            seen_message_ids: Some(seen),
            ..ParseOpencodeIncrementalOptions::default()
        },
    )
    .unwrap();
    assert_eq!(r.turns.len(), 0);
}

// ---------------------------------------------------------------------------
// Errored tool call ⇒ debugging
// ---------------------------------------------------------------------------

fn write_json(p: &Path, body: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn errored_tool_part_marks_turn_as_debugging() {
    let tmp = tempdir().unwrap();
    let storage = tmp.path().join("storage");
    let session_dir = storage.join("session/global");
    let msg_dir = storage.join("message/ses_fail");
    let part_asst = storage.join("part/msg_fail_asst");
    let part_user = storage.join("part/msg_fail_user");

    write_json(
        &session_dir.join("ses_fail.json"),
        r#"{"id":"ses_fail","directory":"/tmp/proj"}"#,
    );
    write_json(
        &msg_dir.join("msg_fail_user.json"),
        r#"{"id":"msg_fail_user","sessionID":"ses_fail","role":"user","time":{"created":1776988000000}}"#,
    );
    write_json(
        &part_user.join("prt_fail_user_1.json"),
        r#"{"id":"prt_fail_user_1","sessionID":"ses_fail","messageID":"msg_fail_user","type":"text","text":"please check why the build is broken"}"#,
    );
    write_json(
        &msg_dir.join("msg_fail_asst.json"),
        r#"{"id":"msg_fail_asst","sessionID":"ses_fail","role":"assistant","providerID":"anthropic","modelID":"claude-haiku-4-5","time":{"created":1776988001000},"path":{"cwd":"/tmp/proj"},"tokens":{"input":10,"output":20,"cache":{"read":0,"write":0}}}"#,
    );
    write_json(
        &part_asst.join("prt_fail_asst_1.json"),
        r#"{"id":"prt_fail_asst_1","sessionID":"ses_fail","messageID":"msg_fail_asst","type":"tool","callID":"call_fail_bash","tool":"bash","state":{"status":"completed","input":{"command":"npm run build"},"output":"command not found: foo","metadata":{"exit":1}}}"#,
    );

    let file = session_dir.join("ses_fail.json");
    let r = parse_opencode_session(&file, &ParseOpencodeOptions::default()).unwrap();
    assert_eq!(r.turns.len(), 1);
    let t = &r.turns[0];
    assert_eq!(
        t.activity.unwrap(),
        crate::reader::types::ActivityCategory::Debugging
    );
    assert_eq!(t.has_edits, Some(false));
}

// ---------------------------------------------------------------------------
// Compaction events
// ---------------------------------------------------------------------------

#[test]
fn emits_compaction_event_anchored_to_preceding_turn() {
    let r = parse("with-compaction", "ses_compact");
    assert_eq!(r.turns.len(), 3);
    assert_eq!(r.events.len(), 1);
    let ev = &r.events[0];
    assert_eq!(ev.v, 1);
    assert!(matches!(ev.source, SourceKind::Opencode));
    assert_eq!(ev.session_id, "ses_compact");
    assert_eq!(ev.ts, "2026-04-24T02:50:03.000Z");
    assert_eq!(ev.preceding_message_id.as_deref(), Some("msg_compact_a1"));
    let preceding = r
        .turns
        .iter()
        .find(|t| t.message_id == "msg_compact_a1")
        .unwrap();
    assert_eq!(ev.tokens_before_compact, Some(preceding.usage.cache_read));
    assert_eq!(ev.tokens_before_compact, Some(12000));
}

#[test]
fn does_not_re_emit_compaction_when_user_id_seen() {
    let path = session_file("with-compaction", "ses_compact");
    let mut seen = BTreeSet::new();
    seen.insert("msg_compact_a1".to_string());
    seen.insert("msg_compact_summary".to_string());
    seen.insert("msg_compact_uc".to_string());
    let r = parse_opencode_session_incremental(
        &path,
        &ParseOpencodeIncrementalOptions {
            seen_message_ids: Some(seen),
            ..ParseOpencodeIncrementalOptions::default()
        },
    )
    .unwrap();
    assert_eq!(r.events.len(), 0);
    assert_eq!(r.turns.len(), 1);
    assert_eq!(r.turns[0].message_id, "msg_compact_a2");
}

// ---------------------------------------------------------------------------
// Content capture
// ---------------------------------------------------------------------------

fn with_content_fixture<F: FnOnce(&Path)>(body: F) {
    let tmp = tempdir().unwrap();
    let storage = tmp.path().join("storage");
    let session_dir = storage.join("session/global");
    let msg_dir = storage.join("message/ses_content");
    let part_asst = storage.join("part/msg_content_asst");
    let part_user = storage.join("part/msg_content_user");

    write_json(
        &session_dir.join("ses_content.json"),
        r#"{"id":"ses_content","directory":"/tmp/proj"}"#,
    );
    write_json(
        &msg_dir.join("msg_content_user.json"),
        r#"{"id":"msg_content_user","sessionID":"ses_content","role":"user","time":{"created":1776988000000}}"#,
    );
    write_json(
        &part_user.join("prt_user_a.json"),
        r#"{"id":"prt_user_a","sessionID":"ses_content","messageID":"msg_content_user","type":"text","text":"run tests"}"#,
    );
    write_json(
        &part_user.join("prt_user_b.json"),
        r#"{"id":"prt_user_b","sessionID":"ses_content","messageID":"msg_content_user","type":"text","text":"<synthetic nudge>","synthetic":true}"#,
    );
    write_json(
        &msg_dir.join("msg_content_asst.json"),
        r#"{"id":"msg_content_asst","sessionID":"ses_content","role":"assistant","providerID":"anthropic","modelID":"claude-sonnet-4-6","time":{"created":1776988001000},"path":{"cwd":"/tmp/proj"},"tokens":{"input":100,"output":20,"cache":{"read":0,"write":0}}}"#,
    );
    write_json(
        &part_asst.join("prt_asst_a.json"),
        r#"{"id":"prt_asst_a","sessionID":"ses_content","messageID":"msg_content_asst","type":"text","text":"running now."}"#,
    );
    write_json(
        &part_asst.join("prt_asst_b.json"),
        r#"{"id":"prt_asst_b","sessionID":"ses_content","messageID":"msg_content_asst","type":"tool","callID":"call_oc_bash","tool":"bash","state":{"status":"completed","input":{"command":"npm test"},"output":"10 passed","metadata":{"exit":0}}}"#,
    );
    write_json(
        &part_asst.join("prt_asst_c.json"),
        r#"{"id":"prt_asst_c","sessionID":"ses_content","messageID":"msg_content_asst","type":"tool","callID":"call_oc_fail","tool":"bash","state":{"status":"completed","input":{"command":"lint"},"output":"ERR","metadata":{"exit":2}}}"#,
    );
    body(&session_dir.join("ses_content.json"));
}

#[test]
fn content_default_off() {
    with_content_fixture(|file| {
        let r = parse_opencode_session(file, &ParseOpencodeOptions::default()).unwrap();
        assert!(r.content.is_empty());
    });
}

#[test]
fn content_hash_only_returns_empty() {
    with_content_fixture(|file| {
        let r = parse_opencode_session(
            file,
            &ParseOpencodeOptions {
                content_mode: Some(ContentStoreMode::HashOnly),
                ..ParseOpencodeOptions::default()
            },
        )
        .unwrap();
        assert!(r.content.is_empty());
    });
}

#[test]
fn content_full_emits_tool_results_keyed_by_call_id() {
    with_content_fixture(|file| {
        let r = parse_opencode_session(
            file,
            &ParseOpencodeOptions {
                content_mode: Some(ContentStoreMode::Full),
                ..ParseOpencodeOptions::default()
            },
        )
        .unwrap();
        assert_eq!(r.turns.len(), 1);
        let tool_results: Vec<_> = r
            .content
            .iter()
            .filter(|c| matches!(c.kind, ContentKind::ToolResult))
            .collect();
        assert_eq!(tool_results.len(), 2);
        let bash = tool_results
            .iter()
            .find(|c| c.tool_result.as_ref().unwrap().tool_use_id == "call_oc_bash")
            .unwrap();
        assert_eq!(
            bash.tool_result.as_ref().unwrap().content.as_str(),
            Some("10 passed"),
        );
        assert_eq!(bash.tool_result.as_ref().unwrap().is_error, None);
        let fail = tool_results
            .iter()
            .find(|c| c.tool_result.as_ref().unwrap().tool_use_id == "call_oc_fail")
            .unwrap();
        assert_eq!(
            fail.tool_result.as_ref().unwrap().content.as_str(),
            Some("ERR"),
        );
        assert_eq!(fail.tool_result.as_ref().unwrap().is_error, Some(true));
        let turn_tool_ids: BTreeSet<_> = r.turns[0]
            .tool_calls
            .iter()
            .map(|tc| tc.id.clone())
            .collect();
        assert!(turn_tool_ids.contains("call_oc_bash"));
        assert!(turn_tool_ids.contains("call_oc_fail"));
    });
}

#[test]
fn content_full_captures_user_text_skipping_synthetic() {
    with_content_fixture(|file| {
        let r = parse_opencode_session(
            file,
            &ParseOpencodeOptions {
                content_mode: Some(ContentStoreMode::Full),
                ..ParseOpencodeOptions::default()
            },
        )
        .unwrap();
        let user_texts: Vec<_> = r
            .content
            .iter()
            .filter(|c| matches!(c.role, ContentRole::User) && matches!(c.kind, ContentKind::Text))
            .map(|c| c.text.clone().unwrap_or_default())
            .collect();
        assert_eq!(user_texts, vec!["run tests".to_string()]);
        let asst_text = r
            .content
            .iter()
            .find(|c| {
                matches!(c.role, ContentRole::Assistant) && matches!(c.kind, ContentKind::Text)
            })
            .unwrap();
        assert_eq!(asst_text.text.as_deref(), Some("running now."));
        let tool_uses = r
            .content
            .iter()
            .filter(|c| matches!(c.kind, ContentKind::ToolUse))
            .count();
        assert_eq!(tool_uses, 2);
    });
}

// ---------------------------------------------------------------------------
// Fidelity (issue #89)
// ---------------------------------------------------------------------------

#[test]
fn fidelity_full_when_tokens_fully_populated() {
    let r = parse("simple", "ses_simple");
    let t = &r.turns[0];
    let f = t.fidelity.as_ref().unwrap();
    assert!(matches!(f.granularity, UsageGranularity::PerTurn));
    assert!(matches!(f.class, FidelityClass::Full));
    assert!(f.coverage.has_input_tokens);
    assert!(f.coverage.has_output_tokens);
    assert!(f.coverage.has_reasoning_tokens);
    assert!(f.coverage.has_cache_read_tokens);
    assert!(f.coverage.has_cache_create_tokens);
    assert!(f.coverage.has_tool_calls);
    assert!(f.coverage.has_tool_result_events);
    assert!(f.coverage.has_session_relationships);
    assert!(f.coverage.has_raw_content);
}

#[test]
fn fidelity_partial_when_no_tokens_block() {
    let tmp = tempdir().unwrap();
    let storage = tmp.path().join("storage");
    let session_dir = storage.join("session/global");
    let msg_dir = storage.join("message/ses_no_tokens");
    fs::create_dir_all(&session_dir).unwrap();
    fs::create_dir_all(&msg_dir).unwrap();
    fs::create_dir_all(storage.join("part/msg_no_tokens_asst")).unwrap();
    write_json(
        &session_dir.join("ses_no_tokens.json"),
        r#"{"id":"ses_no_tokens","directory":"/tmp/proj"}"#,
    );
    write_json(
        &msg_dir.join("msg_no_tokens_asst.json"),
        r#"{"id":"msg_no_tokens_asst","sessionID":"ses_no_tokens","role":"assistant","providerID":"anthropic","modelID":"claude-haiku-4-5","time":{"created":1776988001000},"path":{"cwd":"/tmp/proj"}}"#,
    );
    let file = session_dir.join("ses_no_tokens.json");
    let r = parse_opencode_session(&file, &ParseOpencodeOptions::default()).unwrap();
    assert_eq!(r.turns.len(), 1);
    let f = r.turns[0].fidelity.as_ref().unwrap();
    assert!(matches!(f.granularity, UsageGranularity::PerTurn));
    assert!(matches!(f.class, FidelityClass::Partial));
    assert!(!f.coverage.has_input_tokens);
    assert!(!f.coverage.has_output_tokens);
    assert!(!f.coverage.has_reasoning_tokens);
    assert!(!f.coverage.has_cache_read_tokens);
    assert!(!f.coverage.has_cache_create_tokens);
    assert!(f.coverage.has_tool_calls);
    assert!(f.coverage.has_tool_result_events);
    assert!(f.coverage.has_session_relationships);
    assert!(f.coverage.has_raw_content);
}

#[test]
fn fidelity_flips_cache_flags_when_present() {
    let tmp = tempdir().unwrap();
    let storage = tmp.path().join("storage");
    let session_dir = storage.join("session/global");
    let msg_dir = storage.join("message/ses_cache");
    fs::create_dir_all(&session_dir).unwrap();
    fs::create_dir_all(&msg_dir).unwrap();
    fs::create_dir_all(storage.join("part/msg_cache_asst")).unwrap();
    write_json(
        &session_dir.join("ses_cache.json"),
        r#"{"id":"ses_cache","directory":"/tmp/proj"}"#,
    );
    write_json(
        &msg_dir.join("msg_cache_asst.json"),
        r#"{"id":"msg_cache_asst","sessionID":"ses_cache","role":"assistant","providerID":"anthropic","modelID":"claude-sonnet-4-5","time":{"created":1776988001000},"path":{"cwd":"/tmp/proj"},"tokens":{"input":100,"output":50,"cache":{"read":12000,"write":800}}}"#,
    );
    let r = parse_opencode_session(
        session_dir.join("ses_cache.json"),
        &ParseOpencodeOptions::default(),
    )
    .unwrap();
    assert_eq!(r.turns.len(), 1);
    let f = r.turns[0].fidelity.as_ref().unwrap();
    assert!(f.coverage.has_cache_read_tokens);
    assert!(f.coverage.has_cache_create_tokens);
    assert!(!f.coverage.has_reasoning_tokens);
    assert_eq!(r.turns[0].usage.reasoning, 0);
    assert!(matches!(f.class, FidelityClass::Full));
}

#[test]
fn every_emitted_turn_carries_fidelity() {
    let r = parse("multi-turn", "ses_multi");
    assert!(!r.turns.is_empty());
    let unknown = r.turns.iter().filter(|t| t.fidelity.is_none()).count();
    assert_eq!(unknown, 0);
    for t in &r.turns {
        assert!(matches!(
            t.fidelity.as_ref().unwrap().granularity,
            UsageGranularity::PerTurn,
        ));
    }
}

#[test]
fn fidelity_rolls_up_step_finish_coverage() {
    let tmp = tempdir().unwrap();
    let storage = tmp.path().join("storage");
    let session_dir = storage.join("session/global");
    let msg_dir = storage.join("message/ses_sf");
    let part_asst = storage.join("part/msg_sf_asst");
    fs::create_dir_all(&session_dir).unwrap();
    fs::create_dir_all(&msg_dir).unwrap();
    fs::create_dir_all(&part_asst).unwrap();
    write_json(
        &session_dir.join("ses_sf.json"),
        r#"{"id":"ses_sf","directory":"/tmp/proj"}"#,
    );
    write_json(
        &msg_dir.join("msg_sf_asst.json"),
        r#"{"id":"msg_sf_asst","sessionID":"ses_sf","role":"assistant","providerID":"anthropic","modelID":"claude-sonnet-4-5","time":{"created":1776988001000},"path":{"cwd":"/tmp/proj"},"tokens":{"input":5,"output":3}}"#,
    );
    write_json(
        &part_asst.join("prt_sf_1.json"),
        r#"{"id":"prt_sf_1","sessionID":"ses_sf","messageID":"msg_sf_asst","type":"step-finish","reason":"end_turn","tokens":{"input":5,"output":3,"cache":{"read":1000,"write":200}}}"#,
    );
    let r = parse_opencode_session(
        session_dir.join("ses_sf.json"),
        &ParseOpencodeOptions::default(),
    )
    .unwrap();
    assert_eq!(r.turns.len(), 1);
    let f = r.turns[0].fidelity.as_ref().unwrap();
    assert!(f.coverage.has_cache_read_tokens);
    assert!(f.coverage.has_cache_create_tokens);
}

// ---------------------------------------------------------------------------
// User-turn block sizes (issue #86)
// ---------------------------------------------------------------------------

#[test]
fn user_turn_blocks_one_per_gap() {
    let r = parse("user-turn-blocks", "ses_utb");
    assert_eq!(r.turns.len(), 2);
    assert_eq!(r.user_turns.len(), 2);

    for u in &r.user_turns {
        assert_eq!(u.v, 1);
        assert!(matches!(u.source, SourceKind::Opencode));
        assert_eq!(u.session_id, "ses_utb");
        assert!(!u.user_uuid.is_empty());
        assert!(!u.ts.is_empty());
        assert!(!u.blocks.is_empty());
    }

    let pre = &r.user_turns[0];
    let between = &r.user_turns[1];

    assert!(pre.preceding_message_id.is_none());
    assert_eq!(pre.following_message_id.as_deref(), Some("msg_utb_a1"));
    assert_eq!(pre.user_uuid, "msg_utb_u1");
    assert_eq!(pre.blocks.len(), 1);
    assert!(matches!(pre.blocks[0].kind, UserTurnBlockKind::Text));
    assert_eq!(pre.blocks[0].byte_len, "fix the build".len() as u64);
    // Heuristic counter (bytes/4 ceil): ceil(13/4) = 4. Note: the TS test uses
    // cl100k which yields 3 here; the Rust port currently only ships heuristic.
    assert_eq!(pre.blocks[0].approx_tokens, 4);

    assert_eq!(between.preceding_message_id.as_deref(), Some("msg_utb_a1"));
    assert_eq!(between.following_message_id.as_deref(), Some("msg_utb_a2"));
    assert_eq!(between.user_uuid, "msg_utb_u2");
    assert_eq!(between.blocks.len(), 3);

    let ok_block = between
        .blocks
        .iter()
        .find(|b| b.tool_use_id.as_deref() == Some("call_b1"))
        .unwrap();
    assert!(matches!(ok_block.kind, UserTurnBlockKind::ToolResult));
    assert_eq!(ok_block.byte_len, "ok\n".len() as u64);
    assert_eq!(ok_block.is_error, None);

    let fail_block = between
        .blocks
        .iter()
        .find(|b| b.tool_use_id.as_deref() == Some("call_fail"))
        .unwrap();
    assert!(matches!(fail_block.kind, UserTurnBlockKind::ToolResult));
    assert_eq!(fail_block.byte_len, "ERROR: tests failed".len() as u64);
    assert_eq!(fail_block.is_error, Some(true));

    let txt = between
        .blocks
        .iter()
        .find(|b| matches!(b.kind, UserTurnBlockKind::Text))
        .unwrap();
    assert_eq!(txt.byte_len, "now run tests".len() as u64);
}

#[test]
fn empty_user_turns_when_no_measurable_blocks() {
    let r = parse("multi-turn", "ses_multi");
    assert!(r.user_turns.is_empty());
}

#[test]
fn no_double_emit_user_turns_across_resumed_passes() {
    let path = session_file("user-turn-blocks", "ses_utb");
    let first =
        parse_opencode_session_incremental(&path, &ParseOpencodeIncrementalOptions::default())
            .unwrap();
    assert_eq!(first.user_turns.len(), 2);

    let mut seen = BTreeSet::new();
    seen.insert("msg_utb_a1".to_string());
    let resumed = parse_opencode_session_incremental(
        &path,
        &ParseOpencodeIncrementalOptions {
            seen_message_ids: Some(seen),
            ..ParseOpencodeIncrementalOptions::default()
        },
    )
    .unwrap();
    assert_eq!(resumed.turns.len(), 1);
    assert_eq!(resumed.turns[0].message_id, "msg_utb_a2");
    assert_eq!(resumed.user_turns.len(), 1);
    let u = &resumed.user_turns[0];
    assert_eq!(u.preceding_message_id.as_deref(), Some("msg_utb_a1"));
    assert_eq!(u.following_message_id.as_deref(), Some("msg_utb_a2"));
    assert_eq!(u.blocks.len(), 3);
}

// ---------------------------------------------------------------------------
// Execution graph (#42 / #93)
// ---------------------------------------------------------------------------

#[test]
fn relationships_one_root_for_non_subagent_session() {
    let r = parse("multi-turn", "ses_multi");
    assert_eq!(r.relationships.len(), 1);
    let root = &r.relationships[0];
    assert_eq!(root.v, 1);
    assert!(matches!(root.source, RelationshipSourceKind::Opencode));
    assert_eq!(root.session_id, "ses_multi");
    assert!(matches!(root.relationship_type, RelationshipType::Root));
    assert!(root.related_session_id.is_none());
    assert!(root.ts.is_some());
}

#[test]
fn relationships_subagent_when_parent_id_set() {
    let r = parse("multi-turn", "ses_child");
    assert_eq!(r.relationships.len(), 2);
    let sub = r
        .relationships
        .iter()
        .find(|x| matches!(x.relationship_type, RelationshipType::Subagent))
        .unwrap();
    assert!(matches!(sub.source, RelationshipSourceKind::NativeOpencode));
    assert_eq!(sub.session_id, "ses_child");
    assert_eq!(sub.related_session_id.as_deref(), Some("ses_multi"));
    let root = r
        .relationships
        .iter()
        .find(|x| matches!(x.relationship_type, RelationshipType::Root))
        .unwrap();
    assert_eq!(root.session_id, "ses_child");
}

#[test]
fn relationships_emitted_for_empty_subagent_session() {
    let tmp = tempdir().unwrap();
    let storage = tmp.path().join("storage");
    let session_dir = storage.join("session/global");
    fs::create_dir_all(&session_dir).unwrap();
    write_json(
        &session_dir.join("ses_empty_child.json"),
        r#"{"id":"ses_empty_child","parentID":"ses_parent"}"#,
    );
    let r = parse_opencode_session(
        session_dir.join("ses_empty_child.json"),
        &ParseOpencodeOptions::default(),
    )
    .unwrap();
    assert_eq!(r.turns.len(), 0);
    assert_eq!(r.relationships.len(), 2);
    let sub = r
        .relationships
        .iter()
        .find(|x| matches!(x.relationship_type, RelationshipType::Subagent))
        .unwrap();
    assert_eq!(sub.related_session_id.as_deref(), Some("ses_parent"));
    assert!(sub.ts.is_none());
}

#[test]
fn tool_result_events_per_resolved_part_with_size_and_hash() {
    let tmp = tempdir().unwrap();
    let storage = tmp.path().join("storage");
    let session_dir = storage.join("session/global");
    let msg_dir = storage.join("message/ses_tre");
    let part_asst = storage.join("part/msg_tre_asst");
    fs::create_dir_all(&session_dir).unwrap();
    fs::create_dir_all(&msg_dir).unwrap();
    fs::create_dir_all(&part_asst).unwrap();
    write_json(
        &session_dir.join("ses_tre.json"),
        r#"{"id":"ses_tre","directory":"/tmp/proj"}"#,
    );
    write_json(
        &msg_dir.join("msg_tre_asst.json"),
        r#"{"id":"msg_tre_asst","sessionID":"ses_tre","role":"assistant","providerID":"anthropic","modelID":"claude-sonnet-4-5","time":{"created":1777000000000},"tokens":{"input":5,"output":5,"cache":{"read":0,"write":0}}}"#,
    );
    write_json(
        &part_asst.join("prt_tre_a.json"),
        r#"{"id":"prt_tre_a","sessionID":"ses_tre","messageID":"msg_tre_asst","type":"tool","callID":"call_read","tool":"read","state":{"status":"completed","input":{"filePath":"/x.ts"},"output":"hello world","metadata":{}}}"#,
    );
    write_json(
        &part_asst.join("prt_tre_b.json"),
        r#"{"id":"prt_tre_b","sessionID":"ses_tre","messageID":"msg_tre_asst","type":"tool","callID":"call_err_status","tool":"webfetch","state":{"status":"error","input":{"url":"https://x"},"output":"fetch failed","metadata":{}}}"#,
    );
    write_json(
        &part_asst.join("prt_tre_c.json"),
        r#"{"id":"prt_tre_c","sessionID":"ses_tre","messageID":"msg_tre_asst","type":"tool","callID":"call_bash_exit","tool":"bash","state":{"status":"completed","input":{"command":"false"},"output":"oops","metadata":{"exit":1}}}"#,
    );
    let r = parse_opencode_session(
        session_dir.join("ses_tre.json"),
        &ParseOpencodeOptions::default(),
    )
    .unwrap();
    assert_eq!(r.tool_result_events.len(), 3);

    let ok = r
        .tool_result_events
        .iter()
        .find(|e| e.tool_use_id == "call_read")
        .unwrap();
    assert_eq!(ok.v, 1);
    assert!(matches!(ok.source, SourceKind::Opencode));
    assert_eq!(ok.session_id, "ses_tre");
    assert_eq!(ok.message_id.as_deref(), Some("msg_tre_asst"));
    assert!(matches!(ok.event_source, ToolResultEventSource::ToolResult));
    assert!(matches!(ok.status, ToolResultStatus::Completed));
    assert_eq!(ok.is_error, None);
    assert_eq!(ok.content_length, Some("hello world".len() as u64));
    assert!(ok
        .content_hash
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false));
    assert_eq!(ok.call_index, Some(0));
    assert_eq!(ok.event_index, 0);
    assert_eq!(ok.ts.as_deref(), Some("2026-04-24T03:06:40.000Z"));
    assert!(matches!(
        ok.usage_attribution,
        Some(UsageAttribution::EvenSplitTurn)
    ));
    // Three terminal tools, input usage = 5. Floor share = 1; the first
    // `5 % 3 = 2` tools get one extra so per-tool slices sum back to 5
    // (`call_read` is the first terminal in part-id order: 2 + 2 + 1 = 5).
    assert_eq!(ok.usage.as_ref().unwrap().input, 2);
    let input_sum: u64 = r
        .tool_result_events
        .iter()
        .filter_map(|e| e.usage.as_ref().map(|u| u.input))
        .sum();
    assert_eq!(input_sum, 5, "per-tool usage shares sum to the turn total");

    let err_status = r
        .tool_result_events
        .iter()
        .find(|e| e.tool_use_id == "call_err_status")
        .unwrap();
    assert!(matches!(err_status.status, ToolResultStatus::Errored));
    assert_eq!(err_status.is_error, Some(true));
    assert_eq!(err_status.content_length, Some("fetch failed".len() as u64));

    let err_exit = r
        .tool_result_events
        .iter()
        .find(|e| e.tool_use_id == "call_bash_exit")
        .unwrap();
    assert!(matches!(err_exit.status, ToolResultStatus::Errored));
    assert_eq!(err_exit.is_error, Some(true));
    assert_eq!(err_exit.content_length, Some("oops".len() as u64));

    let mut indices: Vec<u64> = r.tool_result_events.iter().map(|e| e.event_index).collect();
    let sorted = {
        let mut s = indices.clone();
        s.sort();
        s
    };
    assert_eq!(indices, sorted);
    indices.dedup();
    assert_eq!(indices.len(), r.tool_result_events.len());
}

#[test]
fn tool_result_event_hashes_structured_output() {
    let tmp = tempdir().unwrap();
    let storage = tmp.path().join("storage");
    let session_dir = storage.join("session/global");
    let msg_dir = storage.join("message/ses_struct");
    let part_asst = storage.join("part/msg_struct_asst");
    fs::create_dir_all(&session_dir).unwrap();
    fs::create_dir_all(&msg_dir).unwrap();
    fs::create_dir_all(&part_asst).unwrap();
    write_json(
        &session_dir.join("ses_struct.json"),
        r#"{"id":"ses_struct","directory":"/tmp/proj"}"#,
    );
    write_json(
        &msg_dir.join("msg_struct_asst.json"),
        r#"{"id":"msg_struct_asst","sessionID":"ses_struct","role":"assistant","modelID":"claude-sonnet-4-5","time":{"created":1777000001000},"tokens":{"input":5,"output":5,"cache":{"read":0,"write":0}}}"#,
    );
    write_json(
        &part_asst.join("prt_struct.json"),
        r#"{"id":"prt_struct","sessionID":"ses_struct","messageID":"msg_struct_asst","type":"tool","callID":"call_struct","tool":"read","state":{"status":"completed","input":{"filePath":"/x.ts"},"output":{"kind":"image","size":12},"metadata":{}}}"#,
    );
    let r = parse_opencode_session(
        session_dir.join("ses_struct.json"),
        &ParseOpencodeOptions::default(),
    )
    .unwrap();
    assert_eq!(r.tool_result_events.len(), 1);
    let ev = &r.tool_result_events[0];
    let expected = serde_json::to_string(&serde_json::json!({"kind":"image","size":12})).unwrap();
    assert_eq!(ev.content_length, Some(expected.len() as u64));
    assert!(ev
        .content_hash
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false));
}

#[test]
fn tool_result_events_no_duplicates_across_resumed_passes() {
    let path = session_file("multi-turn", "ses_multi");
    let first =
        parse_opencode_session_incremental(&path, &ParseOpencodeIncrementalOptions::default())
            .unwrap();
    let second = parse_opencode_session_incremental(
        &path,
        &ParseOpencodeIncrementalOptions {
            seen_message_ids: Some(first.seen_message_ids.clone()),
            ..ParseOpencodeIncrementalOptions::default()
        },
    )
    .unwrap();
    assert!(!first.tool_result_events.is_empty());
    assert_eq!(second.tool_result_events.len(), 0);
    assert_eq!(second.turns.len(), 0);
    assert_eq!(first.relationships.len(), 1);
    assert_eq!(second.relationships.len(), 1);
}
