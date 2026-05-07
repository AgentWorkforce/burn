//! End-to-end test for all nine SDK verbs.
//!
//! Opens a fixture ledger in a `TempDir`, appends a small set of records
//! (one turn, one content blob with a known FTS token, one stamp), and
//! exercises every verb in both forms (LedgerHandle method + free
//! function). Assertions are structural — the goal is "the wrappers plumb
//! through correctly," not full parity with the TS sibling on real data.

use std::path::Path;

use tempfile::TempDir;

use relayburn_sdk::{
    export_ledger, export_stamps, hotspots, ingest, overhead, overhead_trim, search, session_cost,
    summary, ContentKind, ContentRecord, ContentRole, Enrichment, ExportLedgerOptions,
    ExportStampsOptions, HotspotsOptions, HotspotsResult, IngestOptions, IngestRoots, Ledger,
    LedgerOpenOptions, OverheadOptions, OverheadTrimOptions, SearchQueryOptions, SessionCostOptions,
    SourceKind, Stamp, StampSelector, SummaryOptions, ToolCall, TurnRecord, Usage,
};

const SESSION_ID: &str = "ses_integration_001";
const FTS_TOKEN: &str = "burnsearchneedle";

fn make_turn(model: &str) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: SESSION_ID.into(),
        session_path: None,
        message_id: "msg_1".into(),
        turn_index: 0,
        ts: "2026-04-23T00:00:00.000Z".into(),
        model: model.into(),
        project: None,
        project_key: None,
        usage: Usage {
            input: 100,
            output: 50,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        },
        tool_calls: vec![ToolCall {
            id: "toolu_1".into(),
            name: "Read".into(),
            target: None,
            args_hash: "abc123".into(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }],
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    }
}

fn make_content() -> ContentRecord {
    ContentRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: SESSION_ID.into(),
        message_id: "msg_1".into(),
        ts: "2026-04-23T00:00:00.000Z".into(),
        role: ContentRole::Assistant,
        kind: ContentKind::Text,
        text: Some(format!("hello world {FTS_TOKEN}")),
        tool_use: None,
        tool_result: None,
    }
}

fn make_stamp() -> Stamp {
    let mut enrichment = Enrichment::new();
    enrichment.insert("role".into(), "integration-test".into());
    Stamp::new(
        "2026-04-23T00:00:00.000Z",
        StampSelector {
            session_id: Some(SESSION_ID.into()),
            ..Default::default()
        },
        enrichment,
    )
    .expect("stamp")
}

fn populate(home: &Path) {
    let mut handle = Ledger::open(LedgerOpenOptions::with_home(home)).expect("open");
    let raw = handle.raw_mut();
    raw.append_turns(&[make_turn("claude-sonnet-4-6")])
        .expect("append turn");
    raw.append_content(&[make_content()]).expect("append content");
    raw.append_stamp(&make_stamp()).expect("append stamp");
}

