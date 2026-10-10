//! Self-contained sections of the grouped human `burn summary` report: the
//! context-efficiency headline and session table, and the subagent line.

use relayburn_sdk::{ContextEfficiencySummary, SubagentCounts};

use crate::render::format::{format_uint, render_table};

pub(super) fn format_context_efficiency_line(efficiency: &ContextEfficiencySummary) -> String {
    format!(
        "context efficiency: {} ({} context / {} output; {} zero-output turn{})",
        format_context_ratio(
            efficiency.context_tokens_per_output_token,
            efficiency.unbounded,
        ),
        format_uint(efficiency.context_tokens),
        format_uint(efficiency.output_tokens),
        format_uint(efficiency.zero_output_turns_with_context),
        if efficiency.zero_output_turns_with_context == 1 {
            ""
        } else {
            "s"
        },
    )
}

/// Heading, table, and trailing blank line for the highest-ratio sessions;
/// empty when the report carries no session rows.
pub(super) fn context_efficiency_session_lines(
    efficiency: &ContextEfficiencySummary,
) -> Vec<String> {
    if efficiency.sessions.is_empty() {
        return Vec::new();
    }
    let mut context_rows = vec![vec![
        "session".into(),
        "turns".into(),
        "total context".into(),
        "context:output".into(),
        "p50 context".into(),
        "p95 context".into(),
        "max context".into(),
    ]];
    for session in &efficiency.sessions {
        context_rows.push(vec![
            session.session_id.clone(),
            format_uint(session.turn_count),
            format_uint(session.context_tokens),
            format_context_ratio(session.context_tokens_per_output_token, session.unbounded),
            format_uint(session.context_size.p50),
            format_uint(session.context_size.p95),
            format_uint(session.context_size.max),
        ]);
    }
    vec![
        format!(
            "highest context-efficiency sessions ({} of {} sessions):",
            format_uint(efficiency.sessions.len() as u64),
            format_uint(efficiency.total_sessions),
        ),
        render_table(&context_rows),
        String::new(),
    ]
}

fn format_context_ratio(ratio: Option<f64>, unbounded: bool) -> String {
    if unbounded {
        "unbounded".to_string()
    } else {
        ratio
            .map(|value| format!("{value:.1}:1"))
            .unwrap_or_else(|| "—".to_string())
    }
}

/// Human-readable subagent line for `burn summary`, e.g.
/// `subagents: 2 paired, 1 orphan`. Both counts are rendered so the line
/// is informative even when one bucket is zero — an orphan-only count
/// flags slash-command synthetic dispatches as a non-trivial signal.
/// See AgentWorkforce/burn#435.
pub(super) fn format_subagents_line(s: &SubagentCounts) -> String {
    format!(
        "subagents: {} paired, {} orphan",
        format_uint(s.paired),
        format_uint(s.orphan),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use relayburn_sdk::{ContextSizeDistribution, SessionContextEfficiency};

    #[test]
    fn context_ratio_renders_unbounded_undefined_and_one_decimal() {
        assert_eq!(format_context_ratio(None, true), "unbounded");
        assert_eq!(format_context_ratio(Some(4.0), true), "unbounded");
        assert_eq!(format_context_ratio(None, false), "—");
        assert_eq!(format_context_ratio(Some(382.04), false), "382.0:1");
    }

    #[test]
    fn context_efficiency_line_pluralizes_zero_output_turns() {
        let mut efficiency = ContextEfficiencySummary {
            context_tokens: 1_146_000,
            output_tokens: 3_000,
            context_tokens_per_output_token: Some(382.0),
            zero_output_turns_with_context: 1,
            ..ContextEfficiencySummary::default()
        };
        assert_eq!(
            format_context_efficiency_line(&efficiency),
            "context efficiency: 382.0:1 (1,146,000 context / 3,000 output; 1 zero-output turn)"
        );
        efficiency.zero_output_turns_with_context = 2;
        assert_eq!(
            format_context_efficiency_line(&efficiency),
            "context efficiency: 382.0:1 (1,146,000 context / 3,000 output; 2 zero-output turns)"
        );
    }

    #[test]
    fn session_lines_are_empty_without_sessions() {
        let efficiency = ContextEfficiencySummary {
            total_sessions: 3,
            ..ContextEfficiencySummary::default()
        };
        assert!(context_efficiency_session_lines(&efficiency).is_empty());
    }

    #[test]
    fn session_lines_render_heading_table_and_blank_line() {
        let efficiency = ContextEfficiencySummary {
            total_sessions: 1_200,
            sessions: vec![
                SessionContextEfficiency {
                    session_id: "high".to_string(),
                    turn_count: 1_001,
                    context_tokens: 2_292_000,
                    output_tokens: 3_000,
                    context_tokens_per_output_token: Some(764.0),
                    context_size: ContextSizeDistribution {
                        p50: 1_500,
                        p95: 9_000,
                        max: 12_000,
                    },
                    ..SessionContextEfficiency::default()
                },
                SessionContextEfficiency {
                    session_id: "zero-output".to_string(),
                    turn_count: 2,
                    context_tokens: 4_000,
                    unbounded: true,
                    ..SessionContextEfficiency::default()
                },
            ],
            ..ContextEfficiencySummary::default()
        };
        let row = |cells: [&str; 7]| cells.map(str::to_string).to_vec();
        assert_eq!(
            context_efficiency_session_lines(&efficiency),
            vec![
                "highest context-efficiency sessions (2 of 1,200 sessions):".to_string(),
                render_table(&[
                    row([
                        "session",
                        "turns",
                        "total context",
                        "context:output",
                        "p50 context",
                        "p95 context",
                        "max context",
                    ]),
                    row([
                        "high",
                        "1,001",
                        "2,292,000",
                        "764.0:1",
                        "1,500",
                        "9,000",
                        "12,000",
                    ]),
                    row(["zero-output", "2", "4,000", "unbounded", "0", "0", "0"]),
                ]),
                String::new(),
            ]
        );
    }
}
