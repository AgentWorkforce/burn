//! Integration tests for the gap-warning path wired in #295.
//!
//! Each test drops a fixture session with tool_use blocks but no tool_result
//! content, runs `ingest_all` (or a per-harness verb) against a temp
//! `Ledger`, and asserts that the warning fires exactly once. A second run
//! with no new affected sessions must stay silent (suppression).
//!
//! Coverage spans all three adapters (`claude`, `codex`, `opencode`) so a
//! regression in any one branch of `record_session_gap` shows up here.
//!
//! ## Concurrency / global state
//!
//! The gap tracker is process-global. Tests serialise on both `ENV_LOCK`
//! (to keep `RELAYBURN_HOME` stable) and `GAP_LOCK` (to avoid one test's
//! residual state leaking into another). Each test resets the gap state at
//! entry **and** exit via `reset_ingest_gap_warnings()` and temporarily
//! installs a buffer-backed sink via `set_ingest_gap_writer` / `restore_ingest_gap_writer`.
//!
//! `#[tokio::test]` defaults to `current_thread`, so holding a `Mutex`
//! guard across an `.await` is sound.

#![allow(clippy::await_holding_lock)]

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use relayburn_ingest::{
    ingest_all, ingest_claude_projects, ingest_codex_sessions, ingest_opencode_sessions,
    reset_ingest_gap_warnings, restore_ingest_gap_writer, set_ingest_gap_writer, IngestOptions,
    IngestRoots,
};
use relayburn_ledger::{Ledger, LedgerLayout};
use tempfile::TempDir;

/// Serialises env mutation so `RELAYBURN_HOME` is stable for each test body.
static ENV_LOCK: Mutex<()> = Mutex::new(());
/// Serialises the process-global gap state so tests don't bleed into each other.
static GAP_LOCK: Mutex<()> = Mutex::new(());

fn open_ledger_in(tmp: &TempDir) -> Ledger {
    let layout = LedgerLayout::under(tmp.path().join("ledger"));
    fs::create_dir_all(&layout.home).unwrap();
    Ledger::open(&layout.burn, &layout.content).unwrap()
}

fn isolated_relayburn_home<'a>(tmp: &TempDir) -> std::sync::MutexGuard<'a, ()> {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tmp.path().join("relayburn");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("RELAYBURN_HOME", &home);
    guard
}

fn pinned_roots(tmp: &TempDir) -> IngestRoots {
    IngestRoots {
        claude_projects_dir: Some(tmp.path().join("claude").join("projects")),
        codex_sessions_dir: Some(tmp.path().join("codex").join("sessions")),
        opencode_storage_dir: Some(tmp.path().join("opencode").join("storage")),
    }
}

/// Build a Claude JSONL session with one assistant turn that contains a
/// tool_use block but **no** tool_result — this is the gap scenario.
/// The contentMode=full parse produces a ToolUse ContentRecord but no
/// ToolResult, so `record_session_gap` sees tool_calls > 0 and tool_results == 0.
fn claude_tool_use_session(session_id: &str) -> String {
    let user = serde_json::json!({
        "parentUuid": null,
        "isSidechain": false,
        "type": "user",
        "message": {"role": "user", "content": "do the thing"},
        "uuid": "u-user-gap",
        "timestamp": "2026-04-22T00:00:00.000Z",
        "cwd": "/tmp/project",
        "sessionId": session_id,
    });
    // Assistant turn with a tool_use block — produces a ToolCall on the
    // TurnRecord and a ToolUse ContentRecord, but no ToolResult sidecar.
    let assistant = serde_json::json!({
        "parentUuid": "u-user-gap",
        "isSidechain": false,
        "message": {
            "model": "claude-sonnet-4-6",
            "id": "msg-gap-asst",
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_gap_1",
                "name": "Bash",
                "input": {"command": "ls"}
            }],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 0,
                    "ephemeral_1h_input_tokens": 0
                }
            }
        },
        "type": "assistant",
        "uuid": "u-asst-gap",
        "timestamp": "2026-04-22T00:00:01.000Z",
        "cwd": "/tmp/project",
        "sessionId": session_id,
    });
    format!("{}\n{}\n", user, assistant)
}

