//! The reasoning-effort table of the grouped `burn summary`.

use relayburn_sdk::ReasoningEffortRow;

use crate::render::format::{format_uint, format_usd, render_table};

/// The table and its trailing blank line; nothing when no turn recorded an
/// effort.
pub(super) fn reasoning_effort_lines(rows: &[ReasoningEffortRow]) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    vec![format_reasoning_efforts(rows), String::new()]
}

/// One row per recorded reasoning effort; unpriced cost reads `unpriced`.
fn format_reasoning_efforts(rows: &[ReasoningEffortRow]) -> String {
    let mut table = vec![vec![
        "reasoning effort".to_string(),
        "turns".to_string(),
        "tokens".to_string(),
        "reasoning".to_string(),
        "cost".to_string(),
    ]];
    table.extend(rows.iter().map(|row| {
        vec![
            row.effort
                .clone()
                .unwrap_or_else(|| "unrecorded".to_string()),
            format_uint(row.turns),
            format_uint(row.tokens),
            format_uint(row.reasoning_tokens),
            row.cost_usd
                .map(format_usd)
                .unwrap_or_else(|| "unpriced".to_string()),
        ]
    }));
    render_table(&table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(effort: Option<&str>, cost_usd: Option<f64>) -> ReasoningEffortRow {
        ReasoningEffortRow {
            effort: effort.map(str::to_string),
            turns: 2,
            tokens: 12_000,
            reasoning_tokens: 3_000,
            cost_usd,
            reasoning_cost_usd: cost_usd.map(|c| c / 4.0),
            activities: Vec::new(),
        }
    }

    #[test]
    fn renders_one_row_per_effort_and_never_prices_unpriced_rows() {
        let text = format_reasoning_efforts(&[
            row(Some("low"), Some(0.02)),
            row(Some("high"), None),
            row(None, Some(0.01)),
        ]);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("reasoning effort"), "{text}");
        assert!(lines[1].starts_with("low") && lines[1].contains("12,000"));
        assert!(lines[1].contains("3,000") && lines[1].contains('$'));
        assert!(lines[2].starts_with("high") && lines[2].ends_with("unpriced"));
        assert!(lines[3].starts_with("unrecorded"));
    }

    #[test]
    fn no_recorded_effort_renders_nothing() {
        assert!(reasoning_effort_lines(&[]).is_empty());
        let lines = reasoning_effort_lines(&[row(Some("low"), None)]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1], "");
    }
}
