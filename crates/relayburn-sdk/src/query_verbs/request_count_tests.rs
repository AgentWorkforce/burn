//! Turn counts are model-request counts: a Codex task record counts each
//! request it made, every other turn counts one.

use super::summary::match_summary_relationships_to_turns;
use super::*;
use crate::analyze::pricing::load_builtin_pricing;
use crate::reader::{RelationshipSourceKind, Usage};
use tempfile::TempDir;

const CODEX: &str = "codex-s";
const CLAUDE: &str = "claude-s";

fn turn(
    source: SourceKind,
    session: &str,
    message: &str,
    model: &str,
    request_count: Option<u64>,
) -> TurnRecord {
    let input = 1000 + message.len() as u64;
    serde_json::from_value(serde_json::json!({
        "v": 1, "source": source, "sessionId": session, "messageId": message,
        "turnIndex": 0, "requestCount": request_count,
        "ts": format!("2026-04-23T00:00:0{}.000Z", message.len()),
        "model": model, "project": "/tmp/proj",
        "usage": {"input": input, "output": 50, "reasoning": 0, "cacheRead": 0,
                  "cacheCreate5m": 0, "cacheCreate1h": 0},
        "toolCalls": [{"id": format!("tu-{message}"), "name": "Read", "argsHash": message}],
    }))
    .unwrap()
}

/// Two Codex tasks of three and two requests, and one Claude turn: six
/// requests in three records.
fn turns() -> Vec<TurnRecord> {
    vec![
        turn(SourceKind::Codex, CODEX, "t1", "gpt-5.4", Some(3)),
        turn(SourceKind::Codex, CODEX, "t22", "gpt-5.4", Some(2)),
        turn(
            SourceKind::ClaudeCode,
            CLAUDE,
            "m333",
            "claude-sonnet-4-6",
            None,
        ),
    ]
}

fn handle() -> (TempDir, LedgerHandle) {
    let dir = tempfile::tempdir().unwrap();
    let mut handle = Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
    handle.raw_mut().append_turns(&turns()).unwrap();
    (dir, handle)
}

fn grouped(handle: &LedgerHandle, opts: SummaryReportOptions) -> SummaryGroupedReport {
    match handle.summary_report(opts).unwrap() {
        SummaryReport::Grouped(grouped) => grouped,
        _ => panic!("expected a grouped report"),
    }
}

#[test]
fn summary_counts_requests() {
    let (_dir, handle) = handle();
    let s = handle.summary(SummaryOptions::default()).unwrap();
    assert_eq!(s.turn_count, 6);
    let by_tag = handle
        .summary(SummaryOptions {
            group_by_tag: Some("team".into()),
            ..SummaryOptions::default()
        })
        .unwrap()
        .by_tag
        .unwrap();
    assert_eq!(by_tag.iter().map(|r| r.turn_count).sum::<u64>(), 6);
}

#[test]
fn grouped_reports_count_requests_per_row() {
    let (_dir, handle) = handle();
    let by_model = grouped(&handle, SummaryReportOptions::default());
    assert_eq!(by_model.turn_count, 6);
    let rows: Vec<(&str, u64)> = by_model
        .rows
        .iter()
        .map(|r| (r.label.as_str(), r.turns))
        .collect();
    assert_eq!(rows, [("gpt-5.4", 5), ("claude-sonnet-4-6", 1)]);

    let by_provider = grouped(
        &handle,
        SummaryReportOptions {
            mode: SummaryReportMode::Grouped { by_provider: true },
            ..SummaryReportOptions::default()
        },
    );
    assert_eq!(by_provider.rows.iter().map(|r| r.turns).sum::<u64>(), 6);

    let by_tag = grouped(
        &handle,
        SummaryReportOptions {
            group_by_tag: Some("team".into()),
            ..SummaryReportOptions::default()
        },
    );
    assert_eq!(by_tag.turn_count, 6);
    assert_eq!(by_tag.rows.iter().map(|r| r.turns).sum::<u64>(), 6);
}

#[test]
fn the_by_tool_report_counts_requests() {
    let (_dir, handle) = handle();
    let SummaryReport::ByTool(report) = handle
        .summary_report(SummaryReportOptions {
            mode: SummaryReportMode::ByTool,
            ..SummaryReportOptions::default()
        })
        .unwrap()
    else {
        panic!("expected the by-tool report");
    };
    assert_eq!(report.turn_count, 6);
}

#[test]
fn timeseries_buckets_count_requests() {
    let (_dir, handle) = handle();
    let series = handle
        .summary_timeseries(
            SummaryReportOptions {
                since: Some("2026-04-23T00:00:00Z".into()),
                until: Some("2026-04-23T00:00:30Z".into()),
                ..SummaryReportOptions::default()
            },
            60,
        )
        .unwrap();
    assert_eq!(series.buckets.len(), 1);
    assert_eq!(series.buckets[0].turn_count, 6);
}

#[test]
fn relationship_matches_count_requests() {
    let root = SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::Codex,
        session_id: CODEX.into(),
        related_session_id: None,
        relationship_type: RelationshipType::Root,
        ts: None,
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    };
    let matches = match_summary_relationships_to_turns(&[root], &turns(), &load_builtin_pricing());
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].turn_count, 5);
}

#[test]
fn sessions_count_requests() {
    let (_dir, handle) = handle();
    let cost = handle
        .session_cost(SessionCostOptions {
            session: Some(CODEX.into()),
            ..SessionCostOptions::default()
        })
        .unwrap();
    assert_eq!(cost.turn_count, 5);
    let listed = handle
        .sessions_list(SessionsListOptions::default())
        .unwrap();
    let mut counts: Vec<(&str, u64)> = listed
        .sessions
        .iter()
        .map(|s| (s.session_id.as_str(), s.turn_count))
        .collect();
    counts.sort();
    assert_eq!(counts, [(CLAUDE, 1), (CODEX, 5)]);
}

#[test]
fn compare_analyzes_requests() {
    let (_dir, handle) = handle();
    let opts = || CompareOptions {
        models: vec!["gpt-5.4".into(), "claude-sonnet-4-6".into()],
        since: Some("2026-04-23T00:00:00Z".into()),
        ..CompareOptions::default()
    };
    assert_eq!(handle.compare(opts()).unwrap().analyzed_turns, 6);
    let series = handle.compare_timeseries(opts(), 30 * 86_400).unwrap();
    assert_eq!(
        series
            .buckets
            .iter()
            .map(|b| b.result.analyzed_turns)
            .sum::<u64>(),
        6
    );
}

#[test]
fn a_zero_request_task_adds_no_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut handle = Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
    let mut idle = turn(SourceKind::Codex, CODEX, "t1", "gpt-5.4", Some(0));
    idle.usage = Usage::default();
    handle.raw_mut().append_turns(&[idle]).unwrap();
    assert_eq!(
        handle
            .summary(SummaryOptions::default())
            .unwrap()
            .turn_count,
        0
    );
}
