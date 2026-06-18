use super::flow::bucket_subagents_per_turn;
use super::summary::{
    aggregate_summary_relationship_stats, attribute_summary_cost_to_tools,
    collect_summary_agent_session_tree, collect_summary_connected_relationships, compute_summary,
    summary_subagent_session_filter, summary_tool_attribution_method, summary_turn_identity_key,
    SummaryRelationshipMatch,
};
use super::*;
use crate::reader::{RelationshipSourceKind, ToolCall, Usage, UserTurnBlock, UserTurnBlockKind};
use tempfile::TempDir;

fn fixture_handle() -> (TempDir, LedgerHandle) {
    let dir = tempfile::tempdir().unwrap();
    let opts = LedgerOpenOptions::with_home(dir.path());
    let mut handle = Ledger::open(opts).expect("open ledger");

    let turn1 = TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "sess-a".into(),
        session_path: None,
        message_id: "m-1".into(),
        turn_index: 0,
        ts: "2026-04-23T00:00:00.000Z".into(),
        model: "claude-sonnet-4-6".into(),
        project: Some("/tmp/proj".into()),
        project_key: None,
        usage: Usage {
            input: 1000,
            output: 500,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        },
        tool_calls: vec![ToolCall {
            id: "tu-1".into(),
            name: "Read".into(),
            target: Some("/tmp/proj/foo.rs".into()),
            args_hash: "h1".into(),
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
    };
    let turn2 = TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "sess-a".into(),
        session_path: None,
        message_id: "m-2".into(),
        turn_index: 1,
        ts: "2026-04-23T00:01:00.000Z".into(),
        model: "claude-sonnet-4-6".into(),
        project: Some("/tmp/proj".into()),
        project_key: None,
        usage: Usage {
            input: 800,
            output: 400,
            reasoning: 0,
            cache_read: 200,
            cache_create_5m: 0,
            cache_create_1h: 0,
        },
        tool_calls: vec![ToolCall {
            id: "tu-2".into(),
            name: "Read".into(),
            target: Some("/tmp/proj/foo.rs".into()),
            args_hash: "h1".into(),
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
    };
    handle
        .raw_mut()
        .append_turns(&[turn1, turn2])
        .expect("append turns");
    (dir, handle)
}

#[test]
fn normalize_since_relative_emits_canonical_mmm_z() {
    // Relative ranges must carry the `.000Z` suffix so the cutoff lex-sorts
    // *before* same-second ledger rows with non-zero millis.
    let v = normalize_since(Some("7d")).unwrap().unwrap();
    assert_eq!(v.len(), 24, "expected 24-char canonical shape: {v}");
    assert!(v.ends_with(".000Z"), "expected .000Z suffix in {v}");
}

#[test]
fn normalize_since_widens_no_fraction_iso_to_three_zeros() {
    // Same-second ledger row `...12.500Z` would sort *before* a `--since`
    // cutoff of `...12Z`, dropping valid turns. Canonicalizing widens to
    // `.000Z` so the cutoff is the lower bound for that second.
    assert_eq!(
        normalize_since(Some("2026-04-01T00:00:00Z"))
            .unwrap()
            .as_deref(),
        Some("2026-04-01T00:00:00.000Z"),
    );
}

#[test]
fn normalize_since_preserves_millisecond_precision() {
    assert_eq!(
        normalize_since(Some("2026-05-06T00:00:00.500Z"))
            .unwrap()
            .as_deref(),
        Some("2026-05-06T00:00:00.500Z"),
    );
    // Sub-millisecond digits are truncated to 3.
    assert_eq!(
        normalize_since(Some("2026-05-06T00:00:00.500999Z"))
            .unwrap()
            .as_deref(),
        Some("2026-05-06T00:00:00.500Z"),
    );
    // Shorter fraction is right-padded.
    assert_eq!(
        normalize_since(Some("2026-05-06T00:00:00.5Z"))
            .unwrap()
            .as_deref(),
        Some("2026-05-06T00:00:00.500Z"),
    );
}

#[test]
fn normalize_since_converts_negative_offset_to_utc() {
    // `-07:00` is 7h behind UTC → same wall-clock corresponds to a UTC
    // instant 7h later. 2026-05-06T00:00:00-07:00 == 2026-05-06T07:00:00Z.
    assert_eq!(
        normalize_since(Some("2026-05-06T00:00:00-07:00"))
            .unwrap()
            .as_deref(),
        Some("2026-05-06T07:00:00.000Z"),
    );
}

#[test]
fn normalize_since_converts_positive_offset_to_utc() {
    // 2026-05-06T00:00:00+09:00 == 2026-05-05T15:00:00Z.
    assert_eq!(
        normalize_since(Some("2026-05-06T00:00:00+09:00"))
            .unwrap()
            .as_deref(),
        Some("2026-05-05T15:00:00.000Z"),
    );
}

#[test]
fn normalize_since_accepts_lowercase_z_and_t() {
    assert_eq!(
        normalize_since(Some("2026-05-06t00:00:00.500z"))
            .unwrap()
            .as_deref(),
        Some("2026-05-06T00:00:00.500Z"),
    );
}

#[test]
fn normalize_since_accepts_date_only() {
    // No time component → midnight UTC.
    assert_eq!(
        normalize_since(Some("2026-05-06")).unwrap().as_deref(),
        Some("2026-05-06T00:00:00.000Z"),
    );
}

#[test]
fn normalize_since_rejects_garbage() {
    assert!(normalize_since(Some("zzz")).is_err());
    assert!(normalize_since(Some("2026/05/06")).is_err());
    assert!(normalize_since(Some("2026-13-01T00:00:00Z")).is_err());
    assert!(normalize_since(Some("2026-05-06T25:00:00Z")).is_err());
    assert!(normalize_since(Some("2026-05-06T00:00:00+9")).is_err());
}

#[test]
fn normalize_since_returns_none_for_empty() {
    assert!(normalize_since(None).unwrap().is_none());
    assert!(normalize_since(Some("")).unwrap().is_none());
}

#[test]
fn normalize_since_cutoff_lex_compatible_with_ledger_rows() {
    // Property: a `.000Z` cutoff must lex-sort at-or-before any same-second
    // ledger row with non-zero millis. This is the regression that broke
    // before canonicalization.
    let cutoff = "2026-05-06T12:00:00.000Z";
    assert!(cutoff <= "2026-05-06T12:00:00.500Z");
    assert!(cutoff <= "2026-05-06T12:00:00.001Z");
}

#[test]
fn ymd_days_round_trip() {
    for (y, m, d) in &[(1970, 1, 1), (2026, 5, 6), (2000, 2, 29), (1999, 12, 31)] {
        let days = ymd_to_days(*y, *m, *d).unwrap();
        let (ry, rm, rd) = days_to_ymd(days);
        assert_eq!((*y, *m, *d), (ry, rm, rd));
    }
}

#[test]
fn summary_aggregates_two_turns() {
    let (_dir, handle) = fixture_handle();
    let s = handle.summary(SummaryOptions::default()).unwrap();
    assert_eq!(s.turn_count, 2);
    assert_eq!(s.total_tokens, 1000 + 500 + 800 + 400 + 200);
    assert_eq!(s.by_model.len(), 1);
    assert_eq!(s.by_model[0].model, "claude-sonnet-4-6");
    assert_eq!(s.by_tool.len(), 1);
    assert_eq!(s.by_tool[0].tool, "Read");
    assert_eq!(s.by_tool[0].count, 2);
    assert!(s.total_cost > 0.0);
}

#[test]
fn summary_session_filter_narrows_to_session() {
    let (_dir, handle) = fixture_handle();
    let s = handle
        .summary(SummaryOptions {
            session: Some("nope".into()),
            ..SummaryOptions::default()
        })
        .unwrap();
    assert_eq!(s.turn_count, 0);
    assert_eq!(s.total_tokens, 0);
}

#[test]
fn summary_report_grouped_owns_rows_and_stable_fidelity_shape() {
    let (_dir, handle) = fixture_handle();
    let report = handle
        .summary_report(SummaryReportOptions::default())
        .expect("summary report");
    let SummaryReport::Grouped(grouped) = report else {
        panic!("expected grouped report");
    };

    assert_eq!(grouped.turn_count, 2);
    assert_eq!(grouped.group_by, SummaryGroupBy::Model);
    assert_eq!(grouped.rows.len(), 1);
    assert_eq!(grouped.rows[0].label, "claude-sonnet-4-6");
    assert_eq!(grouped.per_cell_fidelity["groupBy"], "model");
    assert!(summary_fidelity_summary_to_value(&grouped.fidelity)["byClass"].is_object());
}

/// Acceptance test for issue #437: a turn carrying `stop_reason:
/// "max_tokens"` surfaces in the summary outcome counts. Mixes a
/// `max_tokens` turn with a `none` turn (no field on the row) to
/// confirm both buckets land in the right slot.
#[test]
fn summary_report_aggregates_stop_reasons_per_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let opts = LedgerOpenOptions::with_home(dir.path());
    let mut handle = Ledger::open(opts).expect("open ledger");

    let make_turn = |idx: u64, msg: &str, stop_reason: Option<StopReason>| -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "sess-stop".into(),
            session_path: None,
            message_id: msg.into(),
            turn_index: idx,
            ts: format!("2026-05-25T00:0{idx}:00.000Z"),
            model: "claude-sonnet-4-6".into(),
            project: Some("/tmp/proj".into()),
            project_key: None,
            usage: Usage {
                input: 100 + idx,
                output: 50,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: vec![],
            files_touched: None,
            subagent: None,
            stop_reason,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    };

    handle
        .raw_mut()
        .append_turns(&[
            make_turn(0, "m-max", Some(StopReason::MaxTokens)),
            make_turn(1, "m-end", Some(StopReason::EndTurn)),
            make_turn(2, "m-refusal", Some(StopReason::Refusal)),
            // Codex-style: no field on the row.
            make_turn(3, "m-none", None),
        ])
        .expect("append turns");

    let report = handle
        .summary_report(SummaryReportOptions::default())
        .expect("summary report");
    let SummaryReport::Grouped(grouped) = report else {
        panic!("expected grouped report");
    };
    assert_eq!(grouped.turn_count, 4);
    assert_eq!(grouped.stop_reasons.max_tokens, 1);
    assert_eq!(grouped.stop_reasons.end_turn, 1);
    assert_eq!(grouped.stop_reasons.refusal, 1);
    assert_eq!(grouped.stop_reasons.none, 1);
    // Untouched buckets stay at zero.
    assert_eq!(grouped.stop_reasons.tool_use, 0);
    assert_eq!(grouped.stop_reasons.pause_turn, 0);
    assert_eq!(grouped.stop_reasons.silent, 0);
    assert!(!grouped.stop_reasons.is_empty());
}

/// `compute_summary` (the slim legacy verb) populates unpriced_turns
/// and unpriced_models when a turn's model is absent from the pricing table.
#[test]
fn compute_summary_tracks_unpriced_turns_and_models() {
    let pricing = load_pricing(None);
    let unknown_model = "made-up-model-xyz";
    let priced_turn = TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "s".into(),
        session_path: None,
        message_id: "m-priced".into(),
        turn_index: 0,
        ts: "2026-05-01T00:00:00.000Z".into(),
        model: "claude-sonnet-4-6".into(),
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
        tool_calls: vec![],
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    };
    let unpriced_turn = TurnRecord {
        message_id: "m-unpriced".into(),
        turn_index: 1,
        ts: "2026-05-01T00:01:00.000Z".into(),
        model: unknown_model.into(),
        ..priced_turn.clone()
    };
    let turns = vec![priced_turn, unpriced_turn];
    let s = compute_summary(&turns, &pricing);
    assert_eq!(s.turn_count, 2);
    assert_eq!(s.unpriced_turns, 1, "one turn uses an unknown model");
    assert_eq!(
        s.unpriced_models,
        vec![unknown_model],
        "unknown model listed exactly once"
    );
}

