//! End-to-end coverage for JSON output piped to an early-closing consumer.

use std::io::Read;
use std::process::{Command, Stdio};

use relayburn_sdk::{Ledger, LedgerOpenOptions, SourceKind, TurnRecord, Usage};

const SESSION_COUNT: usize = 4_096;

fn turn(index: usize) -> TurnRecord {
    // Keep every session id long as well as unique. The resulting JSON is
    // several MiB, so it cannot fit in a pipe buffer before the reader closes.
    let suffix = "x".repeat(512);
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: format!("session_{index:04}_{suffix}"),
        session_path: None,
        message_id: format!("message_{index:04}"),
        turn_index: 0,
        ts: "2026-08-03T00:00:00.000Z".into(),
        model: "claude-sonnet-4-6".into(),
        project: Some(format!("/tmp/project/{index:04}")),
        project_key: None,
        usage: Usage {
            input: 100,
            output: 50,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        },
        tool_calls: Vec::new(),
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    }
}

#[test]
fn large_json_output_exits_zero_when_consumer_closes_early() {
    let home = tempfile::TempDir::new().expect("temporary ledger home");
    let mut ledger = Ledger::open(LedgerOpenOptions::with_home(home.path())).expect("open ledger");
    let turns: Vec<_> = (0..SESSION_COUNT).map(turn).collect();
    ledger
        .raw_mut()
        .append_turns(&turns)
        .expect("seed large session list");
    drop(ledger);

    let mut child = Command::new(env!("CARGO_BIN_EXE_burn"))
        .args([
            "--json",
            "--ledger-path",
            home.path().to_str().expect("UTF-8 temp path"),
            "sessions",
            "list",
            "--since",
            "1970-01-01T00:00:00Z",
            "--limit",
            &SESSION_COUNT.to_string(),
        ])
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn burn");

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut prefix = [0_u8; 2];
    stdout.read_exact(&mut prefix).expect("read JSON prefix");
    assert_eq!(&prefix, b"{\n");
    drop(stdout);

    let output = child.wait_with_output().expect("wait for burn");
    assert!(
        output.status.success(),
        "early pipe closure should exit 0, got {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
}