#[test]
fn all_nine_verbs_round_trip_against_a_fixture_ledger() {
    let home = TempDir::new().expect("home tmp");
    populate(home.path());

    let handle = Ledger::open(LedgerOpenOptions::with_home(home.path())).expect("open");

    // 1. summary — handle + free
    let s = handle
        .summary(SummaryOptions::default())
        .expect("handle summary");
    assert_eq!(s.turn_count, 1);
    assert_eq!(s.by_model.len(), 1);
    assert_eq!(s.by_model[0].model, "claude-sonnet-4-6");
    assert!(s.total_tokens > 0);
    let s2 = summary(SummaryOptions {
        ledger_home: Some(home.path().to_path_buf()),
        ..Default::default()
    })
    .expect("free summary");
    assert_eq!(s2.turn_count, 1);

    // 2. session_cost — handle + free
    let sc = handle
        .session_cost(SessionCostOptions {
            session: Some(SESSION_ID.into()),
            ..Default::default()
        })
        .expect("handle session_cost");
    assert_eq!(sc.session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(sc.turn_count, 1);
    let sc2 = session_cost(SessionCostOptions {
        session: Some(SESSION_ID.into()),
        ledger_home: Some(home.path().to_path_buf()),
        ..Default::default()
    })
    .expect("free session_cost");
    assert_eq!(sc2.turn_count, 1);

    // 3. overhead — handle + free. No CLAUDE.md/AGENTS.md in the test
    // project, so we expect an empty result with a zero grand total.
    let project = TempDir::new().expect("project tmp");
    let oh = handle
        .overhead(OverheadOptions {
            project: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .expect("handle overhead");
    assert_eq!(oh.grand_total, 0.0);
    assert_eq!(oh.files.len(), 0);
    let _oh2 = overhead(OverheadOptions {
        project: Some(project.path().to_path_buf()),
        ledger_home: Some(home.path().to_path_buf()),
        ..Default::default()
    })
    .expect("free overhead");

    // 4. overhead_trim — handle + free. Same project, expect no
    // recommendations.
    let trim = handle
        .overhead_trim(OverheadTrimOptions {
            project: Some(project.path().to_path_buf()),
            ..Default::default()
        })
        .expect("handle overhead_trim");
    assert_eq!(trim.recommendations.len(), 0);
    assert_eq!(trim.summary.total_recommendations, 0);
    let _trim2 = overhead_trim(OverheadTrimOptions {
        project: Some(project.path().to_path_buf()),
        ledger_home: Some(home.path().to_path_buf()),
        ..Default::default()
    })
    .expect("free overhead_trim");

    // 5. hotspots — handle + free. The fixture turn has no fidelity
    // coverage, so attribution refuses; the discriminated union still
    // returns the attribution shape with `refused = true`.
    let h = handle
        .hotspots(HotspotsOptions::default())
        .expect("handle hotspots");
    match h {
        HotspotsResult::Attribution(_) => {}
        other => panic!("expected attribution shape, got {other:?}"),
    }
    let _h2 = hotspots(HotspotsOptions {
        ledger_home: Some(home.path().to_path_buf()),
        ..Default::default()
    })
    .expect("free hotspots");

    // 6. search — handle + free. The content body contains the unique
    // token, so an FTS query must find at least one hit.
    let sr = handle
        .search(SearchQueryOptions::new(FTS_TOKEN))
        .expect("handle search");
    assert!(!sr.hits.is_empty(), "search must hit the seeded blob");
    assert_eq!(sr.hits[0].session_id, SESSION_ID);
    let sr2 = search(SearchQueryOptions {
        query: FTS_TOKEN.into(),
        ledger_home: Some(home.path().to_path_buf()),
        ..Default::default()
    })
    .expect("free search");
    assert!(!sr2.hits.is_empty());

    // 7. export_ledger — handle + free. The seeded turn must show up as
    // at least one JSONL row of kind "turn".
    let ledger_rows: Vec<_> = handle
        .export_ledger(ExportLedgerOptions::default())
        .expect("handle export_ledger")
        .collect();
    assert!(!ledger_rows.is_empty(), "exported ledger must include the turn");
    assert!(
        ledger_rows
            .iter()
            .any(|v| v.get("kind").and_then(|k| k.as_str()) == Some("turn")),
        "expected at least one row with kind=turn",
    );
    let _ledger_rows2: Vec<_> = export_ledger(ExportLedgerOptions {
        ledger_home: Some(home.path().to_path_buf()),
    })
    .expect("free export_ledger")
    .collect();

    // 8. export_stamps — handle + free. The seeded stamp must round-trip.
    let stamp_rows: Vec<_> = handle
        .export_stamps(ExportStampsOptions::default())
        .expect("handle export_stamps")
        .collect();
    assert!(!stamp_rows.is_empty(), "exported stamps must include the stamp");
    assert!(
        stamp_rows
            .iter()
            .any(|v| v.get("kind").and_then(|k| k.as_str()) == Some("stamp")),
        "expected at least one row with kind=stamp",
    );
    let _stamp_rows2: Vec<_> = export_stamps(ExportStampsOptions {
        ledger_home: Some(home.path().to_path_buf()),
    })
    .expect("free export_stamps")
    .collect();
}

#[tokio::test]
async fn ingest_with_empty_roots_returns_zero_report_via_handle_and_free_fn() {
    // 9. ingest — handle + free. Both forms must accept empty roots and
    // return an all-zero report without scanning the developer's HOME.
    let home = TempDir::new().expect("home tmp");
    let claude = TempDir::new().expect("claude tmp");
    let codex = TempDir::new().expect("codex tmp");
    let opencode = TempDir::new().expect("opencode tmp");

    // `cleanup_stale_pending_stamps` and `load_config` inside ingest_all
    // honor RELAYBURN_HOME; pin it to the temp dir so the test never
    // touches `~/.agentworkforce/burn`.
    std::env::set_var("RELAYBURN_HOME", home.path());

    let mut handle = Ledger::open(LedgerOpenOptions::with_home(home.path())).expect("open");
    let report = handle
        .ingest(IngestOptions {
            ledger_home: Some(home.path().to_path_buf()),
            roots: IngestRoots {
                claude_projects_dir: Some(claude.path().to_path_buf()),
                codex_sessions_dir: Some(codex.path().to_path_buf()),
                opencode_storage_dir: Some(opencode.path().to_path_buf()),
            },
            ..Default::default()
        })
        .await
        .expect("handle ingest");
    assert_eq!(report.scanned_sessions, 0);
    assert_eq!(report.appended_turns, 0);

    let report2 = ingest(IngestOptions {
        ledger_home: Some(home.path().to_path_buf()),
        roots: IngestRoots {
            claude_projects_dir: Some(claude.path().to_path_buf()),
            codex_sessions_dir: Some(codex.path().to_path_buf()),
            opencode_storage_dir: Some(opencode.path().to_path_buf()),
        },
        ..Default::default()
    })
    .await
    .expect("free ingest");
    assert_eq!(report2.scanned_sessions, 0);
    assert_eq!(report2.appended_turns, 0);
}
