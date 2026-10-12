use super::*;

use crate::analyze::WasteFinding;
use crate::reader::Usage;
use tempfile::TempDir;

const SESSION: &str = "sess-cache";

fn turn(message_id: &str, ts: &str, cache_read: u64, create_1h: u64) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: SESSION.into(),
        session_path: None,
        message_id: message_id.into(),
        turn_index: 0,
        ts: ts.into(),
        model: "claude-sonnet-4-6".into(),
        project: Some("/tmp/proj".into()),
        project_key: None,
        usage: Usage {
            input: 10,
            output: 100,
            cache_read,
            cache_create_1h: create_1h,
            ..Usage::default()
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

/// A warm 100k-token context at 10:00, a read at 10:30 that keeps it warm,
/// and a cold re-write two hours later at 12:30.
fn ledger() -> (TempDir, LedgerHandle) {
    let dir = tempfile::tempdir().unwrap();
    let mut handle = Ledger::open(LedgerOpenOptions::with_home(dir.path())).expect("open ledger");
    handle
        .raw_mut()
        .append_turns(&[
            turn("warm", "2026-04-23T10:00:00.000Z", 90_000, 9_990),
            turn("read", "2026-04-23T10:30:00.000Z", 100_000, 0),
            turn("cold", "2026-04-23T12:30:00.000Z", 0, 101_000),
        ])
        .expect("append turns");
    (dir, handle)
}

fn cache_expiry_findings_since(handle: &LedgerHandle, since: Option<&str>) -> Vec<WasteFinding> {
    let result = handle
        .hotspots(HotspotsOptions {
            since: since.map(str::to_string),
            group_by: Some(HotspotsGroupBy::Findings),
            patterns: Some(vec!["cache-expiry".into()]),
            ..Default::default()
        })
        .expect("hotspots");
    match result {
        HotspotsResult::Findings { findings, .. } => findings,
        other => panic!("expected findings, got {other:?}"),
    }
}

#[test]
fn reports_cache_expiry_across_the_whole_ledger() {
    let (_dir, handle) = ledger();
    let findings = cache_expiry_findings_since(&handle, None);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, "cache-expiry");
    assert_eq!(findings[0].session_id, SESSION);
    assert_eq!(
        findings[0].estimated_savings.tokens_per_session,
        Some(100_010)
    );
}

#[test]
fn a_resume_at_the_window_start_is_measured_against_earlier_turns() {
    let (_dir, handle) = ledger();
    let findings = cache_expiry_findings_since(&handle, Some("2026-04-23T12:30:00.000Z"));
    assert_eq!(findings.len(), 1);
    assert_eq!(
        findings[0].estimated_savings.tokens_per_session,
        Some(100_010)
    );
}

#[test]
fn resumes_before_the_window_are_not_reported() {
    let (_dir, handle) = ledger();
    assert!(cache_expiry_findings_since(&handle, Some("2026-04-23T12:31:00.000Z")).is_empty());
}