/// `summary_report` (grouped mode) surfaces unpriced turn count and model
/// names when a turn's model is absent from the pricing snapshot.
#[test]
fn summary_report_grouped_tracks_unpriced_turns_and_models() {
    let dir = tempfile::tempdir().unwrap();
    let opts = LedgerOpenOptions::with_home(dir.path());
    let mut handle = Ledger::open(opts).expect("open ledger");

    let make_turn = |idx: u64, msg: &str, model: &str, ts: &str| -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "sess-unpriced".into(),
            session_path: None,
            message_id: msg.into(),
            turn_index: idx,
            ts: ts.into(),
            model: model.into(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 100 + idx,
                output: 50,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: vec![],
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    };

    handle
        .raw_mut()
        .append_turns(&[
            make_turn(
                0,
                "m-known",
                "claude-sonnet-4-6",
                "2026-05-01T00:00:00.000Z",
            ),
            make_turn(
                1,
                "m-unknown-1",
                "made-up-model-xyz",
                "2026-05-01T00:01:00.000Z",
            ),
            make_turn(
                2,
                "m-unknown-2",
                "made-up-model-xyz",
                "2026-05-01T00:02:00.000Z",
            ),
        ])
        .expect("append turns");

    let report = handle
        .summary_report(SummaryReportOptions::default())
        .expect("summary report");
    let SummaryReport::Grouped(grouped) = report else {
        panic!("expected grouped report");
    };

    assert_eq!(grouped.turn_count, 3);
    assert_eq!(
        grouped.unpriced_turns, 2,
        "two turns used the unknown model"
    );
    assert_eq!(
        grouped.unpriced_models,
        vec!["made-up-model-xyz"],
        "unknown model listed exactly once"
    );
    // The priced turn's cost must be non-zero; total must equal the priced portion.
    assert!(
        grouped.total_cost.total > 0.0,
        "priced turn must contribute positive cost"
    );
}

/// Acceptance test for issue #437: the legacy `LedgerHandle::summary`
/// surface (the slim one) also exposes the new counts. Verifies a turn
/// without a stop_reason field round-trips to `None`/`none` rather
/// than silently leaking into a real bucket.
#[test]
fn summary_legacy_surface_includes_stop_reason_counts_with_none_for_missing_field() {
    let dir = tempfile::tempdir().unwrap();
    let opts = LedgerOpenOptions::with_home(dir.path());
    let mut handle = Ledger::open(opts).expect("open ledger");

    let mut turn = TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "sess-legacy".into(),
        session_path: None,
        message_id: "m-legacy".into(),
        turn_index: 0,
        ts: "2026-05-25T00:00:00.000Z".into(),
        model: "claude-sonnet-4-6".into(),
        project: None,
        project_key: None,
        usage: Usage::default(),
        tool_calls: vec![],
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    };
    handle
        .raw_mut()
        .append_turns(&[turn.clone()])
        .expect("append none turn");
    turn.message_id = "m-pause".into();
    turn.turn_index = 1;
    turn.ts = "2026-05-25T00:01:00.000Z".into();
    turn.stop_reason = Some(StopReason::PauseTurn);
    handle
        .raw_mut()
        .append_turns(&[turn])
        .expect("append pause turn");

    let s = handle.summary(SummaryOptions::default()).expect("summary");
    assert_eq!(s.turn_count, 2);
    assert_eq!(s.stop_reasons.none, 1);
    assert_eq!(s.stop_reasons.pause_turn, 1);
    assert_eq!(s.stop_reasons.end_turn, 0);
}

/// Issue #449 review follow-up: when no filters are set, the
/// subagent count helper must return `None` so the underlying
/// walker preserves its original "count every reachable session"
/// behavior (the global-summary path).
#[test]
fn summary_subagent_session_filter_returns_none_for_unfiltered_summary() {
    let opts = SummaryReportOptions::default();
    let turns: Vec<TurnRecord> = Vec::new();
    assert!(summary_subagent_session_filter(&opts, &turns).is_none());
}

/// Issue #449 review follow-up: when `--session` (or any other
/// scoping filter) is active, the subagent count helper must
/// return `Some(set)` containing exactly the session ids that
/// survived filtering. This is the linkage that stops the
/// `subagents: X paired, Y orphan` line from including sidecars
/// from sessions the user excluded.
#[test]
fn summary_subagent_session_filter_collects_session_ids_when_filtered() {
    let opts = SummaryReportOptions {
        session: Some("sess-a".into()),
        ..SummaryReportOptions::default()
    };
    let mk = |session_id: &str| TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session_id.into(),
        session_path: None,
        message_id: format!("m-{session_id}"),
        turn_index: 0,
        ts: "2026-04-23T00:00:00.000Z".into(),
        model: "claude-sonnet-4-6".into(),
        project: None,
        project_key: None,
        usage: Usage::default(),
        tool_calls: vec![],
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    };
    let turns = vec![mk("sess-a"), mk("sess-a")];
    let filter = summary_subagent_session_filter(&opts, &turns)
        .expect("expected Some(set) when --session is active");
    assert!(filter.contains("sess-a"));
    assert_eq!(filter.len(), 1, "duplicates collapse into the set");
}

/// Each non-default filter on `SummaryReportOptions` must flip the
/// helper into "filtered" mode. Iterating over the surface keeps
/// us from quietly losing scoping when a new filter is added.
#[test]
fn summary_subagent_session_filter_treats_every_filter_as_scoping() {
    let turns: Vec<TurnRecord> = Vec::new();
    let cases: Vec<(&str, SummaryReportOptions)> = vec![
        (
            "project",
            SummaryReportOptions {
                project: Some("/tmp/proj".into()),
                ..SummaryReportOptions::default()
            },
        ),
        (
            "since",
            SummaryReportOptions {
                since: Some("24h".into()),
                ..SummaryReportOptions::default()
            },
        ),
        (
            "workflow",
            SummaryReportOptions {
                workflow: Some("wf-1".into()),
                ..SummaryReportOptions::default()
            },
        ),
        (
            "agent",
            SummaryReportOptions {
                agent: Some("agent-x".into()),
                ..SummaryReportOptions::default()
            },
        ),
        (
            "providers",
            SummaryReportOptions {
                providers: Some(vec!["anthropic".into()]),
                ..SummaryReportOptions::default()
            },
        ),
        (
            "tags",
            SummaryReportOptions {
                tags: Some({
                    let mut m = BTreeMap::new();
                    m.insert("k".into(), "v".into());
                    m
                }),
                ..SummaryReportOptions::default()
            },
        ),
    ];
    for (label, opts) in cases {
        assert!(
            summary_subagent_session_filter(&opts, &turns).is_some(),
            "expected filter to engage for {label}"
        );
    }
}

#[test]
fn summary_report_by_tool_uses_predecessor_before_since_boundary() {
    let (_dir, handle) = fixture_handle();
    let report = handle
        .summary_report(SummaryReportOptions {
            since: Some("2026-04-23T00:00:30.000Z".to_string()),
            mode: SummaryReportMode::ByTool,
            ..SummaryReportOptions::default()
        })
        .expect("summary report");
    let SummaryReport::ByTool(report) = report else {
        panic!("expected by-tool report");
    };

    let read = report
        .rows
        .iter()
        .find(|row| row.tool == "Read")
        .expect("read row");
    assert_eq!(report.turn_count, 1);
    assert_eq!(read.calls, 1);
    assert!(read.attributed_cost > 0.0);
    assert_eq!(report.unattributed_cost, 0.0);
}

#[test]
fn summary_replacement_savings_value_tie_breaks_by_tool_name() {
    let mut savings = ReplacementSavingsSummary {
        calls: 2,
        collapsed_calls: 4,
        estimated_tokens_saved: 20,
        by_tool: IndexMap::new(),
    };
    savings.by_tool.insert(
        "Write".to_string(),
        ToolSavingsAggregate {
            calls: 1,
            collapsed_calls: 2,
            estimated_tokens_saved: 10,
        },
    );
    savings.by_tool.insert(
        "Read".to_string(),
        ToolSavingsAggregate {
            calls: 1,
            collapsed_calls: 2,
            estimated_tokens_saved: 10,
        },
    );

    let value = summary_replacement_savings_to_value(&savings);
    let by_tool = value["byTool"].as_array().expect("byTool array");

    assert_eq!(by_tool[0]["tool"], "Read");
    assert_eq!(by_tool[1]["tool"], "Write");
}

#[test]
fn summary_agent_session_tree_follows_nested_child_sessions_and_agent_ids() {
    let rels = vec![
        relationship("child-session", "root-agent", Some("child-agent")),
        relationship("grandchild-session", "child-session", Some("grand-agent")),
        relationship("great-grandchild-session", "child-agent", None),
    ];

    let sessions = collect_summary_agent_session_tree(&rels, "root-agent");

    assert!(sessions.contains("child-session"));
    assert!(sessions.contains("grandchild-session"));
    assert!(sessions.contains("great-grandchild-session"));
    assert_eq!(sessions.len(), 3);
}

#[test]
fn summary_connected_relationships_follow_related_sessions_and_agent_ids() {
    let rels = vec![
        relationship("child-session", "root-session", Some("child-agent")),
        relationship("grandchild-session", "child-agent", None),
        relationship("unrelated-session", "other-session", None),
    ];

    let connected = collect_summary_connected_relationships(&rels, "root-session");
    let session_ids: HashSet<&str> = connected.iter().map(|r| r.session_id.as_str()).collect();

    assert!(session_ids.contains("child-session"));
    assert!(session_ids.contains("grandchild-session"));
    assert!(!session_ids.contains("unrelated-session"));
}

