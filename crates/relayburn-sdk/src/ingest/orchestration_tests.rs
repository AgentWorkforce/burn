//! Per-harness orchestration round-trip tests for #277.
//!
//! Each test drops one fixture session under the harness's session root,
//! runs the matching ingest verb against a temp `Ledger`, and asserts that
//! turns land in the events DB and a cursor for the file is persisted.
//!
//! `ingest_all_walks_each_harness_root_once` exercises the unified verb
//! across all three harnesses simultaneously. `ingest_claude_session_*`
//! covers the per-session fast-path used when the caller already knows the
//! Claude session id.
//!
//! ## Concurrency note
//!
//! These tests mutate `RELAYBURN_HOME` (the ledger / pending-stamp /
//! config layer reads it at runtime). Cargo runs tests in a single binary
//! on a thread pool, so a per-test env mutation would race. Each test
//! takes [`ENV_LOCK`] for its lifetime — the lock is held across the
//! whole test body so the env stays consistent for any code path that
//! reads `RELAYBURN_HOME` mid-ingest. Tests also pin all three
//! per-harness roots in [`IngestRoots`] so a stray `~/.claude/projects/`
//! on the developer's machine can't be picked up.
//!
//! Synchronous `#[test]` runners, so a `std::sync::Mutex` guard held for
//! the duration of a test body is sound — no other test runs concurrently
//! against the same process env.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ingest::cursors::{load_cursors, ClaudeCursor, FileCursor};
use crate::ingest::ingest::{
    ingest_all, ingest_claude_projects, ingest_claude_session, ingest_claude_transcript_path,
    ingest_codex_sessions, ingest_opencode_sessions, IngestOptions, IngestRoots,
};
use crate::ingest::pending_stamps::{write_pending_stamp, PendingStampHarness, WriteOptions};
use crate::ledger::{Enrichment, Ledger, LedgerLayout, Query};
use tempfile::TempDir;

// Shared with gap_warning_tests / watch_loop_tests so that
// `$RELAYBURN_HOME` mutations stay serialised across all test modules
// in the same binary. See note in `crate::ingest`.
use super::TEST_ENV_LOCK as ENV_LOCK;

fn shared_fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
}

fn open_ledger_in(tmp: &TempDir) -> Ledger {
    let layout = LedgerLayout::under(tmp.path().join("ledger"));
    fs::create_dir_all(&layout.home).unwrap();
    Ledger::open(&layout.burn, &layout.content).unwrap()
}

/// Pin RELAYBURN_HOME under `tmp` so the pending-stamp + config layers
/// can't scribble on the developer's `~/.agentworkforce/burn`. Caller holds
/// the returned mutex guard for the whole test body.
fn isolated_relayburn_home<'a>(tmp: &TempDir) -> std::sync::MutexGuard<'a, ()> {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tmp.path().join("relayburn");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("RELAYBURN_HOME", &home);
    guard
}

/// Build a self-contained Claude JSONL with one user + one assistant turn,
/// `sessionId` baked into every event so the parser doesn't depend on the
/// filename to derive it.
fn claude_minimal_session(session_id: &str) -> String {
    claude_minimal_session_with_cwd(session_id, "/tmp/project")
}

fn claude_minimal_session_with_cwd(session_id: &str, cwd: &str) -> String {
    let user = serde_json::json!({
        "parentUuid": null,
        "isSidechain": false,
        "type": "user",
        "message": {"role": "user", "content": "hi"},
        "uuid": "u-user-1",
        "timestamp": "2026-04-22T00:00:00.000Z",
        "cwd": cwd,
        "sessionId": session_id,
    });
    let assistant = serde_json::json!({
        "parentUuid": "u-user-1",
        "isSidechain": false,
        "message": {
            "model": "claude-sonnet-4-6",
            "id": "msg-asst-1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi there"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 0,
                    "ephemeral_1h_input_tokens": 0
                }
            }
        },
        "type": "assistant",
        "uuid": "u-asst-1",
        "timestamp": "2026-04-22T00:00:01.000Z",
        "cwd": cwd,
        "sessionId": session_id,
    });
    format!("{}\n{}\n", user, assistant)
}