/// Install a buffer-backed gap sink, run `f`, then restore the previous sink
/// and reset the gap state. Returns captured warning bodies.
///
/// The GAP_LOCK is held across the entire call so tests cannot interleave.
/// Cleanup runs via Drop so a panicking test future still restores the
/// process-global writer + clears gap state for the next test in this
/// binary.
async fn with_captured_gap_warnings<F, Fut>(f: F) -> Vec<String>
where
    F: FnOnce(Arc<Mutex<Vec<String>>>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    // The writer slot is `Arc<dyn Fn(&str) + Send + Sync>` in the gap
    // module; the type alias is private, so re-declare it here.
    type GapWriter = Arc<dyn Fn(&str) + Send + Sync>;

    struct Cleanup {
        prev: Option<GapWriter>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                restore_ingest_gap_writer(prev);
            }
            reset_ingest_gap_warnings();
        }
    }

    let _g = GAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ingest_gap_warnings();

    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let clone = captured.clone();
    let prev: GapWriter = set_ingest_gap_writer(move |body| {
        clone.lock().unwrap().push(body.to_string());
    });
    let cleanup = Cleanup { prev: Some(prev) };

    f(captured.clone()).await;

    drop(cleanup);

    Arc::try_unwrap(captured).unwrap().into_inner().unwrap()
}

/// A Claude session with tool_use (no tool_result) must fire the gap warning
/// on the first `ingest_all` call and stay silent on the second.
#[tokio::test]
async fn gap_warning_fires_once_then_suppressed_for_claude() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    // Write a fixture session with a tool_use block.
    let project_dir = roots
        .claude_projects_dir
        .as_ref()
        .unwrap()
        .join("-tmp-project");
    fs::create_dir_all(&project_dir).unwrap();
    let sid = "gggggggg-gggg-gggg-gggg-gggggggggggg";
    let session_file = project_dir.join(format!("{sid}.jsonl"));
    fs::write(&session_file, claude_tool_use_session(sid)).unwrap();

    let warnings = with_captured_gap_warnings(|_captured| async move {
        let mut ledger = open_ledger_in(&tmp);
        let opts = IngestOptions {
            roots: roots.clone(),
            ..Default::default()
        };

        // First ingest: sees tool_use without tool_result → gap warning fires.
        let warn_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let warn_clone = warn_log.clone();
        let opts_with_warn = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                warn_clone.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_all(&mut ledger, &opts_with_warn).await.unwrap();

        let first_warnings = warn_log.lock().unwrap().clone();
        assert_eq!(
            first_warnings.len(),
            1,
            "gap warning must fire exactly once on first ingest (got {:?})",
            first_warnings
        );
        assert!(
            first_warnings[0].contains("claude"),
            "warning body must name the adapter: {:?}",
            first_warnings[0]
        );
        assert!(
            first_warnings[0].contains("1 session"),
            "expected 1 session in warning: {:?}",
            first_warnings[0]
        );

        // Second ingest: same affected set, no new sessions → warning suppressed.
        let warn_log2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let warn_clone2 = warn_log2.clone();
        let opts_with_warn2 = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                warn_clone2.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_all(&mut ledger, &opts_with_warn2).await.unwrap();

        let second_warnings = warn_log2.lock().unwrap().clone();
        assert!(
            second_warnings.is_empty(),
            "second ingest must stay silent (same affected set), got {:?}",
            second_warnings
        );

        // Suppress the unused opts warning.
        let _ = opts;
    })
    .await;

    // The global sink captured nothing because we used `on_warn` callbacks above.
    assert!(
        warnings.is_empty(),
        "global sink must be silent when on_warn is supplied"
    );
}