#[test]
fn summary_relationship_stats_count_relationships_separately_from_sessions() {
    let matches = vec![
        SummaryRelationshipMatch {
            relationship_type: RelationshipType::Fork,
            session_id: "session".to_string(),
            subagent_type: None,
            turn_count: 2,
            cost: 1.0,
        },
        SummaryRelationshipMatch {
            relationship_type: RelationshipType::Fork,
            session_id: "session".to_string(),
            subagent_type: None,
            turn_count: 3,
            cost: 2.0,
        },
    ];

    let stats = aggregate_summary_relationship_stats(&matches);
    let fork = stats
        .iter()
        .find(|s| s.relationship_type == RelationshipType::Fork)
        .expect("fork stats");

    assert_eq!(fork.count, 2);
    assert_eq!(fork.session_count, 1);
    assert_eq!(fork.turn_count, 5);
    assert_eq!(fork.total_cost, 3.0);
}

#[test]
fn summary_by_tool_attribution_uses_user_turn_block_byte_shares() {
    let pricing = load_pricing(None);
    let turns = vec![
        summary_test_turn(
            0,
            "assistant-1",
            Usage::default(),
            vec![
                summary_test_tool("call-read", "Read"),
                summary_test_tool("call-edit", "Edit"),
            ],
        ),
        summary_test_turn(
            1,
            "assistant-2",
            Usage {
                input: 1_000,
                ..Usage::default()
            },
            Vec::new(),
        ),
    ];
    let mut user_turns_by_session = HashMap::new();
    user_turns_by_session.insert(
        "session".to_string(),
        vec![UserTurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "session".to_string(),
            user_uuid: "user-1".to_string(),
            ts: "2026-04-20T00:00:01.000Z".to_string(),
            preceding_message_id: Some("assistant-1".to_string()),
            following_message_id: Some("assistant-2".to_string()),
            blocks: vec![
                summary_test_tool_result_block("call-read", 75),
                summary_test_tool_result_block("call-edit", 25),
            ],
        }],
    );

    let (by_tool, unattributed) =
        attribute_summary_cost_to_tools(&turns, &pricing, &user_turns_by_session, None);
    let read = by_tool.get("Read").expect("read agg");
    let edit = by_tool.get("Edit").expect("edit agg");

    assert_eq!(read.calls, 1);
    assert_eq!(edit.calls, 1);
    assert!(read.cost > edit.cost * 2.9);
    assert!(read.cost < edit.cost * 3.1);
    assert!(unattributed.abs() < 1e-12);
    assert_eq!(
        summary_tool_attribution_method(read),
        SummaryToolAttributionMethod::Sized
    );
}

#[test]
fn summary_by_tool_attribution_uses_real_predecessor_when_selection_skips_turns() {
    let pricing = load_pricing(None);
    let turns = vec![
        summary_test_turn(
            0,
            "assistant-1",
            Usage::default(),
            vec![summary_test_tool("call-read", "Read")],
        ),
        summary_test_turn(
            1,
            "assistant-2",
            Usage::default(),
            vec![summary_test_tool("call-edit", "Edit")],
        ),
        summary_test_turn(
            2,
            "assistant-3",
            Usage {
                input: 1_000,
                ..Usage::default()
            },
            Vec::new(),
        ),
    ];
    let selected = HashSet::from([summary_turn_identity_key(&turns[2])]);
    let user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();

    let (by_tool, unattributed) =
        attribute_summary_cost_to_tools(&turns, &pricing, &user_turns_by_session, Some(&selected));

    let edit = by_tool.get("Edit").expect("edit agg");
    assert_eq!(edit.calls, 1);
    assert!(edit.cost > 0.0);
    assert_eq!(by_tool.get("Read").map(|agg| agg.cost).unwrap_or(0.0), 0.0);
    assert_eq!(unattributed, 0.0);
}

#[test]
fn session_cost_returns_note_when_session_missing() {
    let (_dir, handle) = fixture_handle();
    let r = handle.session_cost(SessionCostOptions::default()).unwrap();
    assert!(r.session_id.is_none());
    assert_eq!(r.note.as_deref(), Some("no session id provided"));
    assert_eq!(r.turn_count, 0);
}

#[test]
fn session_cost_aggregates_turns_for_known_session() {
    let (_dir, handle) = fixture_handle();
    let r = handle
        .session_cost(SessionCostOptions {
            session: Some("sess-a".into()),
            ..SessionCostOptions::default()
        })
        .unwrap();
    assert_eq!(r.session_id.as_deref(), Some("sess-a"));
    assert_eq!(r.turn_count, 2);
    assert_eq!(r.models, vec!["claude-sonnet-4-6".to_string()]);
    assert!(r.total_usd > 0.0);
    assert!(r.note.is_none());
}

#[test]
fn session_cost_known_session_with_no_turns_emits_note() {
    let (_dir, handle) = fixture_handle();
    let r = handle
        .session_cost(SessionCostOptions {
            session: Some("ghost".into()),
            ..SessionCostOptions::default()
        })
        .unwrap();
    assert_eq!(r.session_id.as_deref(), Some("ghost"));
    assert_eq!(r.turn_count, 0);
    assert_eq!(
        r.note.as_deref(),
        Some("no turns recorded for this session yet")
    );
}

#[test]
fn overhead_returns_empty_when_no_files_present() {
    let (_dir, handle) = fixture_handle();
    let project = tempfile::tempdir().unwrap();
    let r = handle
        .overhead(OverheadOptions {
            project: Some(project.path().to_path_buf()),
            ..OverheadOptions::default()
        })
        .unwrap();
    assert!(r.files.is_empty());
    assert!(r.per_file.is_empty());
    assert_eq!(r.grand_total, 0.0);
}

#[test]
fn overhead_attributes_when_claude_md_present() {
    let (_dir, handle) = fixture_handle();
    let project = tempfile::tempdir().unwrap();
    let body = format!("## Section\n{}", "x".repeat(800));
    std::fs::write(project.path().join("CLAUDE.md"), &body).unwrap();
    let r = handle
        .overhead(OverheadOptions {
            project: Some(project.path().to_path_buf()),
            ..OverheadOptions::default()
        })
        .unwrap();
    assert_eq!(r.files.len(), 1);
    assert_eq!(r.per_file.len(), 1);
    assert_eq!(r.files[0].kind, OverheadFileKind::ClaudeMd);
}

#[test]
fn overhead_trim_emits_summary_when_claude_md_present() {
    let (_dir, handle) = fixture_handle();
    let project = tempfile::tempdir().unwrap();
    let body = format!(
        "## Big\n{}\n\n## Small\n{}\n",
        "y".repeat(8000),
        "z".repeat(200)
    );
    std::fs::write(project.path().join("CLAUDE.md"), &body).unwrap();
    let r = handle
        .overhead_trim(OverheadTrimOptions {
            project: Some(project.path().to_path_buf()),
            top: Some(1),
            ..OverheadTrimOptions::default()
        })
        .unwrap();
    // The fixture's turns have cache_read=0/200 — well below this
    // CLAUDE.md's token count — so attribution sees no rides and total
    // cost is 0. `build_trim_recommendations` still emits a top-N row
    // per non-preamble section, with projected savings = 0; that's the
    // contract. With `top=1` and two H2 sections in the file, we get
    // a single recommendation.
    assert_eq!(r.summary.files_analyzed, 1);
    assert_eq!(r.recommendations.len(), 1);
    assert_eq!(r.recommendations[0].projected_savings.per_session_usd, 0.0);
    assert!(r.recommendations[0].diff.is_some());
    assert_eq!(r.since, "all time");
}

#[test]
fn hotspots_returns_attribution_shape_by_default() {
    let (_dir, handle) = fixture_handle();
    let r = handle.hotspots(HotspotsOptions::default()).unwrap();
    match r {
        HotspotsResult::Attribution(a) => {
            // Our turns lack `fidelity` (None), so the coverage gate
            // passes — both turns are eligible.
            assert_eq!(a.turns_analyzed, 2);
            assert!(a.grand_total >= 0.0);
            assert_eq!(a.fidelity.analyzed, 2);
            assert_eq!(a.fidelity.excluded, 0);
        }
        other => panic!("expected attribution, got {other:?}"),
    }
}

#[test]
fn hotspots_group_by_file_returns_file_kind() {
    let (_dir, handle) = fixture_handle();
    let r = handle
        .hotspots(HotspotsOptions {
            group_by: Some(HotspotsGroupBy::File),
            ..HotspotsOptions::default()
        })
        .unwrap();
    match r {
        HotspotsResult::File { rows, refused, .. } => {
            assert!(refused.is_none());
            // Two `Read` calls on /tmp/proj/foo.rs collapse into 1 row.
            assert!(rows.len() <= 1);
        }
        other => panic!("expected file, got {other:?}"),
    }
}

#[test]
fn hotspots_with_patterns_returns_findings_kind() {
    let (_dir, handle) = fixture_handle();
    let r = handle
        .hotspots(HotspotsOptions {
            patterns: Some(vec!["retry-loop".into()]),
            ..HotspotsOptions::default()
        })
        .unwrap();
    match r {
        HotspotsResult::Findings { findings, summary } => {
            // No retries in fixture, so findings is empty — but the
            // kind:findings shape and summary block should still ship.
            assert!(findings.is_empty());
            assert!(summary.is_object());
        }
        other => panic!("expected findings, got {other:?}"),
    }
}

#[test]
fn hotspots_group_by_findings_returns_findings_kind() {
    let (_dir, handle) = fixture_handle();
    let r = handle
        .hotspots(HotspotsOptions {
            group_by: Some(HotspotsGroupBy::Findings),
            ..HotspotsOptions::default()
        })
        .unwrap();
    match r {
        HotspotsResult::Findings { findings, summary } => {
            assert!(findings.iter().all(|f| !f.kind.is_empty()));
            assert!(summary.is_object());
        }
        other => panic!("expected findings, got {other:?}"),
    }
}

#[test]
fn hotspots_group_by_findings_honors_patterns_filter() {
    let (_dir, handle) = fixture_handle();
    let r = handle
        .hotspots(HotspotsOptions {
            group_by: Some(HotspotsGroupBy::Findings),
            patterns: Some(vec!["retry-loop".into()]),
            ..HotspotsOptions::default()
        })
        .unwrap();
    match r {
        HotspotsResult::Findings { findings, summary } => {
            assert!(findings.is_empty());
            assert!(summary.is_object());
        }
        other => panic!("expected findings, got {other:?}"),
    }
}