/// Build a roots overrides struct that pins all three harness roots under
/// `tmp` so an unset root can never escape into the developer's HOME.
fn pinned_roots(tmp: &TempDir) -> IngestRoots {
    IngestRoots {
        claude_projects_dir: Some(tmp.path().join("claude").join("projects")),
        codex_sessions_dir: Some(tmp.path().join("codex").join("sessions")),
        opencode_storage_dir: Some(tmp.path().join("opencode").join("storage")),
    }
}

#[test]
fn ingest_claude_projects_round_trips_a_fixture_session() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let project_dir = roots
        .claude_projects_dir
        .as_ref()
        .unwrap()
        .join("-tmp-project");
    fs::create_dir_all(&project_dir).unwrap();
    let sid = "11111111-1111-1111-1111-111111111111";
    let session_file = project_dir.join(format!("{sid}.jsonl"));
    fs::write(&session_file, claude_minimal_session(sid)).unwrap();

    let mut ledger = open_ledger_in(&tmp);
    let opts = IngestOptions {
        roots,
        ..Default::default()
    };
    let report = ingest_claude_projects(&mut ledger, &opts).unwrap();

    assert!(report.appended_turns >= 1, "expected ≥1 turn ingested");
    assert!(report.ingested_sessions >= 1);
    let turns = ledger.query_turns(&Query::for_session(sid)).unwrap();
    assert!(
        !turns.is_empty(),
        "ingested turns should be queryable by sessionId"
    );

    let cursors = load_cursors(&ledger).unwrap();
    let key = session_file.to_string_lossy().into_owned();
    match cursors.get_typed(&key) {
        Some(FileCursor::Claude(_)) => {}
        other => panic!("expected ClaudeCursor for {key}, got {other:?}"),
    }
}

#[test]
fn ingest_claude_projects_resolves_pending_stamp_tags() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let mut enrichment = Enrichment::new();
    enrichment.insert("persona".to_string(), "code-reviewer".to_string());
    let cwd = tmp.path().join("project");
    fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_string_lossy().into_owned();
    write_pending_stamp(WriteOptions {
        harness: PendingStampHarness::Claude,
        cwd: cwd.clone(),
        enrichment: enrichment.clone(),
        ..Default::default()
    })
    .unwrap();

    let project_dir = roots
        .claude_projects_dir
        .as_ref()
        .unwrap()
        .join("-tmp-project");
    fs::create_dir_all(&project_dir).unwrap();
    let sid = "33333333-3333-3333-3333-333333333333";
    let session_file = project_dir.join(format!("{sid}.jsonl"));
    fs::write(&session_file, claude_minimal_session_with_cwd(sid, &cwd)).unwrap();

    let mut ledger = open_ledger_in(&tmp);
    let opts = IngestOptions {
        roots,
        ..Default::default()
    };
    let report = ingest_claude_projects(&mut ledger, &opts).unwrap();

    assert!(report.appended_turns >= 1, "expected >=1 turn ingested");
    assert_eq!(report.applied_pending_stamps, 1);
    let turns = ledger
        .query_turns(&Query {
            enrichment: Some(enrichment),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].turn.session_id, sid);
}

#[test]
fn ingest_codex_sessions_round_trips_a_fixture_session() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let codex_root = roots.codex_sessions_dir.clone().unwrap();
    fs::create_dir_all(&codex_root).unwrap();
    let session_file = codex_root.join("rollout-2026-04-20.jsonl");
    let src = shared_fixture_dir().join("codex").join("simple-turn.jsonl");
    fs::copy(&src, &session_file).unwrap();

    let mut ledger = open_ledger_in(&tmp);
    let opts = IngestOptions {
        roots,
        ..Default::default()
    };
    let report = ingest_codex_sessions(&mut ledger, &opts).unwrap();
    assert!(report.appended_turns >= 1, "expected ≥1 codex turn");
    let turns = ledger
        .query_turns(&Query::for_session("sess_simple_1"))
        .unwrap();
    assert!(!turns.is_empty(), "codex turn should be queryable");

    let cursors = load_cursors(&ledger).unwrap();
    let key = session_file.to_string_lossy().into_owned();
    match cursors.get_typed(&key) {
        Some(FileCursor::Codex(_)) => {}
        other => panic!("expected CodexCursor for {key}, got {other:?}"),
    }
}

