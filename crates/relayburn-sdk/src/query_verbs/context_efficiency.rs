use super::*;

use std::cmp::Ordering;

/// Default session context-to-output ratio that produces a hotspot finding.
/// The boundary is inclusive, so the motivating 382:1 incident qualifies.
pub const DEFAULT_CONTEXT_OUTPUT_RATIO_THRESHOLD: f64 = 382.0;

/// Default minimum session context volume for the ratio finding. This keeps
/// low-volume sessions out of the inspection signal without coupling it to
/// dollar cost. Set the corresponding option to zero to disable the floor.
pub const DEFAULT_CONTEXT_OUTPUT_MIN_TOKENS: u64 = 1_000_000;

/// Maximum per-session distribution rows included in default summary
/// surfaces. The full set remains available to internal findings evaluation.
pub const SUMMARY_CONTEXT_SESSION_LIMIT: usize = 10;

/// Context-window size percentiles across the turns in one session.
///
/// Percentiles use the nearest-rank rule (`ceil(p * n) - 1`) used by the
/// other summary distributions. For one turn p50, p95, and max are equal;
/// for two sorted turns p50 is the smaller value and p95 is the larger.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSizeDistribution {
    pub p50: u64,
    pub p95: u64,
    pub max: u64,
}

/// First-class context-efficiency metric for one inference turn.
///
/// A context-consuming zero-output turn is `unbounded` with a `None` ratio;
/// an empty 0/0 turn is undefined (`None`) but not unbounded. This shape is
/// always legal JSON and is the unit rolled into session totals below.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnContextEfficiency {
    pub context_tokens: u64,
    pub output_tokens: u64,
    pub context_tokens_per_output_token: Option<f64>,
    pub unbounded: bool,
}

/// Context efficiency rolled up for one session.
///
/// `context_tokens_per_output_token` is the ratio of session totals, not an
/// average of per-turn ratios. It is `None` when the session produced no
/// output, keeping JSON free of `Infinity`/`NaN`; `unbounded` distinguishes a
/// context-consuming zero-output session from an empty 0/0 session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionContextEfficiency {
    pub session_id: String,
    pub turn_count: u64,
    pub context_tokens: u64,
    pub output_tokens: u64,
    pub context_tokens_per_output_token: Option<f64>,
    pub unbounded: bool,
    pub zero_output_turns_with_context: u64,
    pub context_size: ContextSizeDistribution,
}

/// Context efficiency for a filtered summary and its constituent sessions.
///
/// A context token is an input, cache-read, or cache-creation token:
/// `input + cache_read + cache_create_5m + cache_create_1h`. All four occupy
/// the inference context and are paid input-side work. All generated output
/// is the denominator. Codex reports reasoning inside
/// `usage.output`, so its raw output is retained; harnesses that report
/// reasoning separately add it to raw output. The denominator therefore
/// means all generated tokens consistently across harnesses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEfficiencySummary {
    pub context_tokens: u64,
    pub output_tokens: u64,
    pub context_tokens_per_output_token: Option<f64>,
    pub unbounded: bool,
    pub zero_output_turns_with_context: u64,
    /// Distinct sessions in the filtered summary, including zero-token rows.
    pub total_sessions: u64,
    /// Sessions meeting the default 1M-context findings eligibility floor.
    pub eligible_sessions: u64,
    /// Session distributions. The default summary verbs keep only the ten
    /// highest-ratio sessions; the full compute helper returns all.
    pub sessions: Vec<SessionContextEfficiency>,
}

/// Per-turn context tokens: input plus cache reads and both cache-creation
/// buckets. Output and reasoning tokens deliberately do not occupy this side
/// of the metric.
pub fn context_tokens(usage: &crate::reader::Usage) -> u64 {
    usage
        .input
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_create_5m)
        .saturating_add(usage.cache_create_1h)
}

/// Normalize all generated tokens for one turn. Sources such as Codex already
/// include reasoning in `usage.output`; other sources add separately reported
/// reasoning so identical generation has identical denominator semantics.
pub fn generated_output_tokens(turn: &TurnRecord) -> u64 {
    if crate::analyze::reasoning_mode_for_source(turn.source)
        == Some(crate::analyze::ReasoningMode::IncludedInOutput)
    {
        turn.usage.output
    } else {
        turn.usage.output.saturating_add(turn.usage.reasoning)
    }
}

/// Compute the context-to-generated-output metric for one inference turn.
pub fn context_efficiency_for_turn(turn: &TurnRecord) -> TurnContextEfficiency {
    let context = context_tokens(&turn.usage);
    let output = generated_output_tokens(turn);
    TurnContextEfficiency {
        context_tokens: context,
        output_tokens: output,
        context_tokens_per_output_token: ratio(context, output),
        unbounded: context > 0 && output == 0,
    }
}