#[test]
fn hotspots_session_filter_narrows_to_session() {
    let (_dir, handle) = fixture_handle();
    // Match: fixture has 2 turns under sess-a.
    let r_match = handle
        .hotspots(HotspotsOptions {
            session: Some("sess-a".into()),
            ..HotspotsOptions::default()
        })
        .unwrap();
    match r_match {
        HotspotsResult::Attribution(a) => assert_eq!(a.turns_analyzed, 2),
        other => panic!("expected attribution, got {other:?}"),
    }
    // No match: nonexistent session id.
    let r_none = handle
        .hotspots(HotspotsOptions {
            session: Some("ghost".into()),
            ..HotspotsOptions::default()
        })
        .unwrap();
    match r_none {
        HotspotsResult::Attribution(a) => assert_eq!(a.turns_analyzed, 0),
        other => panic!("expected attribution, got {other:?}"),
    }
}

#[test]
fn hotspots_workflow_filter_uses_enrichment_stamp() {
    let (_dir, mut handle) = fixture_handle();
    let mut enrichment = crate::Enrichment::new();
    enrichment.insert("workflowId".into(), "wf-1".into());
    let stamp = crate::Stamp::new(
        "2026-04-23T00:00:30.000Z",
        crate::StampSelector {
            session_id: Some("sess-a".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    handle.raw_mut().append_stamp(&stamp).unwrap();

    // Match: sess-a turns are stamped with wf-1.
    let r_match = handle
        .hotspots(HotspotsOptions {
            workflow: Some("wf-1".into()),
            ..HotspotsOptions::default()
        })
        .unwrap();
    match r_match {
        HotspotsResult::Attribution(a) => assert_eq!(a.turns_analyzed, 2),
        other => panic!("expected attribution, got {other:?}"),
    }
    // No match: a workflow id no stamp folds onto.
    let r_none = handle
        .hotspots(HotspotsOptions {
            workflow: Some("wf-missing".into()),
            ..HotspotsOptions::default()
        })
        .unwrap();
    match r_none {
        HotspotsResult::Attribution(a) => assert_eq!(a.turns_analyzed, 0),
        other => panic!("expected attribution, got {other:?}"),
    }
}

#[test]
fn hotspots_provider_filter_drops_non_matching_provider() {
    let (_dir, handle) = fixture_handle();
    // Both fixture turns are claude-sonnet-4-6 (provider=anthropic);
    // filtering to anthropic keeps them, filtering to openai drops them.
    let keep = handle
        .hotspots(HotspotsOptions {
            provider: Some(vec!["anthropic".into()]),
            ..HotspotsOptions::default()
        })
        .unwrap();
    match keep {
        HotspotsResult::Attribution(a) => assert_eq!(a.turns_analyzed, 2),
        other => panic!("expected attribution, got {other:?}"),
    }
    let drop = handle
        .hotspots(HotspotsOptions {
            provider: Some(vec!["openai".into()]),
            ..HotspotsOptions::default()
        })
        .unwrap();
    match drop {
        HotspotsResult::Attribution(a) => assert_eq!(a.turns_analyzed, 0),
        other => panic!("expected attribution, got {other:?}"),
    }
}

#[test]
fn compare_requires_at_least_two_models() {
    let (_dir, handle) = fixture_handle();
    let err = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into()],
            ..CompareOptions::default()
        })
        .unwrap_err();
    assert!(err.to_string().contains("needs at least 2 models"));
}

#[test]
fn compare_returns_flat_cells_and_absent_models() {
    let (_dir, handle) = fixture_handle();
    let r = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(r.analyzed_turns, 2);
    assert_eq!(r.min_sample, 5);
    assert!(r.models.contains(&"claude-sonnet-4-6".to_string()));
    assert!(r.models.contains(&"claude-haiku-4-5".to_string()));
    assert!(r
        .cells
        .iter()
        .any(|c| c.model == "claude-sonnet-4-6" && c.turns == 2));
    assert_eq!(r.fidelity.minimum, FidelityClass::Partial);
    assert_eq!(r.fidelity.excluded.total, 0);

    let json = serde_json::to_value(&r).unwrap();
    assert!(json["fidelity"]["summary"]["byClass"].is_object());
    assert!(json["fidelity"]["summary"]["missingCoverage"].is_object());
}

#[test]
fn compare_metadata_counts_all_matched_turns_pre_models_filter() {
    // TS-parity contract: `analyzedTurns` and `fidelity.summary` describe
    // the slice the comparison was *drawn from* — i.e. all turns passing
    // (since/until/project/session/source/provider) and the fidelity
    // gate. The `models` allow-list is honored by the cell builder, NOT
    // by these top-level metadata counts. A `claude-opus-4-5` turn that
    // is not in the requested-models list still counts toward
    // `analyzedTurns` and `summary.total`, but does not appear in the
    // `models` / `totals` rows. This mirrors `packages/sdk/index.js
    // ::compare` where `analyzedTurns = filteredTurns.length` is taken
    // before `buildCompareTable` applies the model filter.
    let (_dir, mut handle) = fixture_handle();
    let extra = TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "sess-b".into(),
        session_path: None,
        message_id: "m-extra".into(),
        turn_index: 0,
        ts: "2026-04-23T00:02:00.000Z".into(),
        model: "claude-opus-4-5".into(),
        project: Some("/tmp/proj".into()),
        project_key: None,
        usage: Usage {
            input: 100,
            output: 50,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        },
        tool_calls: vec![],
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    };
    handle.raw_mut().append_turns(&[extra]).unwrap();

    let r = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();

    assert_eq!(r.analyzed_turns, 3);
    assert_eq!(r.fidelity.summary.total, 3);
    // The unrequested model is excluded from cells/totals/models, even
    // though it counts toward analyzed_turns + fidelity summary above.
    assert!(!r.models.contains(&"claude-opus-4-5".to_string()));
    assert!(!r.totals.contains_key("claude-opus-4-5"));
}

#[test]
fn compare_reports_full_fidelity_summary_when_no_requested_model_appears() {
    // Regression for the α-followup PR #355 conformance miss: when the
    // caller asks to compare two models that don't appear in the
    // ledger at all, `analyzedTurns` and `fidelity.summary` MUST still
    // describe the underlying slice — not zero. The TS contract from
    // `packages/sdk/index.js::compare` builds these counters from
    // `filteredTurns.length` (post-fidelity-gate, pre-models-filter);
    // an earlier Rust implementation pre-filtered by `opts.models`,
    // collapsing the metadata to zeros and breaking the conformance
    // gate even though models extraction worked.
    let (_dir, handle) = fixture_handle();

    let r = handle
        .compare(CompareOptions {
            // Neither model exists in the fixture.
            models: vec!["claude-sonnet-4-5".into(), "claude-opus-4-7".into()],
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();

    // Fixture has 2 sonnet-4-6 turns. Neither requested model matches
    // them, but the metadata still describes the slice.
    assert_eq!(r.analyzed_turns, 2);
    assert_eq!(r.fidelity.summary.total, 2);
    // Requested models stay visible as all-empty columns (compare
    // pre-seeds the model allow-list).
    assert!(r.models.contains(&"claude-sonnet-4-5".to_string()));
    assert!(r.models.contains(&"claude-opus-4-7".to_string()));
    // `claude-sonnet-4-6` is in the ledger but not requested, so it
    // does NOT appear in the result rows even though it contributed
    // to `analyzed_turns`.
    assert!(!r.models.contains(&"claude-sonnet-4-6".to_string()));
    // Every cell for the requested-but-absent models is no_data.
    for cell in &r.cells {
        assert!(cell.no_data, "expected no_data for cell {cell:?}");
        assert_eq!(cell.turns, 0);
    }
}

#[test]
fn compare_filters_by_folded_workflow_enrichment() {
    let (_dir, mut handle) = fixture_handle();
    let mut enrichment = crate::Enrichment::new();
    enrichment.insert("workflowId".into(), "wf-1".into());
    let stamp = crate::Stamp::new(
        "2026-04-22T00:00:00.000Z",
        crate::StampSelector {
            session_id: Some("sess-a".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    handle.raw_mut().append_stamp(&stamp).unwrap();

    let matched = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            workflow: Some("wf-1".into()),
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(matched.analyzed_turns, 2);

    let missed = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            workflow: Some("wf-missing".into()),
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(missed.analyzed_turns, 0);
}

#[test]
fn compare_filters_by_folded_agent_enrichment() {
    let (_dir, mut handle) = fixture_handle();
    let mut enrichment = crate::Enrichment::new();
    enrichment.insert("agentId".into(), "agent-7".into());
    let stamp = crate::Stamp::new(
        "2026-04-22T00:00:00.000Z",
        crate::StampSelector {
            session_id: Some("sess-a".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    handle.raw_mut().append_stamp(&stamp).unwrap();

    let matched = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            agent: Some("agent-7".into()),
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(matched.analyzed_turns, 2);

    let missed = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            agent: Some("agent-missing".into()),
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(missed.analyzed_turns, 0);
}

#[test]
fn compare_filters_by_effective_provider() {
    // Fixture has 2 sonnet-4-6 turns (collector-implied `anthropic`).
    // A matching filter keeps them; a non-matching one drops them.
    let (_dir, handle) = fixture_handle();

    let matched = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            provider: Some(vec!["anthropic".into()]),
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(matched.analyzed_turns, 2);

    let missed = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            provider: Some(vec!["openai".into()]),
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(missed.analyzed_turns, 0);

    // Case-insensitive: upper-case input must still match `anthropic`.
    let mixed_case = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            provider: Some(vec!["ANTHROPIC".into()]),
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(mixed_case.analyzed_turns, 2);
}

#[test]
fn compare_filters_by_workflow_and_agent_intersection() {
    // `Query.enrichment` is AND-semantics: every key/value pair must
    // match. Pin that here so a future drift to OR-semantics regresses
    // visibly. Stamp folds both keys onto sess-a's two turns.
    let (_dir, mut handle) = fixture_handle();
    let mut enrichment = crate::Enrichment::new();
    enrichment.insert("workflowId".into(), "wf-1".into());
    enrichment.insert("agentId".into(), "agent-7".into());
    let stamp = crate::Stamp::new(
        "2026-04-22T00:00:00.000Z",
        crate::StampSelector {
            session_id: Some("sess-a".into()),
            ..Default::default()
        },
        enrichment,
    )
    .unwrap();
    handle.raw_mut().append_stamp(&stamp).unwrap();

    let matched = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            workflow: Some("wf-1".into()),
            agent: Some("agent-7".into()),
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(matched.analyzed_turns, 2);

    // Workflow matches but agent does not → 0.
    let agent_missing = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            workflow: Some("wf-1".into()),
            agent: Some("agent-missing".into()),
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(agent_missing.analyzed_turns, 0);

    // Agent matches but workflow does not → 0.
    let workflow_missing = handle
        .compare(CompareOptions {
            models: vec!["claude-sonnet-4-6".into(), "claude-haiku-4-5".into()],
            workflow: Some("wf-missing".into()),
            agent: Some("agent-7".into()),
            min_fidelity: Some(FidelityClass::Partial),
            ..CompareOptions::default()
        })
        .unwrap();
    assert_eq!(workflow_missing.analyzed_turns, 0);
}

#[test]
fn free_function_summary_round_trips_through_ledger_home() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut handle = Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
        let t = TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "x".into(),
            session_path: None,
            message_id: "m".into(),
            turn_index: 0,
            ts: "2026-04-23T00:00:00.000Z".into(),
            model: "claude-sonnet-4-6".into(),
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
            tool_calls: Vec::new(),
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        };
        handle.raw_mut().append_turns(&[t]).unwrap();
    }
    let s = summary(SummaryOptions {
        ledger_home: Some(dir.path().to_path_buf()),
        ..SummaryOptions::default()
    })
    .unwrap();
    assert_eq!(s.turn_count, 1);
    assert_eq!(s.total_tokens, 150);
}

#[test]
fn state_status_reports_zero_rows_on_fresh_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let handle = Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
    let s = handle.state_status().unwrap();
    assert!(s.burn.exists);
    assert!(s.content.exists);
    assert_eq!(s.burn.rows.turns, 0);
    assert_eq!(s.burn.rows.user_turns, 0);
    assert_eq!(s.burn.rows.compactions, 0);
    assert_eq!(s.burn.rows.relationships, 0);
    assert_eq!(s.burn.rows.tool_result_events, 0);
    assert_eq!(s.burn.rows.inferences, 0);
    assert_eq!(s.burn.rows.sessions, 0);
    assert_eq!(s.burn.rows.stamps, 0);
    assert_eq!(s.burn.tracked_rows, 0);
    assert_eq!(s.content.rows, 0);
    // v6 (#468 `archive_state.source_fingerprint`) chained onto v5
    // (#434 `inferences`), v4 (#435 `turns.subagent_id`), v3 (#436
    // `tool_result_events.output_bytes` / `output_truncated`) and v2
    // (#437 `turns.stop_reason`).
    assert_eq!(s.archive.schema_version, 6);
    assert!(s.archive.last_built_at.is_none());
    assert!(s.archive.last_rebuild_at.is_none());
}

#[test]
fn state_status_counts_appended_turns_and_user_turns() {
    let (_dir, handle) = fixture_handle();
    let s = handle.state_status().unwrap();
    assert_eq!(s.burn.rows.turns, 2);
    assert_eq!(s.burn.tracked_rows, 2);
}

#[test]
fn state_status_free_function_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _ = Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
    }
    let s = state_status(StateStatusOptions {
        ledger_home: Some(dir.path().to_path_buf()),
    })
    .unwrap();
    assert!(s.burn.exists);
    assert_eq!(s.burn.tracked_rows, 0);
}

#[test]
fn state_status_reads_config_from_active_home_not_env_default() {
    // Regression: previously `resolve_config_summary` called bare
    // `load_config()`, which always resolved against the env-var
    // home. Under `--ledger-path foo state status` that mixed one
    // home's databases with the env-default home's retention
    // settings. Verify the override home's config is honored.
    // Lock the env so a parallel test can't leak `RELAYBURN_HOME`
    // into the picker functions and shift the resolution off the
    // override path.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev_store = std::env::var("RELAYBURN_CONTENT_STORE").ok();
    let prev_ttl = std::env::var("RELAYBURN_CONTENT_TTL_DAYS").ok();
    std::env::remove_var("RELAYBURN_CONTENT_STORE");
    std::env::remove_var("RELAYBURN_CONTENT_TTL_DAYS");

    let dir = tempfile::tempdir().unwrap();
    // Put a config.json under the override home with non-default
    // values; the status report should reflect THESE, not the
    // hard-coded defaults.
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"content":{"store":"hash-only","retentionDays":7}}"#,
    )
    .unwrap();
    let _ = Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
    let s = state_status(StateStatusOptions {
        ledger_home: Some(dir.path().to_path_buf()),
    })
    .unwrap();
    assert_eq!(s.config.store, "hash-only");
    assert_eq!(s.config.retention_days, Some(7.0));
    assert!(!s.config.retention_forever);

    if let Some(v) = prev_store {
        std::env::set_var("RELAYBURN_CONTENT_STORE", v);
    }
    if let Some(v) = prev_ttl {
        std::env::set_var("RELAYBURN_CONTENT_TTL_DAYS", v);
    }
}