#[test]
fn ingest_opencode_sessions_round_trips_a_fixture_session() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let storage_root = roots.opencode_storage_dir.clone().unwrap();
    let src_storage = shared_fixture_dir()
        .join("opencode")
        .join("simple")
        .join("storage");
    copy_dir_recursive(&src_storage, &storage_root);

    let mut ledger = open_ledger_in(&tmp);
    let opts = IngestOptions {
        roots,
        ..Default::default()
    };
    let report = ingest_opencode_sessions(&mut ledger, &opts).unwrap();
    assert!(
        report.appended_turns >= 1,
        "expected ≥1 opencode turn (got {})",
        report.appended_turns
    );

    let session_file = storage_root
        .join("session")
        .join("global")
        .join("ses_simple.json");
    let cursors = load_cursors(&ledger).unwrap();
    let key = session_file.to_string_lossy().into_owned();
    match cursors.get_typed(&key) {
        Some(FileCursor::Opencode(_)) => {}
        other => panic!("expected OpencodeCursor for {key}, got {other:?}"),
    }
}

#[test]
fn ingest_all_walks_each_harness_root_once() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    // Claude
    let cl_proj = roots
        .claude_projects_dir
        .as_ref()
        .unwrap()
        .join("-tmp-project");
    fs::create_dir_all(&cl_proj).unwrap();
    let claude_sid = "22222222-2222-2222-2222-222222222222";
    let claude_file = cl_proj.join(format!("{claude_sid}.jsonl"));
    fs::write(&claude_file, claude_minimal_session(claude_sid)).unwrap();

    // Codex
    let codex_root = roots.codex_sessions_dir.clone().unwrap();
    fs::create_dir_all(&codex_root).unwrap();
    let codex_file = codex_root.join("rollout-2026-04-20.jsonl");
    fs::copy(
        shared_fixture_dir().join("codex").join("simple-turn.jsonl"),
        &codex_file,
    )
    .unwrap();

    // OpenCode
    let opencode_root = roots.opencode_storage_dir.clone().unwrap();
    copy_dir_recursive(
        &shared_fixture_dir()
            .join("opencode")
            .join("simple")
            .join("storage"),
        &opencode_root,
    );

    let mut ledger = open_ledger_in(&tmp);
    let opts = IngestOptions {
        roots,
        ..Default::default()
    };
    let report = ingest_all(&mut ledger, &opts).unwrap();
    assert!(
        report.appended_turns >= 3,
        "expected ≥3 turns total across the three harnesses (got {})",
        report.appended_turns
    );

    let cursors = load_cursors(&ledger).unwrap();
    let cursor_keys: HashSet<_> = cursors.files.keys().cloned().collect();
    assert!(
        cursor_keys.contains(&claude_file.to_string_lossy().into_owned()),
        "claude cursor was not persisted"
    );
    assert!(
        cursor_keys.contains(&codex_file.to_string_lossy().into_owned()),
        "codex cursor was not persisted"
    );
    let opencode_session_path = opencode_root
        .join("session")
        .join("global")
        .join("ses_simple.json");
    assert!(
        cursor_keys.contains(&opencode_session_path.to_string_lossy().into_owned()),
        "opencode cursor was not persisted"
    );
}

#[test]
fn ingest_claude_session_writes_eof_cursor_so_followup_skips_file() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let cwd = "/tmp/myproject";
    let sid = "abcdef12-3456-7890-abcd-ef1234567890";
    let project_dir = roots
        .claude_projects_dir
        .as_ref()
        .unwrap()
        .join("-tmp-myproject");
    fs::create_dir_all(&project_dir).unwrap();
    let session_file = project_dir.join(format!("{sid}.jsonl"));
    fs::write(&session_file, claude_minimal_session(sid)).unwrap();
    let original_size = fs::metadata(&session_file).unwrap().len();

    let mut ledger = open_ledger_in(&tmp);
    let opts = IngestOptions {
        roots,
        ..Default::default()
    };

    let r = ingest_claude_session(&mut ledger, cwd, sid, &opts).unwrap();
    assert!(r.appended_turns >= 1, "expected ≥1 turn appended");
    assert_eq!(r.ingested_sessions, 1);

    // Cursor must point at EOF.
    let cursors = load_cursors(&ledger).unwrap();
    let key = session_file.to_string_lossy().into_owned();
    match cursors.get_typed(&key) {
        Some(FileCursor::Claude(ClaudeCursor { offset_bytes, .. })) => {
            assert_eq!(offset_bytes, original_size);
        }
        other => panic!("expected ClaudeCursor at EOF for {key}, got {other:?}"),
    }

    // A subsequent ingest_all sweep with the same file content must skip
    // it — appendedTurns should not go up.
    let before_count = ledger.query_turns(&Query::for_session(sid)).unwrap().len();
    let r2 = ingest_all(&mut ledger, &opts).unwrap();
    let after_count = ledger.query_turns(&Query::for_session(sid)).unwrap().len();
    assert_eq!(
        before_count, after_count,
        "follow-up ingest_all should not re-append turns when cursor is at EOF"
    );
    assert_eq!(
        r2.appended_turns, 0,
        "follow-up ingest_all reported {} new turns; cursor should have skipped the file",
        r2.appended_turns
    );
}