/// Compute context efficiency per turn and roll it up by session and across
/// the complete filtered slice. Ratios divide aggregate context by aggregate
/// generated output so large turns carry their natural token weight.
pub fn compute_context_efficiency(turns: &[TurnRecord]) -> ContextEfficiencySummary {
    #[derive(Default)]
    struct Acc {
        turn_count: u64,
        context_tokens: u64,
        output_tokens: u64,
        zero_output_turns_with_context: u64,
        context_sizes: Vec<u64>,
    }

    let mut by_session: HashMap<String, Acc> = HashMap::new();
    let mut total = Acc::default();

    for turn in turns {
        let turn_efficiency = context_efficiency_for_turn(turn);
        let context = turn_efficiency.context_tokens;
        let output = turn_efficiency.output_tokens;
        let zero_output_with_context = u64::from(turn_efficiency.unbounded);

        total.turn_count += 1;
        total.context_tokens = total.context_tokens.saturating_add(context);
        total.output_tokens = total.output_tokens.saturating_add(output);
        total.zero_output_turns_with_context += zero_output_with_context;

        let session = by_session.entry(turn.session_id.clone()).or_default();
        session.turn_count += 1;
        session.context_tokens = session.context_tokens.saturating_add(context);
        session.output_tokens = session.output_tokens.saturating_add(output);
        session.zero_output_turns_with_context += zero_output_with_context;
        session.context_sizes.push(context);
    }

    let mut sessions: Vec<SessionContextEfficiency> = by_session
        .into_iter()
        .map(|(session_id, mut acc)| {
            acc.context_sizes.sort_unstable();
            SessionContextEfficiency {
                session_id,
                turn_count: acc.turn_count,
                context_tokens: acc.context_tokens,
                output_tokens: acc.output_tokens,
                context_tokens_per_output_token: ratio(acc.context_tokens, acc.output_tokens),
                unbounded: acc.context_tokens > 0 && acc.output_tokens == 0,
                zero_output_turns_with_context: acc.zero_output_turns_with_context,
                context_size: ContextSizeDistribution {
                    p50: context_percentile(&acc.context_sizes, 0.50),
                    p95: context_percentile(&acc.context_sizes, 0.95),
                    max: acc.context_sizes.last().copied().unwrap_or(0),
                },
            }
        })
        .collect();

    sessions.sort_by(|a, b| {
        b.unbounded
            .cmp(&a.unbounded)
            .then_with(|| {
                b.context_tokens_per_output_token
                    .partial_cmp(&a.context_tokens_per_output_token)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| b.context_size.max.cmp(&a.context_size.max))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    let total_sessions = sessions.len() as u64;
    let eligible_sessions = sessions
        .iter()
        .filter(|session| session.context_tokens >= DEFAULT_CONTEXT_OUTPUT_MIN_TOKENS)
        .count() as u64;

    ContextEfficiencySummary {
        context_tokens: total.context_tokens,
        output_tokens: total.output_tokens,
        context_tokens_per_output_token: ratio(total.context_tokens, total.output_tokens),
        unbounded: total.context_tokens > 0 && total.output_tokens == 0,
        zero_output_turns_with_context: total.zero_output_turns_with_context,
        total_sessions,
        eligible_sessions,
        sessions,
    }
}

/// Compute the bounded context-efficiency projection shipped by default
/// summary surfaces. It retains only the ten highest-ratio sessions,
/// preventing all-time summaries from materializing tens of thousands of
/// per-session objects without hiding distributions for lower-volume ledgers.
pub(crate) fn compute_context_efficiency_for_summary(
    turns: &[TurnRecord],
) -> ContextEfficiencySummary {
    let mut summary = compute_context_efficiency(turns);
    summary.sessions.truncate(SUMMARY_CONTEXT_SESSION_LIMIT);
    summary
}

pub(crate) fn validate_context_output_ratio_threshold(threshold: f64) -> Result<()> {
    if !threshold.is_finite() || threshold < 0.0 {
        anyhow::bail!("context-output ratio threshold must be finite and non-negative");
    }
    Ok(())
}

pub(crate) fn context_output_ratio_findings(
    efficiency: &ContextEfficiencySummary,
    threshold: f64,
    min_context_tokens: u64,
) -> Vec<WasteFinding> {
    efficiency
        .sessions
        .iter()
        .filter(|session| {
            session.context_tokens >= min_context_tokens
                && (session.unbounded
                    || session
                        .context_tokens_per_output_token
                        .is_some_and(|ratio| ratio >= threshold))
        })
        .map(|session| {
            let high = session.unbounded
                || session
                    .context_tokens_per_output_token
                    .is_some_and(|ratio| ratio >= threshold * 2.0);
            let ratio_label = session
                .context_tokens_per_output_token
                .map(|ratio| format!("{ratio:.1}:1"))
                .unwrap_or_else(|| "unbounded (zero output)".to_string());
            crate::analyze::context_output_ratio_finding(
                crate::analyze::ContextOutputRatioFindingInput {
                    session_id: &session.session_id,
                    high,
                    ratio_label: &ratio_label,
                    context_tokens: session.context_tokens,
                    output_tokens: session.output_tokens,
                    threshold,
                    min_context_tokens,
                },
            )
        })
        .collect()
}

fn ratio(context: u64, output: u64) -> Option<f64> {
    (output > 0).then(|| context as f64 / output as f64)
}

fn context_percentile(sorted: &[u64], percentile: f64) -> u64 {
    let values: Vec<f64> = sorted.iter().map(|value| *value as f64).collect();
    super::summary::summary_percentile(&values, percentile) as u64
}

#[cfg(test)]
mod tests {
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
}