#[test]
fn state_status_propagates_io_error_when_config_is_unreadable() {
    // Regression: `resolve_config_summary` previously called
    // `unwrap_or_default()`, masking IO errors as a default config.
    // Permissions errors during `state_status` should propagate so
    // the typed-error reporter can surface them. Use a directory
    // *as* the config.json path — `read_to_string` will fail with
    // EISDIR (or similar) and that error is a Result::Err rather
    // than the parse-error fail-soft path.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev_store = std::env::var("RELAYBURN_CONTENT_STORE").ok();
    let prev_ttl = std::env::var("RELAYBURN_CONTENT_TTL_DAYS").ok();
    std::env::remove_var("RELAYBURN_CONTENT_STORE");
    std::env::remove_var("RELAYBURN_CONTENT_TTL_DAYS");

    let dir = tempfile::tempdir().unwrap();
    // Make config.json a directory; reading it as a file errors.
    std::fs::create_dir(dir.path().join("config.json")).unwrap();
    let _ = Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
    // The `read_config_file` path catches IO errors as a stderr
    // warning + treats the file as absent (TS parity), so it does
    // NOT surface as Err. Status should still succeed AND fall
    // through to defaults — the home plumbing kept us scoped to
    // the override directory rather than reading some other home's
    // config. Belt-and-braces: assert defaults, not the env home.
    let s = state_status(StateStatusOptions {
        ledger_home: Some(dir.path().to_path_buf()),
    })
    .unwrap();
    assert_eq!(s.config.store, "full");
    assert_eq!(s.config.retention_days, Some(90.0));

    if let Some(v) = prev_store {
        std::env::set_var("RELAYBURN_CONTENT_STORE", v);
    }
    if let Some(v) = prev_ttl {
        std::env::set_var("RELAYBURN_CONTENT_TTL_DAYS", v);
    }
}

fn relationship(
    session_id: &str,
    related_session_id: &str,
    agent_id: Option<&str>,
) -> SessionRelationshipRecord {
    SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::SpawnEnv,
        session_id: session_id.to_string(),
        related_session_id: Some(related_session_id.to_string()),
        relationship_type: RelationshipType::Subagent,
        ts: None,
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: agent_id.map(str::to_string),
        subagent_type: None,
        description: None,
    }
}

fn summary_test_turn(
    turn_index: u64,
    message_id: &str,
    usage: Usage,
    tool_calls: Vec<ToolCall>,
) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "session".to_string(),
        session_path: None,
        message_id: message_id.to_string(),
        turn_index,
        ts: format!("2026-04-20T00:00:0{turn_index}.000Z"),
        model: "claude-sonnet-4-6".to_string(),
        project: None,
        project_key: None,
        usage,
        tool_calls,
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    }
}

fn summary_test_tool(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        target: None,
        args_hash: "args".to_string(),
        is_error: None,
        edit_pre_hash: None,
        edit_post_hash: None,
        skill_name: None,
        replaced_tools: None,
        collapsed_calls: None,
    }
}

fn summary_test_tool_result_block(tool_use_id: &str, byte_len: u64) -> UserTurnBlock {
    UserTurnBlock {
        kind: UserTurnBlockKind::ToolResult,
        tool_use_id: Some(tool_use_id.to_string()),
        byte_len,
        approx_tokens: 0,
        is_error: None,
    }
}

// -----------------------------------------------------------------------
// compute_summary — replacement_savings field
// -----------------------------------------------------------------------

fn make_turn_with_calls(calls: Vec<ToolCall>) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "s".to_string(),
        session_path: None,
        message_id: "m".to_string(),
        turn_index: 0,
        ts: "2026-04-20T00:00:00.000Z".to_string(),
        model: "claude-sonnet-4-6".to_string(),
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
        tool_calls: calls,
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
fn compute_summary_replacement_savings_some_when_replacement_tool_present() {
    let tc = ToolCall {
        id: "tc-1".into(),
        name: "relaywash__Search".into(),
        target: None,
        args_hash: "h".into(),
        is_error: None,
        edit_pre_hash: None,
        edit_post_hash: None,
        skill_name: None,
        replaced_tools: Some(vec!["Glob".into(), "Grep".into(), "Read".into()]),
        collapsed_calls: Some(9),
    };
    let turns = vec![make_turn_with_calls(vec![tc])];
    let pricing = load_pricing(None);
    let result = compute_summary(&turns, &pricing);
    let savings = result
        .replacement_savings
        .expect("should have replacement_savings");
    assert_eq!(savings.calls, 1);
    assert_eq!(savings.collapsed_calls, 9);
    assert!(!savings.by_tool.is_empty());
    assert!(savings.by_tool.contains_key("relaywash__Search"));
}

#[test]
fn compute_summary_replacement_savings_none_when_no_replacement_tools() {
    let tc = ToolCall {
        id: "tc-1".into(),
        name: "Bash".into(),
        target: None,
        args_hash: "h".into(),
        is_error: None,
        edit_pre_hash: None,
        edit_post_hash: None,
        skill_name: None,
        replaced_tools: None,
        collapsed_calls: None,
    };
    let turns = vec![make_turn_with_calls(vec![tc])];
    let pricing = load_pricing(None);
    let result = compute_summary(&turns, &pricing);
    assert!(result.replacement_savings.is_none());
}

#[test]
fn duration_to_since_iso_emits_canonical_zulu_ms() {
    let iso = super::duration_to_since_iso(std::time::Duration::from_secs(60));
    // Shape only — actual value depends on system clock. We assert
    // the canonical lower-bound shape `YYYY-MM-DDTHH:MM:SS.mmmZ`
    // that `Query::since` lex-compares against ledger rows.
    assert_eq!(iso.len(), 24, "{iso}");
    assert!(iso.ends_with(".000Z"));
    assert!(iso.contains('T'));
}

/// Regression for the `since`-is-ignored bug: when `opts.since` is
/// `Some`, sessions whose latest turn is older than the window must
/// not appear in the deltas output. With a 1-second window and
/// fixtures whose turns are dated 2026-04 (weeks in the past),
/// every fixture session falls outside and the result is empty.
/// Without the fix the SDK would walk every session, so this
/// asserts the seed `query_turns(&since_scoped)` actually narrows.
#[test]
fn context_delta_since_filter_excludes_old_sessions() {
    use crate::analyze::context_delta::ContextDeltaOpts;
    let (_dir, handle) = multi_session_handle();
    let opts = ContextDeltaOpts {
        since: Some(std::time::Duration::from_secs(1)),
        ..ContextDeltaOpts::default()
    };
    let deltas = handle.context_delta(opts).expect("context_delta");
    assert!(
        deltas.is_empty(),
        "since=1s must drop fixture sessions whose latest turn is weeks old; got {} deltas",
        deltas.len(),
    );
}