/// `ingest_claude_transcript_path` is the SDK fast-path used by `burn
/// ingest --hook claude`: hand it the JSONL the Claude Code hook
/// payload points at and it must ingest only that one session,
/// persist an EOF cursor for it, and a follow-up `ingest_all` sweep
/// must skip the file. Mirrors
/// `ingest_claude_session_writes_eof_cursor_so_followup_skips_file`
/// but exercises the path-based public entrypoint instead of the
/// cwd+sessionId-based one so the hook's transcript_path stays an
/// opaque path (no cwd decoding round-trip).
#[test]
fn ingest_claude_transcript_path_writes_eof_cursor_so_followup_skips_file() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let sid = "abcdef12-3456-7890-abcd-ef1234567899";
    let project_dir = roots
        .claude_projects_dir
        .as_ref()
        .unwrap()
        .join("-tmp-myproject");
    fs::create_dir_all(&project_dir).unwrap();
    let session_file = project_dir.join(format!("{sid}.jsonl"));
    fs::write(&session_file, claude_minimal_session(sid)).unwrap();

    let mut ledger = open_ledger_in(&tmp);
    let opts = IngestOptions {
        roots,
        ..Default::default()
    };

    let r = ingest_claude_transcript_path(&mut ledger, &session_file, &opts).unwrap();
    assert!(r.appended_turns >= 1, "expected ≥1 turn appended");
    assert_eq!(r.ingested_sessions, 1);

    let cursors = load_cursors(&ledger).unwrap();
    let key = session_file.to_string_lossy().into_owned();
    match cursors.get_typed(&key) {
        Some(FileCursor::Claude(ClaudeCursor { offset_bytes, .. })) => {
            assert_eq!(offset_bytes, fs::metadata(&session_file).unwrap().len());
        }
        other => panic!("expected ClaudeCursor at EOF for {key}, got {other:?}"),
    }

    let before_count = ledger.query_turns(&Query::for_session(sid)).unwrap().len();
    let r2 = ingest_all(&mut ledger, &opts).unwrap();
    let after_count = ledger.query_turns(&Query::for_session(sid)).unwrap().len();
    assert_eq!(
        before_count, after_count,
        "follow-up ingest_all should not re-append turns when cursor is at EOF"
    );
    assert_eq!(r2.appended_turns, 0);
}

/// Missing transcript path is a no-op, not an error: hook callers
/// must never fail the parent invocation just because the JSONL was
/// rotated or never written. Returns an empty report; subsequent
/// queries must still succeed.
#[test]
fn ingest_claude_transcript_path_missing_file_is_empty_report() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let bogus = tmp.path().join("never-existed.jsonl");
    let mut ledger = open_ledger_in(&tmp);
    let opts = IngestOptions {
        roots,
        ..Default::default()
    };
    let r = ingest_claude_transcript_path(&mut ledger, &bogus, &opts).unwrap();
    assert_eq!(r.ingested_sessions, 0);
    assert_eq!(r.appended_turns, 0);
}