/// A Claude session with NO tool_use must not trigger the gap warning at all.
#[tokio::test]
async fn no_gap_warning_for_chat_only_claude_session() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let project_dir = roots
        .claude_projects_dir
        .as_ref()
        .unwrap()
        .join("-tmp-proj2");
    fs::create_dir_all(&project_dir).unwrap();
    let sid = "hhhhhhhh-hhhh-hhhh-hhhh-hhhhhhhhhhhh";
    let session_file = project_dir.join(format!("{sid}.jsonl"));

    // Plain text session — no tool_use blocks.
    let user = serde_json::json!({
        "parentUuid": null,
        "isSidechain": false,
        "type": "user",
        "message": {"role": "user", "content": "hi"},
        "uuid": "u-user-chat",
        "timestamp": "2026-04-22T00:00:00.000Z",
        "cwd": "/tmp/project",
        "sessionId": sid,
    });
    let assistant = serde_json::json!({
        "parentUuid": "u-user-chat",
        "isSidechain": false,
        "message": {
            "model": "claude-sonnet-4-6",
            "id": "msg-chat-asst",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 5,
                "output_tokens": 3,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 0,
                    "ephemeral_1h_input_tokens": 0
                }
            }
        },
        "type": "assistant",
        "uuid": "u-asst-chat",
        "timestamp": "2026-04-22T00:00:01.000Z",
        "cwd": "/tmp/project",
        "sessionId": sid,
    });
    fs::write(
        &session_file,
        format!("{}\n{}\n", user, assistant),
    )
    .unwrap();

    let warnings = with_captured_gap_warnings(|captured| async move {
        let mut ledger = open_ledger_in(&tmp);
        let captured_clone = captured.clone();
        let opts = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                captured_clone.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_all(&mut ledger, &opts).await.unwrap();
    })
    .await;

    assert!(
        warnings.is_empty(),
        "chat-only session must not trigger a gap warning, got {:?}",
        warnings
    );
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// Build a Codex rollout JSONL with one `function_call` response_item but
/// no matching `function_call_output` — the gap scenario for the codex
/// adapter. The session_meta line carries the sessionId; under
/// `contentMode=full` the parser emits a ToolUse ContentRecord but no
/// ToolResult, so `record_session_gap(AdapterName::Codex, ...)` sees
/// tool_calls > 0 / tool_results == 0.
fn codex_tool_call_session(session_id: &str) -> String {
    let lines = [
        serde_json::json!({"timestamp":"2026-04-22T00:00:00.000Z","type":"session_meta","payload":{"id":session_id,"cwd":"/tmp/project","timestamp":"2026-04-22T00:00:00.000Z"}}),
        serde_json::json!({"timestamp":"2026-04-22T00:00:00.050Z","type":"turn_context","payload":{"turn_id":"turn_gap_1","cwd":"/tmp/project","model":"gpt-5.4"}}),
        serde_json::json!({"timestamp":"2026-04-22T00:00:00.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"do a thing"}]}}),
        serde_json::json!({"timestamp":"2026-04-22T00:00:00.200Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn_gap_1"}}),
        serde_json::json!({"timestamp":"2026-04-22T00:00:00.400Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}","call_id":"call_gap_1"}}),
        // NB: no `function_call_output` — that's the gap.
        serde_json::json!({"timestamp":"2026-04-22T00:00:00.900Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5}}}}),
        serde_json::json!({"timestamp":"2026-04-22T00:00:01.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn_gap_1"}}),
    ];
    let mut out = String::new();
    for v in &lines {
        out.push_str(&v.to_string());
        out.push('\n');
    }
    out
}

/// A Codex session with function_call (no function_call_output) must fire
/// the gap warning on the first `ingest_all` call and stay silent on the
/// second. Locks the codex branch of `record_session_gap` against silent
/// regression.
#[tokio::test]
async fn gap_warning_fires_once_then_suppressed_for_codex() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let sessions_dir = roots.codex_sessions_dir.as_ref().unwrap();
    fs::create_dir_all(sessions_dir).unwrap();
    let sid = "sess_codex_gap";
    fs::write(
        sessions_dir.join("rollout-codex-gap.jsonl"),
        codex_tool_call_session(sid),
    )
    .unwrap();

    with_captured_gap_warnings(|_captured| async move {
        let mut ledger = open_ledger_in(&tmp);
        let warn1: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let warn1_clone = warn1.clone();
        let opts1 = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                warn1_clone.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_all(&mut ledger, &opts1).await.unwrap();
        let first = warn1.lock().unwrap().clone();
        assert_eq!(
            first.len(),
            1,
            "codex gap warning must fire exactly once on first ingest (got {:?})",
            first
        );
        assert!(
            first[0].starts_with("codex:"),
            "codex warning must lead with adapter name: {:?}",
            first[0]
        );
        assert!(
            first[0].contains("1 session"),
            "expected 1 session: {:?}",
            first[0]
        );

        let warn2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let warn2_clone = warn2.clone();
        let opts2 = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                warn2_clone.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_all(&mut ledger, &opts2).await.unwrap();
        assert!(
            warn2.lock().unwrap().is_empty(),
            "second ingest must stay silent for unchanged codex set"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------------

/// Drop a complete OpenCode session tree (session/, message/, part/) into
/// `<storage>` with one assistant message containing a tool part whose
/// `state` has `input` but **no** `output` — the gap scenario for the
/// opencode adapter. Mirrors the layout in
/// `crates/relayburn-reader/src/opencode/tests.rs`.
fn write_opencode_gap_session(storage: &Path, session_id: &str) {
    let session_dir = storage.join("session/global");
    let msg_dir = storage.join(format!("message/{session_id}"));
    let user_msg = format!("msg_{session_id}_user");
    let asst_msg = format!("msg_{session_id}_asst");
    let part_user = storage.join(format!("part/{user_msg}"));
    let part_asst = storage.join(format!("part/{asst_msg}"));

    fs::create_dir_all(&session_dir).unwrap();
    fs::create_dir_all(&msg_dir).unwrap();
    fs::create_dir_all(&part_user).unwrap();
    fs::create_dir_all(&part_asst).unwrap();

    fs::write(
        session_dir.join(format!("{session_id}.json")),
        serde_json::json!({"id": session_id, "directory": "/tmp/proj"}).to_string(),
    )
    .unwrap();
    fs::write(
        msg_dir.join(format!("{user_msg}.json")),
        serde_json::json!({
            "id": user_msg,
            "sessionID": session_id,
            "role": "user",
            "time": {"created": 1_776_988_000_000_i64},
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        part_user.join("prt_user_a.json"),
        serde_json::json!({
            "id": "prt_user_a",
            "sessionID": session_id,
            "messageID": user_msg,
            "type": "text",
            "text": "do the thing",
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        msg_dir.join(format!("{asst_msg}.json")),
        serde_json::json!({
            "id": asst_msg,
            "sessionID": session_id,
            "role": "assistant",
            "providerID": "anthropic",
            "modelID": "claude-sonnet-4-6",
            "time": {"created": 1_776_988_001_000_i64},
            "path": {"cwd": "/tmp/proj"},
            "tokens": {"input": 100, "output": 20, "cache": {"read": 0, "write": 0}},
        })
        .to_string(),
    )
    .unwrap();
    // Tool part with `input` but no `output` — capture sees a ToolUse but
    // no ToolResult content record, the gap scenario.
    fs::write(
        part_asst.join("prt_asst_tool.json"),
        serde_json::json!({
            "id": "prt_asst_tool",
            "sessionID": session_id,
            "messageID": asst_msg,
            "type": "tool",
            "callID": "call_oc_gap",
            "tool": "bash",
            "state": {"status": "running", "input": {"command": "sleep 60"}},
        })
        .to_string(),
    )
    .unwrap();
}

/// An OpenCode session with a tool-use part missing its output must fire
/// the gap warning on the first `ingest_all` call and stay silent on the
/// second. Locks the opencode branch of `record_session_gap`.
#[tokio::test]
async fn gap_warning_fires_once_then_suppressed_for_opencode() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let storage = roots.opencode_storage_dir.as_ref().unwrap();
    fs::create_dir_all(storage).unwrap();
    let sid = "ses_opencode_gap";
    write_opencode_gap_session(storage, sid);

    with_captured_gap_warnings(|_captured| async move {
        let mut ledger = open_ledger_in(&tmp);
        let warn1: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let warn1_clone = warn1.clone();
        let opts1 = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                warn1_clone.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_all(&mut ledger, &opts1).await.unwrap();
        let first = warn1.lock().unwrap().clone();
        assert_eq!(
            first.len(),
            1,
            "opencode gap warning must fire exactly once on first ingest (got {:?})",
            first
        );
        assert!(
            first[0].starts_with("opencode:"),
            "opencode warning must lead with adapter name: {:?}",
            first[0]
        );
        assert!(
            first[0].contains("1 session"),
            "expected 1 session: {:?}",
            first[0]
        );

        let warn2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let warn2_clone = warn2.clone();
        let opts2 = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                warn2_clone.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_all(&mut ledger, &opts2).await.unwrap();
        assert!(
            warn2.lock().unwrap().is_empty(),
            "second ingest must stay silent for unchanged opencode set"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// Per-harness verb followed by ingest_all
// ---------------------------------------------------------------------------

/// Lock the per-adapter emission boundary in `ingest_all`: a per-harness
/// `ingest_codex_sessions` should fire its own warning, and a follow-up
/// `ingest_all` with no new affected codex sessions must stay silent for
/// codex while still being able to fire for a fresh claude gap. This
/// catches regressions where stale gap state from a per-harness verb
/// suppresses what should be a fresh `ingest_all` warning, or where a
/// later adapter's failure swallows an earlier adapter's gap.
#[tokio::test]
async fn per_harness_then_ingest_all_keeps_each_adapter_isolated() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    // Codex fixture present from the start; claude fixture appears
    // between the two ingest calls.
    let sessions_dir = roots.codex_sessions_dir.as_ref().unwrap();
    fs::create_dir_all(sessions_dir).unwrap();
    fs::write(
        sessions_dir.join("rollout-mix.jsonl"),
        codex_tool_call_session("sess_codex_mix"),
    )
    .unwrap();

    with_captured_gap_warnings(|_captured| async move {
        let mut ledger = open_ledger_in(&tmp);

        // Step 1: per-harness codex ingest fires its own warning.
        let warn1: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let warn1_clone = warn1.clone();
        let opts_codex = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                warn1_clone.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_codex_sessions(&mut ledger, &opts_codex)
            .await
            .unwrap();
        let after_codex = warn1.lock().unwrap().clone();
        assert_eq!(
            after_codex.len(),
            1,
            "per-harness codex ingest should fire exactly one warning (got {:?})",
            after_codex
        );
        assert!(after_codex[0].starts_with("codex:"));

        // Step 2: drop a fresh claude gap fixture; a follow-up
        // ingest_all should stay silent for codex (nothing new) but
        // emit for claude (fresh adapter).
        let project_dir = roots
            .claude_projects_dir
            .as_ref()
            .unwrap()
            .join("-tmp-mix");
        fs::create_dir_all(&project_dir).unwrap();
        let claude_sid = "mmmmmmmm-mmmm-mmmm-mmmm-mmmmmmmmmmmm";
        fs::write(
            project_dir.join(format!("{claude_sid}.jsonl")),
            claude_tool_use_session(claude_sid),
        )
        .unwrap();

        let warn2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let warn2_clone = warn2.clone();
        let opts_all = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                warn2_clone.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_all(&mut ledger, &opts_all).await.unwrap();
        let after_all = warn2.lock().unwrap().clone();
        let claude_count = after_all
            .iter()
            .filter(|w| w.starts_with("claude:"))
            .count();
        let codex_count = after_all
            .iter()
            .filter(|w| w.starts_with("codex:"))
            .count();
        assert_eq!(
            claude_count, 1,
            "ingest_all should fire one claude warning after fresh fixture (got {:?})",
            after_all
        );
        assert_eq!(
            codex_count, 0,
            "ingest_all should stay silent for codex (no new affected session): {:?}",
            after_all
        );
    })
    .await;
}

/// Smoke-test the per-harness public verbs (`ingest_claude_projects`,
/// `ingest_opencode_sessions`) emit gap warnings end-to-end. Pairs with
/// the codex coverage above so all three per-harness paths are exercised.
#[tokio::test]
async fn per_harness_verbs_emit_gap_warnings() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    // Claude fixture.
    let project_dir = roots
        .claude_projects_dir
        .as_ref()
        .unwrap()
        .join("-tmp-claude");
    fs::create_dir_all(&project_dir).unwrap();
    let claude_sid = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    fs::write(
        project_dir.join(format!("{claude_sid}.jsonl")),
        claude_tool_use_session(claude_sid),
    )
    .unwrap();

    // OpenCode fixture.
    let storage = roots.opencode_storage_dir.as_ref().unwrap();
    fs::create_dir_all(storage).unwrap();
    write_opencode_gap_session(storage, "ses_oc_smoke");

    with_captured_gap_warnings(|_captured| async move {
        let mut ledger = open_ledger_in(&tmp);

        let warn_claude: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let warn_claude_clone = warn_claude.clone();
        let opts_claude = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                warn_claude_clone.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_claude_projects(&mut ledger, &opts_claude)
            .await
            .unwrap();
        let claude = warn_claude.lock().unwrap().clone();
        assert_eq!(claude.len(), 1, "claude per-harness verb fires once");
        assert!(claude[0].starts_with("claude:"));

        let warn_oc: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let warn_oc_clone = warn_oc.clone();
        let opts_oc = IngestOptions {
            roots: roots.clone(),
            on_warn: Some(Box::new(move |body: &str| {
                warn_oc_clone.lock().unwrap().push(body.to_string());
            })),
            ..Default::default()
        };
        ingest_opencode_sessions(&mut ledger, &opts_oc)
            .await
            .unwrap();
        let oc = warn_oc.lock().unwrap().clone();
        assert_eq!(oc.len(), 1, "opencode per-harness verb fires once");
        assert!(oc[0].starts_with("opencode:"));
    })
    .await;
}