fn multi_session_handle() -> (TempDir, LedgerHandle) {
    let dir = tempfile::tempdir().unwrap();
    let opts = LedgerOpenOptions::with_home(dir.path());
    let mut handle = Ledger::open(opts).expect("open ledger");

    let mk = |session: &str,
              project: Option<&str>,
              ts: &str,
              message_id: &str,
              model: &str|
     -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session.into(),
            session_path: None,
            message_id: message_id.into(),
            turn_index: 0,
            ts: ts.into(),
            model: model.into(),
            project: project.map(|s| s.into()),
            project_key: None,
            usage: Usage {
                input: 100,
                output: 50,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: vec![],
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    };

    // sess-old: oldest session, two turns, project /tmp/proj-a
    // sess-mid: middle session, one turn, project /tmp/proj-b
    // sess-new: newest session, one turn, project /tmp/proj-a
    handle
        .raw_mut()
        .append_turns(&[
            mk(
                "sess-old",
                Some("/tmp/proj-a"),
                "2026-04-20T10:00:00.000Z",
                "m-1",
                "claude-sonnet-4-6",
            ),
            mk(
                "sess-old",
                Some("/tmp/proj-a"),
                "2026-04-20T10:05:00.000Z",
                "m-2",
                "claude-sonnet-4-6",
            ),
            mk(
                "sess-mid",
                Some("/tmp/proj-b"),
                "2026-04-22T08:00:00.000Z",
                "m-3",
                "claude-sonnet-4-6",
            ),
            mk(
                "sess-new",
                Some("/tmp/proj-a"),
                "2026-04-23T12:00:00.000Z",
                "m-4",
                "claude-sonnet-4-6",
            ),
        ])
        .expect("append turns");
    (dir, handle)
}

#[test]
fn sessions_list_orders_most_recent_first_with_aggregates() {
    let (_dir, handle) = multi_session_handle();
    let result = handle
        .sessions_list(SessionsListOptions::default())
        .expect("sessions_list");
    assert_eq!(result.limit, SESSIONS_LIST_DEFAULT_LIMIT);
    assert!(!result.truncated);
    let ids: Vec<&str> = result
        .sessions
        .iter()
        .map(|s| s.session_id.as_str())
        .collect();
    assert_eq!(ids, vec!["sess-new", "sess-mid", "sess-old"]);

    let old = result
        .sessions
        .iter()
        .find(|s| s.session_id == "sess-old")
        .unwrap();
    assert_eq!(old.turn_count, 2);
    assert_eq!(old.started_at, "2026-04-20T10:00:00.000Z");
    assert_eq!(old.last_seen, "2026-04-20T10:05:00.000Z");
    assert_eq!(old.project.as_deref(), Some("/tmp/proj-a"));
    assert_eq!(old.models, vec!["claude-sonnet-4-6"]);
    assert!(old.total_cost_usd > 0.0);
}

#[test]
fn sessions_list_project_filter_narrows_to_match() {
    let (_dir, handle) = multi_session_handle();
    let result = handle
        .sessions_list(SessionsListOptions {
            project: Some("/tmp/proj-b".into()),
            ..SessionsListOptions::default()
        })
        .expect("sessions_list");
    let ids: Vec<&str> = result
        .sessions
        .iter()
        .map(|s| s.session_id.as_str())
        .collect();
    assert_eq!(ids, vec!["sess-mid"]);
}

#[test]
fn sessions_list_grep_matches_session_id_or_project_case_insensitive() {
    let (_dir, handle) = multi_session_handle();

    // session_id substring
    let by_id = handle
        .sessions_list(SessionsListOptions {
            grep: Some("OLD".into()),
            ..SessionsListOptions::default()
        })
        .expect("sessions_list");
    let ids: Vec<&str> = by_id
        .sessions
        .iter()
        .map(|s| s.session_id.as_str())
        .collect();
    assert_eq!(ids, vec!["sess-old"]);

    // project substring (matches sess-old + sess-new, both /tmp/proj-a)
    let by_project = handle
        .sessions_list(SessionsListOptions {
            grep: Some("proj-a".into()),
            ..SessionsListOptions::default()
        })
        .expect("sessions_list");
    let ids: Vec<&str> = by_project
        .sessions
        .iter()
        .map(|s| s.session_id.as_str())
        .collect();
    assert_eq!(ids, vec!["sess-new", "sess-old"]);
}

#[test]
fn sessions_list_limit_truncates_and_reports_truncation() {
    let (_dir, handle) = multi_session_handle();
    let result = handle
        .sessions_list(SessionsListOptions {
            limit: Some(2),
            ..SessionsListOptions::default()
        })
        .expect("sessions_list");
    assert_eq!(result.limit, 2);
    assert!(result.truncated);
    let ids: Vec<&str> = result
        .sessions
        .iter()
        .map(|s| s.session_id.as_str())
        .collect();
    assert_eq!(ids, vec!["sess-new", "sess-mid"]);
}

#[test]
fn sessions_list_since_drops_sessions_outside_window() {
    let (_dir, handle) = multi_session_handle();
    let result = handle
        .sessions_list(SessionsListOptions {
            since: Some("2026-04-22T00:00:00.000Z".into()),
            ..SessionsListOptions::default()
        })
        .expect("sessions_list");
    let ids: Vec<&str> = result
        .sessions
        .iter()
        .map(|s| s.session_id.as_str())
        .collect();
    assert_eq!(ids, vec!["sess-new", "sess-mid"]);
}

// ---------------------------------------------------------------------
// fingerprint
// ---------------------------------------------------------------------

#[test]
fn fingerprint_is_stable_when_nothing_changes() {
    let (_dir, handle) = fixture_handle();
    let a = handle.fingerprint(FingerprintScope::AllSessions).unwrap();
    let b = handle.fingerprint(FingerprintScope::AllSessions).unwrap();
    assert_eq!(a, b);
    // Triple shape: count:max_ts:total_bytes.
    let parts: Vec<&str> = a.as_str().split(':').collect();
    assert_eq!(parts.len(), 3, "expected count:max_ts:total_bytes, got {a}");
    assert_eq!(parts[0], "2", "fixture appends 2 turns");
    assert!(!parts[1].is_empty(), "max_ts must be non-empty");
    let total_bytes: u64 = parts[2].parse().expect("total_bytes is numeric");
    assert!(
        total_bytes > 0,
        "total_bytes must be > 0 for non-empty fixture"
    );
}

#[test]
fn fingerprint_changes_when_a_new_turn_is_appended() {
    let (_dir, mut handle) = fixture_handle();
    let before = handle.fingerprint(FingerprintScope::AllSessions).unwrap();

    let extra = TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "sess-a".into(),
        session_path: None,
        message_id: "m-3".into(),
        turn_index: 2,
        ts: "2026-04-23T00:02:00.000Z".into(),
        model: "claude-sonnet-4-6".into(),
        project: Some("/tmp/proj".into()),
        project_key: None,
        usage: Usage::default(),
        tool_calls: Vec::new(),
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    };
    handle.raw_mut().append_turns(&[extra]).unwrap();

    let after = handle.fingerprint(FingerprintScope::AllSessions).unwrap();
    assert_ne!(before, after, "fingerprint must change after ingest");

    // All three components moved: count up, max_ts up, total_bytes up.
    let before_parts: Vec<&str> = before.as_str().split(':').collect();
    let after_parts: Vec<&str> = after.as_str().split(':').collect();
    assert_eq!(before_parts[0], "2");
    assert_eq!(after_parts[0], "3");
    assert!(after_parts[1] > before_parts[1], "max_ts must advance");
    let b_size: u64 = before_parts[2].parse().unwrap();
    let a_size: u64 = after_parts[2].parse().unwrap();
    assert!(a_size > b_size, "total_bytes must grow");
}

#[test]
fn fingerprint_per_session_differs_from_global() {
    let (_dir, mut handle) = fixture_handle();

    // Add a second session so global ≠ per-session.
    let other = TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "sess-b".into(),
        session_path: None,
        message_id: "m-b1".into(),
        turn_index: 0,
        ts: "2026-04-23T01:00:00.000Z".into(),
        model: "claude-sonnet-4-6".into(),
        project: Some("/tmp/proj".into()),
        project_key: None,
        usage: Usage::default(),
        tool_calls: Vec::new(),
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    };
    handle.raw_mut().append_turns(&[other]).unwrap();

    let global = handle.fingerprint(FingerprintScope::AllSessions).unwrap();
    let only_a = handle
        .fingerprint(FingerprintScope::Session("sess-a".into()))
        .unwrap();
    let only_b = handle
        .fingerprint(FingerprintScope::Session("sess-b".into()))
        .unwrap();

    assert_ne!(global, only_a);
    assert_ne!(global, only_b);
    assert_ne!(only_a, only_b);
    // Sanity: per-session count totals match the global count.
    let g_count: u64 = global.as_str().split(':').next().unwrap().parse().unwrap();
    let a_count: u64 = only_a.as_str().split(':').next().unwrap().parse().unwrap();
    let b_count: u64 = only_b.as_str().split(':').next().unwrap().parse().unwrap();
    assert_eq!(g_count, a_count + b_count);
}

#[test]
fn fingerprint_empty_ledger_is_well_formed() {
    // No turns appended → count=0, max_ts="", total_bytes=0 — but
    // the string format is still the triple, so equality checks
    // continue to work for the "still empty" case.
    let dir = tempfile::tempdir().unwrap();
    let opts = LedgerOpenOptions::with_home(dir.path());
    let handle = Ledger::open(opts).unwrap();
    let fp = handle.fingerprint(FingerprintScope::AllSessions).unwrap();
    assert_eq!(fp.as_str(), "0::0");
}

#[test]
fn fingerprint_session_scope_for_missing_session_is_empty_shape() {
    let (_dir, handle) = fixture_handle();
    let fp = handle
        .fingerprint(FingerprintScope::Session("nope".into()))
        .unwrap();
    assert_eq!(fp.as_str(), "0::0");
}

#[test]
fn fingerprint_project_scope_matches_path_string() {
    let (_dir, handle) = fixture_handle();
    let fp = handle
        .fingerprint(FingerprintScope::Project("/tmp/proj".into()))
        .unwrap();
    let parts: Vec<&str> = fp.as_str().split(':').collect();
    assert_eq!(parts[0], "2");
}

#[test]
fn fingerprint_options_rejects_session_and_project_together() {
    let opts = FingerprintOptions {
        session: Some("a".into()),
        project: Some("/tmp/proj".into()),
        ledger_home: None,
    };
    assert!(opts.scope().is_err());
}