/// No-op fast path (#468): once a sweep records the source fingerprint, a
/// follow-up `ingest_all` with no upstream change must short-circuit before
/// it walks any session file. The earlier EOF-cursor test proves
/// `appended_turns == 0`; this one proves the stronger property that the
/// loops never ran at all (`scanned_sessions == 0`).
#[test]
fn ingest_all_short_circuits_when_source_unchanged() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let project_dir = roots
        .claude_projects_dir
        .as_ref()
        .unwrap()
        .join("-tmp-project");
    fs::create_dir_all(&project_dir).unwrap();
    let sid = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let session_file = project_dir.join(format!("{sid}.jsonl"));
    fs::write(&session_file, claude_minimal_session(sid)).unwrap();

    let mut ledger = open_ledger_in(&tmp);
    let opts = IngestOptions {
        roots,
        ..Default::default()
    };

    let r1 = ingest_all(&mut ledger, &opts).unwrap();
    assert!(r1.scanned_sessions >= 1, "first sweep should walk the file");
    assert!(
        r1.appended_turns >= 1,
        "first sweep should ingest the turns"
    );

    // Nothing on disk moved — the second sweep must take the fast path.
    let r2 = ingest_all(&mut ledger, &opts).unwrap();
    assert_eq!(
        r2.scanned_sessions, 0,
        "unchanged source should short-circuit before walking any file"
    );
    assert_eq!(r2.appended_turns, 0);
}

/// The fast path must not mask new data: appending to an existing session
/// file changes its size + mtime, so the source fingerprint differs and the
/// next `ingest_all` re-walks and picks up the new turns.
#[test]
fn ingest_all_rescans_after_source_file_appended() {
    let tmp = TempDir::new().unwrap();
    let _env = isolated_relayburn_home(&tmp);
    let roots = pinned_roots(&tmp);

    let project_dir = roots
        .claude_projects_dir
        .as_ref()
        .unwrap()
        .join("-tmp-project");
    fs::create_dir_all(&project_dir).unwrap();

    let sid_a = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    let file_a = project_dir.join(format!("{sid_a}.jsonl"));
    fs::write(&file_a, claude_minimal_session(sid_a)).unwrap();

    let mut ledger = open_ledger_in(&tmp);
    let opts = IngestOptions {
        roots,
        ..Default::default()
    };

    let r1 = ingest_all(&mut ledger, &opts).unwrap();
    assert!(r1.appended_turns >= 1);

    // Confirm the gate is armed: an unchanged re-sweep short-circuits.
    assert_eq!(ingest_all(&mut ledger, &opts).unwrap().scanned_sessions, 0);

    // Drop a brand-new session file — the fingerprint's file count + byte
    // total + mtime sum all move, so the gate must reopen. Give it a
    // distinct timestamp/token shape so layer-2 content dedup doesn't
    // collapse it onto session A's structurally-identical turn.
    let sid_b = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    let file_b = project_dir.join(format!("{sid_b}.jsonl"));
    fs::write(&file_b, claude_distinct_session(sid_b)).unwrap();

    let r3 = ingest_all(&mut ledger, &opts).unwrap();
    assert!(
        r3.scanned_sessions >= 1,
        "a new source file must defeat the fast path"
    );
    assert!(
        r3.appended_turns >= 1,
        "the new session's turns must be ingested"
    );
    let turns_b = ledger.query_turns(&Query::for_session(sid_b)).unwrap();
    assert!(!turns_b.is_empty(), "new session should be queryable");
}

// --- helpers -------------------------------------------------------------

/// Like [`claude_minimal_session`] but with a distinct timestamp, message
/// id, and token shape so its layer-2 content fingerprint differs from the
/// minimal fixture — lets a test ingest two sessions without one being
/// deduped onto the other.
fn claude_distinct_session(session_id: &str) -> String {
    let user = serde_json::json!({
        "parentUuid": null,
        "isSidechain": false,
        "type": "user",
        "message": {"role": "user", "content": "hello again"},
        "uuid": "u-user-2",
        "timestamp": "2026-05-01T12:00:00.000Z",
        "cwd": "/tmp/project",
        "sessionId": session_id,
    });
    let assistant = serde_json::json!({
        "parentUuid": "u-user-2",
        "isSidechain": false,
        "message": {
            "model": "claude-sonnet-4-6",
            "id": "msg-asst-2",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "different reply"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 42,
                "output_tokens": 99,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 0,
                    "ephemeral_1h_input_tokens": 0
                }
            }
        },
        "type": "assistant",
        "uuid": "u-asst-2",
        "timestamp": "2026-05-01T12:00:01.000Z",
        "cwd": "/tmp/project",
        "sessionId": session_id,
    });
    format!("{}\n{}\n", user, assistant)
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ft = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&entry.path(), &target);
        } else if ft.is_file() {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}
