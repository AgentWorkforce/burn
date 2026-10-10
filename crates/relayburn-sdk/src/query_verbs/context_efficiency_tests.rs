use super::*;
use crate::reader::{SourceKind, Usage};

fn turn(session: &str, index: u64, usage: Usage) -> TurnRecord {
    TurnRecord {
        v: 1,
        source: SourceKind::ClaudeCode,
        session_id: session.to_string(),
        message_id: format!("m-{index}"),
        turn_index: index,
        ts: format!("2026-07-30T00:00:{index:02}.000Z"),
        model: "test-model".to_string(),
        session_path: None,
        project: None,
        project_key: None,
        usage,
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
fn context_math_includes_input_reads_and_both_creation_buckets() {
    let mut incident = turn(
        "incident",
        0,
        Usage {
            input: 10,
            output: 2,
            reasoning: 1,
            cache_read: 700,
            cache_create_5m: 40,
            cache_create_1h: 14,
        },
    );
    // Codex's output already contains its separately exposed reasoning;
    // this keeps the expected denominator at two generated tokens.
    incident.source = SourceKind::Codex;
    let turns = vec![incident];
    let summary = compute_context_efficiency(&turns);
    assert_eq!(summary.context_tokens, 764);
    assert_eq!(summary.output_tokens, 2);
    assert_eq!(summary.context_tokens_per_output_token, Some(382.0));
    assert_eq!(summary.sessions[0].context_size.max, 764);
}

#[test]
fn denominator_includes_all_generation_consistently_across_harnesses() {
    let separate_usage = Usage {
        input: 100,
        output: 30,
        reasoning: 10,
        ..Usage::default()
    };
    let included_usage = Usage {
        input: 100,
        output: 40,
        reasoning: 10,
        ..Usage::default()
    };
    let claude = turn("claude", 0, separate_usage);
    let mut codex = turn("codex", 0, included_usage);
    codex.source = SourceKind::Codex;

    assert_eq!(generated_output_tokens(&claude), 40);
    assert_eq!(generated_output_tokens(&codex), 40);
    assert_eq!(context_efficiency_for_turn(&codex).output_tokens, 40);

    let mut reasoning_only = turn(
        "reasoning-only",
        0,
        Usage {
            input: 100,
            output: 0,
            reasoning: 10,
            ..Usage::default()
        },
    );
    reasoning_only.source = SourceKind::Opencode;
    assert_eq!(generated_output_tokens(&reasoning_only), 10);
    assert!(!context_efficiency_for_turn(&reasoning_only).unbounded);
}

#[test]
fn turn_metric_marks_only_context_consuming_zero_output_turns_unbounded() {
    let context_only = turn(
        "s",
        0,
        Usage {
            input: 50,
            ..Usage::default()
        },
    );
    let empty = turn("s", 1, Usage::default());
    let productive = turn(
        "s",
        2,
        Usage {
            input: 30,
            output: 10,
            ..Usage::default()
        },
    );
    assert_eq!(
        context_efficiency_for_turn(&context_only),
        TurnContextEfficiency {
            context_tokens: 50,
            output_tokens: 0,
            context_tokens_per_output_token: None,
            unbounded: true,
        }
    );
    assert_eq!(
        context_efficiency_for_turn(&empty),
        TurnContextEfficiency::default()
    );
    assert_eq!(
        context_efficiency_for_turn(&productive),
        TurnContextEfficiency {
            context_tokens: 30,
            output_tokens: 10,
            context_tokens_per_output_token: Some(3.0),
            unbounded: false,
        }
    );

    assert!(compute_context_efficiency(std::slice::from_ref(&context_only)).unbounded);
    assert!(!compute_context_efficiency(std::slice::from_ref(&empty)).unbounded);

    let summary = compute_context_efficiency(&[context_only, empty, productive]);
    assert!(!summary.unbounded);
    assert_eq!(summary.context_tokens_per_output_token, Some(8.0));
    assert_eq!(summary.zero_output_turns_with_context, 1);
    assert_eq!(summary.sessions[0].zero_output_turns_with_context, 1);
    assert_eq!(summary.sessions[0].turn_count, 3);
}

#[test]
fn ratio_findings_are_high_when_unbounded_or_at_double_the_threshold() {
    let summary = compute_context_efficiency(&[
        turn(
            "double",
            0,
            Usage {
                input: 200,
                output: 1,
                ..Usage::default()
            },
        ),
        turn(
            "above",
            0,
            Usage {
                input: 199,
                output: 1,
                ..Usage::default()
            },
        ),
        turn(
            "zero-output",
            0,
            Usage {
                input: 50,
                ..Usage::default()
            },
        ),
    ]);
    let severities: Vec<(String, crate::analyze::WasteSeverity)> =
        context_output_ratio_findings(&summary, 100.0, 0)
            .into_iter()
            .map(|finding| (finding.session_id, finding.severity))
            .collect();
    assert_eq!(
        severities,
        vec![
            (
                "zero-output".to_string(),
                crate::analyze::WasteSeverity::High
            ),
            ("double".to_string(), crate::analyze::WasteSeverity::High),
            ("above".to_string(), crate::analyze::WasteSeverity::Warn),
        ]
    );
}

#[test]
fn session_ratio_divides_totals_instead_of_averaging_turn_ratios() {
    let turns = vec![
        turn(
            "weighted",
            0,
            Usage {
                input: 100,
                output: 1,
                ..Usage::default()
            },
        ),
        turn(
            "weighted",
            1,
            Usage {
                input: 100,
                output: 99,
                ..Usage::default()
            },
        ),
    ];
    assert_eq!(
        compute_context_efficiency(&turns).sessions[0].context_tokens_per_output_token,
        Some(2.0)
    );
}

#[test]
fn zero_output_is_json_safe_and_distinguishes_context_from_empty() {
    let summary = compute_context_efficiency(&[
        turn(
            "unbounded",
            0,
            Usage {
                input: 50,
                ..Usage::default()
            },
        ),
        turn("empty", 0, Usage::default()),
    ]);
    let unbounded = summary
        .sessions
        .iter()
        .find(|s| s.session_id == "unbounded")
        .unwrap();
    assert_eq!(unbounded.context_tokens_per_output_token, None);
    assert!(unbounded.unbounded);
    let json = serde_json::to_string(&summary).unwrap();
    assert!(!json.contains("NaN"));
    assert!(!json.contains("Infinity"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap()["sessions"][0]
            ["contextTokensPerOutputToken"],
        serde_json::Value::Null
    );

    let empty = summary
        .sessions
        .iter()
        .find(|s| s.session_id == "empty")
        .unwrap();
    assert!(!empty.unbounded);
}

#[test]
fn distribution_uses_nearest_rank_for_one_two_and_many_turns() {
    let one = compute_context_efficiency(&[turn(
        "one",
        0,
        Usage {
            input: 7,
            output: 1,
            ..Usage::default()
        },
    )]);
    assert_eq!(
        one.sessions[0].context_size,
        ContextSizeDistribution {
            p50: 7,
            p95: 7,
            max: 7
        }
    );

    let two = compute_context_efficiency(&[
        turn(
            "two",
            0,
            Usage {
                input: 10,
                output: 1,
                ..Usage::default()
            },
        ),
        turn(
            "two",
            1,
            Usage {
                input: 20,
                output: 1,
                ..Usage::default()
            },
        ),
    ]);
    assert_eq!(
        two.sessions[0].context_size,
        ContextSizeDistribution {
            p50: 10,
            p95: 20,
            max: 20
        }
    );

    let many: Vec<_> = (1..=20)
        .map(|n| {
            turn(
                "many",
                n,
                Usage {
                    input: n,
                    output: 1,
                    ..Usage::default()
                },
            )
        })
        .collect();
    assert_eq!(
        compute_context_efficiency(&many).sessions[0].context_size,
        ContextSizeDistribution {
            p50: 10,
            p95: 19,
            max: 20
        }
    );
}

#[test]
fn finding_threshold_is_inclusive_and_override_changes_selection() {
    let summary = compute_context_efficiency(&[
        turn(
            "boundary",
            0,
            Usage {
                input: 1_146_000,
                output: 3_000,
                ..Usage::default()
            },
        ),
        turn(
            "below",
            0,
            Usage {
                input: 1_145_700,
                output: 3_000,
                ..Usage::default()
            },
        ),
        turn(
            "incident",
            0,
            Usage {
                input: 2_292_000,
                output: 6_000,
                ..Usage::default()
            },
        ),
    ]);
    let default_findings = context_output_ratio_findings(
        &summary,
        DEFAULT_CONTEXT_OUTPUT_RATIO_THRESHOLD,
        DEFAULT_CONTEXT_OUTPUT_MIN_TOKENS,
    );
    assert_eq!(default_findings.len(), 2);
    assert!(default_findings.iter().any(|f| f.session_id == "boundary"));
    assert!(default_findings.iter().any(|f| f.session_id == "incident"));

    let overridden = context_output_ratio_findings(&summary, 400.0, 0);
    assert!(overridden.is_empty());
}

#[test]
fn context_floor_filters_trivial_sessions_and_findings_rank_by_ratio() {
    let summary = compute_context_efficiency(&[
        turn(
            "tiny",
            0,
            Usage {
                input: 25_000,
                output: 10,
                ..Usage::default()
            },
        ),
        turn(
            "incident",
            0,
            Usage {
                input: 1_146_000,
                output: 3_000,
                ..Usage::default()
            },
        ),
        turn(
            "worst",
            0,
            Usage {
                input: 2_500_000,
                output: 1_000,
                ..Usage::default()
            },
        ),
        turn(
            "high-less",
            0,
            Usage {
                input: 1_600_000,
                output: 2_000,
                ..Usage::default()
            },
        ),
    ]);
    let mut findings = context_output_ratio_findings(
        &summary,
        DEFAULT_CONTEXT_OUTPUT_RATIO_THRESHOLD,
        DEFAULT_CONTEXT_OUTPUT_MIN_TOKENS,
    );
    crate::analyze::sort_findings(&mut findings);
    assert_eq!(
        findings
            .iter()
            .map(|finding| finding.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["worst", "high-less", "incident"]
    );
}

#[test]
fn calibrated_rule_has_expected_rate_across_mixed_session_lengths() {
    fn session_turns(
        session: &str,
        turns: u64,
        context_per_turn: u64,
        output_per_turn: u64,
    ) -> Vec<TurnRecord> {
        (0..turns)
            .map(|index| {
                turn(
                    session,
                    index,
                    Usage {
                        input: context_per_turn,
                        output: output_per_turn,
                        ..Usage::default()
                    },
                )
            })
            .collect()
    }

    let mut corpus = Vec::new();
    // Exact incident boundary, 2 turns, 1.146M context: flags.
    corpus.extend(session_turns("incident", 2, 573_000, 1_500));
    // Long high-ratio session: flags.
    corpus.extend(session_turns("long-high", 25, 80_000, 100));
    // Long but below-ratio session: does not flag despite 1.25M context.
    corpus.extend(session_turns("long-normal", 25, 50_000, 200));
    // Short, high-volume, below-ratio session: does not flag.
    corpus.extend(session_turns("short-normal", 3, 400_000, 2_000));

    let summary = compute_context_efficiency(&corpus);
    let findings = context_output_ratio_findings(
        &summary,
        DEFAULT_CONTEXT_OUTPUT_RATIO_THRESHOLD,
        DEFAULT_CONTEXT_OUTPUT_MIN_TOKENS,
    );
    assert_eq!(summary.sessions.len(), 4);
    assert_eq!(findings.len(), 2);
    assert_eq!(findings.len() as f64 / summary.sessions.len() as f64, 0.5);
    assert!(findings.iter().any(|f| f.session_id == "incident"));
    assert!(findings.iter().any(|f| f.session_id == "long-high"));
}

#[test]
fn summary_projection_keeps_low_volume_sessions_and_caps_rows() {
    let mut turns = Vec::new();
    turns.push(turn(
        "tiny-high-ratio",
        0,
        Usage {
            input: 20_000,
            output: 1,
            ..Usage::default()
        },
    ));
    for index in 0..12 {
        turns.push(turn(
            &format!("eligible-{index:02}"),
            0,
            Usage {
                input: 1_000_000 + index,
                output: 1_000 + index,
                ..Usage::default()
            },
        ));
    }

    let summary = compute_context_efficiency_for_summary(&turns);
    assert_eq!(summary.total_sessions, 13);
    assert_eq!(summary.eligible_sessions, 12);
    assert_eq!(summary.sessions.len(), SUMMARY_CONTEXT_SESSION_LIMIT);
    assert!(summary
        .sessions
        .iter()
        .any(|session| session.session_id == "tiny-high-ratio"));
}

#[test]
fn threshold_validation_rejects_non_finite_and_negative_values() {
    assert!(validate_context_output_ratio_threshold(-1.0).is_err());
    assert!(validate_context_output_ratio_threshold(f64::NAN).is_err());
    assert!(validate_context_output_ratio_threshold(f64::INFINITY).is_err());
    assert!(validate_context_output_ratio_threshold(0.0).is_ok());
}

#[test]
fn ratio_config_applies_defaults_and_overrides() {
    assert_eq!(
        ContextOutputRatioConfig::from_hotspots_options(&HotspotsOptions::default()).unwrap(),
        ContextOutputRatioConfig {
            threshold: 382.0,
            min_context_tokens: 1_000_000,
        }
    );
    let overridden = HotspotsOptions {
        context_output_ratio_threshold: Some(50.5),
        context_output_min_tokens: Some(0),
        ..HotspotsOptions::default()
    };
    assert_eq!(
        ContextOutputRatioConfig::from_hotspots_options(&overridden).unwrap(),
        ContextOutputRatioConfig {
            threshold: 50.5,
            min_context_tokens: 0,
        }
    );
    let negative = HotspotsOptions {
        context_output_ratio_threshold: Some(-1.0),
        ..HotspotsOptions::default()
    };
    let err = ContextOutputRatioConfig::from_hotspots_options(&negative).unwrap_err();
    assert_eq!(
        err.to_string(),
        "context-output ratio threshold must be finite and non-negative"
    );
}

#[test]
fn ratio_config_findings_apply_threshold_and_floor_to_turns() {
    let turns = [
        turn(
            "flagged",
            0,
            Usage {
                input: 600_000,
                output: 1_000,
                ..Usage::default()
            },
        ),
        turn(
            "below-ratio",
            0,
            Usage {
                input: 590_000,
                output: 1_000,
                ..Usage::default()
            },
        ),
        turn(
            "below-floor",
            0,
            Usage {
                input: 9_000,
                output: 1,
                ..Usage::default()
            },
        ),
    ];
    let config = ContextOutputRatioConfig {
        threshold: 600.0,
        min_context_tokens: 10_000,
    };
    let findings = config.findings(&turns);
    assert_eq!(
        findings
            .iter()
            .map(|finding| finding.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["flagged"]
    );
    assert_eq!(findings[0].title, "600.0:1 context-to-output ratio");
}