/// Performance bench (skipped: no 100k-row fixture in this tree).
/// Wired as a `#[ignore]` test so a future fixture can run it via
/// `cargo test -- --ignored fingerprint_perf`. The target is <10 ms
/// per call on a 100k-row ledger (#440).
#[test]
#[ignore = "requires 100k-row fixture; documents the <10ms perf target"]
fn fingerprint_perf_target_under_10ms_on_100k_rows() {
    let (_dir, handle) = fixture_handle();
    let start = std::time::Instant::now();
    for _ in 0..1_000 {
        let _ = handle.fingerprint(FingerprintScope::AllSessions).unwrap();
    }
    let per_call = start.elapsed() / 1_000;
    assert!(
        per_call < std::time::Duration::from_millis(10),
        "fingerprint per call {per_call:?} exceeds the 10ms #440 target"
    );
}

// ----------------------------------------------------------------
// Span tree integration tests (#430)
// ----------------------------------------------------------------

/// `session_span_trees` returns one [`TurnSpanTree`] per turn,
/// each carrying the right scalars projected off the Inference
/// child. Uses the `fixture_handle` fixture which pre-loads two
/// Claude turns into the test ledger.
#[test]
fn session_span_trees_round_trips_two_turn_fixture() {
    use crate::analyze::span_tree::{SpanKind, SpanStatus};

    let (_dir, handle) = fixture_handle();
    let trees = handle.session_span_trees("sess-a").expect("trees");
    assert_eq!(trees.len(), 2, "two turns in fixture");
    // Turn order matches the ledger row order (turn_index 0, 1).
    assert_eq!(trees[0].turn_id, "m-1");
    assert_eq!(trees[1].turn_id, "m-2");

    // Root status is Ok (no stop_reason / no tool_error in
    // fixture).
    assert_eq!(trees[0].root.status, SpanStatus::Ok);

    // Each root has UserPrompt + at least one Inference child.
    let kinds: Vec<SpanKind> = trees[0].root.children.iter().map(|c| c.kind).collect();
    assert!(kinds.contains(&SpanKind::UserPrompt));
    assert!(kinds.contains(&SpanKind::Inference));

    // Token scalars project off the tree and match
    // TurnRecord::usage (1000 input, 500 output for turn 1).
    assert_eq!(trees[0].sum_attr_int("tokens.input"), 1000);
    assert_eq!(trees[0].sum_attr_int("tokens.output"), 500);
    assert_eq!(trees[1].sum_attr_int("tokens.input"), 800);
    assert_eq!(trees[1].sum_attr_int("tokens.cache_read"), 200);
}

/// `turn_span_tree` returns the same tree as
/// `session_span_trees`'s matching entry. Pinning the contract so
/// downstream consumers can pick whichever verb suits their
/// access pattern without worrying about divergence.
#[test]
fn turn_span_tree_matches_session_entry() {
    let (_dir, handle) = fixture_handle();
    let single = handle.turn_span_tree("sess-a", "m-2").expect("turn");
    let from_session = handle
        .session_span_trees("sess-a")
        .expect("session")
        .into_iter()
        .find(|t| t.turn_id == "m-2")
        .expect("m-2 present");
    // The structures are deterministic projections of the same
    // input, so PartialEq passes.
    assert_eq!(single, from_session);
}

/// Missing turn → error rather than empty / panic.
#[test]
fn turn_span_tree_missing_turn_errors() {
    let (_dir, handle) = fixture_handle();
    let err = handle
        .turn_span_tree("sess-a", "does-not-exist")
        .unwrap_err();
    assert!(err.to_string().contains("turn not found"), "got: {err:?}");
}

/// Unknown session id → no trees, no error.
#[test]
fn session_span_trees_unknown_session_is_empty() {
    let (_dir, handle) = fixture_handle();
    let trees = handle.session_span_trees("not-a-session").expect("ok");
    assert!(trees.is_empty());
}

// --- bucket_subagents_per_turn unit tests ----------------------------

fn bucket_turn(message_id: &str, ts: &str, tool_use_ids: &[&str]) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: "s".into(),
        session_path: None,
        message_id: message_id.into(),
        turn_index: 0,
        ts: ts.into(),
        model: "claude".into(),
        project: None,
        project_key: None,
        usage: Usage::default(),
        tool_calls: tool_use_ids
            .iter()
            .map(|id| ToolCall {
                id: (*id).into(),
                name: "Task".into(),
                target: None,
                args_hash: "h".into(),
                is_error: None,
                edit_pre_hash: None,
                edit_post_hash: None,
                skill_name: None,
                replaced_tools: None,
                collapsed_calls: None,
            })
            .collect(),
        files_touched: None,
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: None,
    }
}

fn bucket_subagent(
    agent_id: &str,
    paired_tool_use_id: Option<&str>,
    first_record_ts: Option<&str>,
) -> crate::reader::SubagentTranscript {
    let records: Vec<serde_json::Value> = first_record_ts
        .map(|ts| vec![serde_json::json!({ "timestamp": ts })])
        .unwrap_or_default();
    crate::reader::SubagentTranscript {
        agent_id: agent_id.into(),
        agent_type: None,
        description: None,
        meta_tool_use_id: None,
        records,
        paired_tool_use_id: paired_tool_use_id.map(str::to_string),
        source_path: std::path::PathBuf::from(format!("/tmp/agent-{agent_id}.jsonl")),
    }
}

/// Acceptance: paired subagents land under the turn whose
/// tool_calls carry the matching tool_use_id; orphans land under
/// the latest preceding turn. Each subagent appears in **exactly
/// one** turn bucket — no duplication.
#[test]
fn bucket_subagents_paired_and_orphan_each_land_in_one_turn() {
    // Two turns with one Task tool each.
    let turn0 = bucket_turn("m-1", "2026-04-23T00:00:00.000Z", &["tu-a"]);
    let turn1 = bucket_turn("m-2", "2026-04-23T00:05:00.000Z", &["tu-b"]);
    let turns = vec![turn0, turn1];

    // Paired sidecar matches tu-b (lives in turn 1).
    let paired = bucket_subagent("paired-1", Some("tu-b"), None);
    // Orphan whose first record timestamp lands between the two
    // turns — must attach to turn 0 (latest preceding turn).
    let orphan_mid = bucket_subagent("orphan-mid", None, Some("2026-04-23T00:02:00.000Z"));
    // Orphan timestamped before any turn — attaches to turn 0
    // (first-turn fallback).
    let orphan_early = bucket_subagent("orphan-early", None, Some("2026-04-22T23:00:00.000Z"));
    // Orphan timestamped after both turns — attaches to turn 1
    // (latest preceding).
    let orphan_late = bucket_subagent("orphan-late", None, Some("2026-04-23T00:10:00.000Z"));
    let subagents = vec![paired, orphan_mid, orphan_early, orphan_late];

    let buckets = bucket_subagents_per_turn(&turns, &subagents);

    // Each subagent must appear in exactly one bucket (the
    // duplication bug would have placed orphans into every turn).
    let total_placements: usize = buckets.values().map(|v| v.len()).sum();
    assert_eq!(
        total_placements,
        subagents.len(),
        "each subagent must land in exactly one turn"
    );

    // Turn 0 owns: orphan-mid (latest preceding) + orphan-early
    // (first-turn fallback). Turn 1 owns: paired-1 + orphan-late.
    let turn0_agents: Vec<&str> = buckets
        .get(&0)
        .unwrap_or(&Vec::new())
        .iter()
        .map(|idx| subagents[*idx].agent_id.as_str())
        .collect();
    let turn1_agents: Vec<&str> = buckets
        .get(&1)
        .unwrap_or(&Vec::new())
        .iter()
        .map(|idx| subagents[*idx].agent_id.as_str())
        .collect();
    assert!(turn0_agents.contains(&"orphan-mid"), "orphan-mid -> turn0");
    assert!(
        turn0_agents.contains(&"orphan-early"),
        "orphan-early -> turn0 (first-turn fallback)"
    );
    assert!(turn1_agents.contains(&"paired-1"), "paired-1 -> turn1");
    assert!(
        turn1_agents.contains(&"orphan-late"),
        "orphan-late -> turn1"
    );
    // No turn carries the same agent twice.
    assert_eq!(turn0_agents.len(), 2);
    assert_eq!(turn1_agents.len(), 2);
}

/// Regression for the P1 finding: session-wide orphan subagents
/// must NOT be duplicated into every turn's tree. The end-to-end
/// proof is at the verb level — build trees for both turns and
/// verify the orphan is a child of exactly one root.
///
/// This shape goes through `session_span_trees` (the bug site)
/// rather than the helper directly, so we exercise the full
/// orchestration path.
#[test]
fn session_span_trees_orphan_subagent_not_duplicated_across_turns() {
    use crate::analyze::span_tree::SpanKind;

    let (_dir, handle) = fixture_handle();
    // Both fixture turns have no Task tool_use ids matching the
    // sidecar — synthesize the bucketing path directly with two
    // turns and one orphan to confirm the per-turn placement.
    let turns_view = vec![
        bucket_turn("m-1", "2026-04-23T00:00:00.000Z", &[]),
        bucket_turn("m-2", "2026-04-23T00:05:00.000Z", &[]),
    ];
    let subagents = vec![bucket_subagent(
        "lone-orphan",
        None,
        Some("2026-04-23T00:03:00.000Z"),
    )];
    let buckets = bucket_subagents_per_turn(&turns_view, &subagents);
    let placements: usize = buckets.values().map(|v| v.len()).sum();
    assert_eq!(placements, 1, "orphan placed exactly once");

    // Sanity: the verb path runs cleanly against the fixture (no
    // subagents in the fixture session, but the call mustn't
    // duplicate anything either).
    let trees = handle.session_span_trees("sess-a").expect("trees");
    let orphan_subs_per_tree: Vec<usize> = trees
        .iter()
        .map(|t| {
            t.root
                .children
                .iter()
                .filter(|c| {
                    c.kind == SpanKind::Subagent
                        && matches!(
                            c.attributes.get("unattached"),
                            Some(crate::analyze::span_tree::AttrValue::Bool(true))
                        )
                })
                .count()
        })
        .collect();
    // Fixture has no subagents, so per-turn orphan counts are
    // zero; the assertion catches the regression by failing if a
    // future bug re-introduces orphan duplication.
    assert!(orphan_subs_per_tree.iter().all(|n| *n == 0));
}

