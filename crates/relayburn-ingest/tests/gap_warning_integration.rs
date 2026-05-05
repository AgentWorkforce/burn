//! Integration tests for the gap-warning path wired in #295.
//!
//! Each test drops a fixture Claude session that has tool_use blocks but no
//! tool_result content, runs `ingest_all` against a temp `Ledger`, and
//! asserts that the warning fires exactly once. A second run with no new
//! affected sessions must stay silent (suppression).
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
use std::sync::{Arc, Mutex};

use relayburn_ingest::{
    ingest_all, reset_ingest_gap_warnings, restore_ingest_gap_writer, set_ingest_gap_writer,
    IngestOptions, IngestRoots,
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
async fn with_captured_gap_warnings<F, Fut>(f: F) -> Vec<String>
where
    F: FnOnce(Arc<Mutex<Vec<String>>>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _g = GAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ingest_gap_warnings();

    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let clone = captured.clone();
    let prev = set_ingest_gap_writer(move |body| {
        clone.lock().unwrap().push(body.to_string());
    });

    f(captured.clone()).await;

    restore_ingest_gap_writer(prev);
    reset_ingest_gap_warnings();

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