#[test]
fn summary_report_by_tool_aggregates_across_multiple_sessions() {
    // Regression: the old N+1 loop was replaced with a single batched
    // query. Verify that per-tool totals include contributions from
    // BOTH sessions so a future regression restoring the loop can't
    // silently drop cross-session data.
    let dir = tempfile::tempdir().unwrap();
    let opts = LedgerOpenOptions::with_home(dir.path());
    let mut handle = Ledger::open(opts).expect("open ledger");

    let make_turn_with_tool =
        |session_id: &str, message_id: &str, ts: &str, tool: &str| TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.into(),
            session_path: None,
            message_id: message_id.into(),
            turn_index: 0,
            ts: ts.into(),
            model: "claude-sonnet-4-6".into(),
            project: Some("/tmp/proj".into()),
            project_key: None,
            usage: Usage {
                input: 1000,
                output: 500,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: vec![ToolCall {
                id: "tc-1".into(),
                name: tool.into(),
                target: None,
                args_hash: "h1".into(),
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
        };

    // Attribution works by attributing turn[i]'s input cost to the tool
    // calls in turn[i-1]. So to get a tool to appear in the report, we
    // need a "response turn" that follows it.
    //
    // Layout:
    //   sess-a: ma-1 (Read), ma-2 (no tools) → ma-2 cost attributed to Read
    //   sess-b: mb-1 (Edit), mb-2 (no tools) → mb-2 cost attributed to Edit
    //
    // Cross-session assertion: Read from sess-a and Edit from sess-b both
    // appear in the same report, proving the batched query covers all sessions.
    let make_turn_no_tools = |session_id: &str, message_id: &str, ts: &str| {
        let mut t = make_turn_with_tool(session_id, message_id, ts, "Read");
        t.tool_calls.clear();
        t
    };

    handle
        .raw_mut()
        .append_turns(&[
            make_turn_with_tool("sess-a", "ma-1", "2026-05-01T00:00:00.000Z", "Read"),
            make_turn_no_tools("sess-a", "ma-2", "2026-05-01T00:01:00.000Z"),
            make_turn_with_tool("sess-b", "mb-1", "2026-05-01T01:00:00.000Z", "Edit"),
            make_turn_no_tools("sess-b", "mb-2", "2026-05-01T01:01:00.000Z"),
        ])
        .expect("append turns");

    let report = handle
        .summary_report(SummaryReportOptions {
            mode: SummaryReportMode::ByTool,
            ..SummaryReportOptions::default()
        })
        .expect("summary report");
    let SummaryReport::ByTool(report) = report else {
        panic!("expected by-tool report");
    };

    // 4 turns across 2 sessions.
    assert_eq!(report.turn_count, 4);

    let read_row = report
        .rows
        .iter()
        .find(|r| r.tool == "Read")
        .expect("Read row from sess-a");
    let edit_row = report
        .rows
        .iter()
        .find(|r| r.tool == "Edit")
        .expect("Edit row from sess-b");

    // Each tool call is from a different session — cross-session totals.
    assert_eq!(read_row.calls, 1, "Read calls should come from sess-a");
    assert_eq!(edit_row.calls, 1, "Edit calls should come from sess-b");
    // Attribution cost must be positive for both tools.
    assert!(read_row.attributed_cost > 0.0);
    assert!(edit_row.attributed_cost > 0.0);
}
#[cfg(test)]
mod fingerprint_bench {
    use super::*;
    use crate::reader::{ToolCall, Usage};

    /// Manual: `cargo test -p relayburn-sdk --release --lib fingerprint_bench -- --ignored --nocapture`.
    /// Builds a 100k-row in-memory ledger and times the all-sessions
    /// fingerprint. Prints the per-call timing.
    #[test]
    #[ignore = "100k-row bench; manual run only"]
    fn manual_perf_100k() {
        let dir = tempfile::tempdir().unwrap();
        let opts = LedgerOpenOptions::with_home(dir.path());
        let mut handle = Ledger::open(opts).unwrap();

        let mut turns = Vec::with_capacity(100_000);
        for i in 0..100_000u32 {
            turns.push(TurnRecord {
                v: 1,
                source: crate::reader::SourceKind::ClaudeCode,
                session_id: format!("sess-{}", i % 100),
                session_path: None,
                message_id: format!("m-{i}"),
                turn_index: (i % 1000) as u64,
                ts: format!("2026-04-{:02}T{:02}:00:00.000Z", 1 + (i / 24) % 28, i % 24),
                model: "claude-sonnet-4-6".into(),
                project: Some("/tmp/p".into()),
                project_key: None,
                usage: Usage::default(),
                tool_calls: vec![ToolCall {
                    id: format!("tu-{i}"),
                    name: "Read".into(),
                    target: Some("/tmp/p/x.rs".into()),
                    args_hash: format!("h{i}"),
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
            });
        }
        handle.raw_mut().append_turns(&turns).unwrap();

        let iters = 100;
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let _ = handle.fingerprint(FingerprintScope::AllSessions).unwrap();
        }
        let all_per = start.elapsed() / iters;
        println!("fingerprint(AllSessions) 100k rows: {all_per:?} per call");

        let start = std::time::Instant::now();
        for _ in 0..iters {
            let _ = handle
                .fingerprint(FingerprintScope::Session("sess-1".into()))
                .unwrap();
        }
        let sess_per = start.elapsed() / iters;
        println!("fingerprint(Session) 100k rows: {sess_per:?} per call");

        // The #440 target was <10 ms on a 100k-row ledger. The
        // session-scoped path easily clears it (sees ~1k rows via
        // `idx_turns_session`). The all-sessions path on the synthetic
        // 100k fixture is dominated by
        // `SUM(LENGTH(CAST(record_json AS BLOB)))`'s sequential scan
        // over ~50 MB of JSON — release builds land at ~30 ms here.
        // The fix would be a stored `byte_size` column on `turns`,
        // which is a schema bump deliberately scoped out of #440
        // (poll-only primitive). Assert a generous all-sessions bound
        // so a regression that pushes well past the scan-rate envelope
        // still flags.
        assert!(
            sess_per < std::time::Duration::from_millis(10),
            "session-scope per-call {sess_per:?} exceeds the 10ms #440 target"
        );
        assert!(
            all_per < std::time::Duration::from_millis(150),
            "all-sessions per-call {all_per:?} regressed past the scan-rate envelope"
        );
    }
}

// ---------------------------------------------------------------------------
// `--bucket` time-series
// ---------------------------------------------------------------------------

fn bucket_test_turn(session: &str, message: &str, ts: &str, input: u64) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session.into(),
        session_path: None,
        message_id: message.into(),
        turn_index: 0,
        ts: ts.into(),
        model: "claude-sonnet-4-6".into(),
        project: Some("/tmp/proj".into()),
        project_key: None,
        usage: Usage {
            input,
            output: 0,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        },
        tool_calls: vec![],
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
fn parse_bucket_uses_minute_grammar_and_rejects_garbage() {
    assert_eq!(super::parse_bucket("30s").unwrap(), 30);
    assert_eq!(super::parse_bucket("5m").unwrap(), 300); // minutes, not months
    assert_eq!(super::parse_bucket("1h").unwrap(), 3_600);
    assert_eq!(super::parse_bucket("12h").unwrap(), 43_200);
    assert_eq!(super::parse_bucket("1d").unwrap(), 86_400);
    assert_eq!(super::parse_bucket("7d").unwrap(), 604_800);
    assert_eq!(super::parse_bucket("1w").unwrap(), 604_800);
    for bad in ["", "5", "5x", "0m", "abc", "h", "-5m", "m"] {
        assert!(
            super::parse_bucket(bad).is_err(),
            "{bad:?} should be rejected"
        );
    }
}

#[test]
fn iso_z_to_epoch_secs_roundtrips_with_formatter() {
    for secs in [0_i64, 1_700_000_000, super::system_now_secs() as i64] {
        let iso = super::format_iso_z_ms(secs, 0);
        assert_eq!(
            super::iso_z_to_epoch_secs(&iso),
            Some(secs),
            "roundtrip {iso}"
        );
    }
    // sub-second precision is floored, not rejected.
    assert_eq!(
        super::iso_z_to_epoch_secs("2026-04-23T00:00:00.500Z"),
        super::iso_z_to_epoch_secs("2026-04-23T00:00:00.000Z")
    );
    assert_eq!(super::iso_z_to_epoch_secs("garbage"), None);
}

#[test]
fn buckets_partition_edges_and_indices() {
    let b = super::Buckets::new(0, 1000, 300); // edges 0,300,600,900,1200 -> 4 buckets
    assert_eq!(b.len(), 4);
    assert_eq!(b.index_for(0), Some(0));
    assert_eq!(b.index_for(299), Some(0));
    assert_eq!(b.index_for(300), Some(1));
    assert_eq!(b.index_for(899), Some(2));
    assert_eq!(b.index_for(950), Some(3));
    assert_eq!(b.index_for(-1), None); // before anchor
    assert_eq!(b.index_for(1200), None); // last edge is exclusive
}

#[test]
fn summary_timeseries_places_turns_in_buckets_and_sums_to_total() {
    let dir = tempfile::tempdir().unwrap();
    let mut handle = Ledger::open(LedgerOpenOptions::with_home(dir.path())).expect("open");

    // Two turns inside the last hour, ~9 minutes apart, so they land in
    // different 5-minute buckets. Timestamps are anchored to `now` so the
    // `--since 1h` window (which ends at `now`) actually contains them.
    let now = super::system_now_secs() as i64;
    let ts_recent = super::format_iso_z_ms(now - 180, 0); // 3m ago
    let ts_older = super::format_iso_z_ms(now - 720, 0); // 12m ago
    handle
        .raw_mut()
        .append_turns(&[
            bucket_test_turn("s1", "m1", &ts_recent, 1_000),
            bucket_test_turn("s1", "m2", &ts_older, 2_000),
        ])
        .expect("append");

    let series = handle
        .summary_timeseries(
            SummaryReportOptions {
                since: Some("1h".into()),
                mode: SummaryReportMode::Grouped { by_provider: false },
                ..SummaryReportOptions::default()
            },
            300, // 5-minute buckets
        )
        .expect("timeseries");

    assert_eq!(series.bucket_secs, 300);
    let nonempty: Vec<_> = series.buckets.iter().filter(|b| b.turn_count > 0).collect();
    assert_eq!(
        nonempty.len(),
        2,
        "two turns 9m apart -> two distinct 5m buckets"
    );
    assert!(nonempty.iter().all(|b| b.turn_count == 1));

    // Per-bucket totals reconcile with the un-bucketed total.
    let total_tokens: u64 = series.buckets.iter().map(|b| b.total_tokens).sum();
    assert_eq!(total_tokens, 3_000);
    let total_turns: u64 = series.buckets.iter().map(|b| b.turn_count).sum();
    assert_eq!(total_turns, 2);
}

#[test]
fn summary_timeseries_rejects_attribution_modes() {
    let (_dir, handle) = fixture_handle();
    let err = handle
        .summary_timeseries(
            SummaryReportOptions {
                mode: SummaryReportMode::ByTool,
                ..SummaryReportOptions::default()
            },
            300,
        )
        .expect_err("--bucket must reject --by-tool");
    assert!(err.to_string().contains("--bucket"));
}
