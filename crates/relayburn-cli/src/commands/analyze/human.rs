//! Human report for `burn analyze`: identity and totals, then findings,
//! then one line or table per section.

use relayburn_sdk::{
    ActivityBreakdown, BashAggregation, FileAggregation, Finding, FindingImpact,
    HotspotsAttributionResult, OverheadReport as SessionOverheadReport, SessionAnalysis,
    StopReasonCounts, WasteSeverity,
};

use crate::render::format::{format_uint, format_usd, render_table};

/// Rows shown per table and per hotspot list.
const TOP: usize = 5;
/// Widest hotspot command shown; JSON keeps the full command.
const COMMAND_WIDTH: usize = 80;

pub(super) fn render(a: &SessionAnalysis) -> String {
    let mut out = Vec::new();
    header(a, &mut out);
    findings(&a.findings, &mut out);
    if let Some(activity) = a.activity.data() {
        activity_tables(activity, &mut out);
    }
    sections(a, &mut out);
    unavailable(a, &mut out);
    let mut text = out.join("\n");
    text.push('\n');
    text
}

fn field(label: &str, value: impl AsRef<str>) -> String {
    format!("{label:<14}{}", value.as_ref())
}

fn usd(cost: Option<f64>) -> String {
    cost.map(format_usd)
        .unwrap_or_else(|| "unknown (unpriced model)".to_string())
}

fn header(a: &SessionAnalysis, out: &mut Vec<String>) {
    let s = &a.session;
    out.push(field(
        "session",
        format!("{} ({})", s.session_id, s.harness),
    ));
    if let Some(path) = &s.transcript_path {
        out.push(field("transcript", path));
    }
    if let Some(project) = &s.project {
        out.push(field("project", project));
    }
    if !s.models.is_empty() {
        out.push(field("models", s.models.join(", ")));
    }
    out.push(field(
        "turns",
        format!(
            "{} assistant · {} user · {} tool calls · {} compactions",
            format_uint(s.turn_count),
            format_uint(s.user_turn_count),
            format_uint(s.tool_call_count),
            format_uint(s.compaction_count),
        ),
    ));
    if let Some(m) = a.metrics.data() {
        let u = &m.usage;
        out.push(field(
            "tokens",
            format!(
                "{} (input {} · output {} · cache read {} · cache write {} · reasoning {})",
                format_uint(u.total_tokens),
                format_uint(u.input_tokens),
                format_uint(u.output_tokens),
                format_uint(u.cache_read_tokens),
                format_uint(u.cache_write_tokens),
                format_uint(u.reasoning_tokens),
            ),
        ));
        out.push(field(
            "cost",
            usd(m.cost_usd_micros.map(|micros| micros as f64 / 1_000_000.0)),
        ));
    }
    let full = a.fidelity.summary["byClass"]["full"].as_u64().unwrap_or(0);
    out.push(field(
        "fidelity",
        format!(
            "{full} of {} turns full · {} attributable",
            s.turn_count, a.fidelity.attributable_turns
        ),
    ));
}

fn findings(findings: &[Finding], out: &mut Vec<String>) {
    out.push(String::new());
    if findings.is_empty() {
        out.push("findings      none".to_string());
        return;
    }
    out.push(format!("findings ({})", findings.len()));
    for f in findings {
        let severity = match f.severity {
            WasteSeverity::High => "high",
            WasteSeverity::Warn => "warn",
            WasteSeverity::Info => "info",
        };
        out.push(format!("  [{severity}] {}", f.title));
        let evidence = evidence_line(f);
        if !evidence.is_empty() {
            out.push(format!("         {evidence}"));
        }
        out.push(format!("         why  {}", f.explanation));
        out.push(format!("         fix  {}", f.suggestion));
    }
}

fn evidence_line(f: &Finding) -> String {
    let e = &f.evidence;
    let mut parts: Vec<String> = Vec::new();
    match (e.turn_indexes.first(), e.turn_indexes.last()) {
        (Some(first), Some(last)) if first != last => parts.push(format!("turns {first}-{last}")),
        (Some(only), _) => parts.push(format!("turn {only}")),
        _ if !e.turn_ids.is_empty() => parts.push(format!("turn {}", e.turn_ids.join(", "))),
        _ => {}
    }
    for (label, values) in [
        ("tools", &e.tools),
        ("files", &e.files),
        ("models", &e.models),
    ] {
        if !values.is_empty() {
            parts.push(format!("{label} {}", values.join(", ")));
        }
    }
    parts.extend(impact_parts(&f.impact));
    parts.join(" · ")
}

fn impact_parts(impact: &FindingImpact) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(tokens) = impact.tokens {
        parts.push(format!("{} tokens", format_uint(tokens)));
    }
    if let Some(cost) = impact.cost_usd {
        parts.push(format_usd(cost));
    }
    parts
}

fn activity_tables(activity: &ActivityBreakdown, out: &mut Vec<String>) {
    let mut rows = vec![vec![
        "category".to_string(),
        "turns".to_string(),
        "tokens".to_string(),
        "cost".to_string(),
    ]];
    rows.extend(activity.categories.iter().take(TOP).map(|row| {
        vec![
            row.category
                .map(|c| wire(&c))
                .unwrap_or_else(|| "unlabeled".to_string()),
            format_uint(row.turns),
            format_uint(row.tokens),
            usd(row.cost_usd),
        ]
    }));
    out.push(String::new());
    out.push(render_table(&rows));
    if activity.tools.is_empty() {
        return;
    }
    let mut rows = vec![vec![
        "tool".to_string(),
        "calls".to_string(),
        "errors".to_string(),
        "tokens".to_string(),
        "cost".to_string(),
    ]];
    rows.extend(activity.tools.iter().take(TOP).map(|row| {
        vec![
            row.tool.clone(),
            format_uint(row.calls),
            format_uint(row.errors),
            format_uint(row.tokens),
            usd(row.cost_usd),
        ]
    }));
    out.push(String::new());
    out.push(render_table(&rows));
}

fn sections(a: &SessionAnalysis, out: &mut Vec<String>) {
    out.push(String::new());
    let priced = a.metrics.data().is_some_and(|m| m.unpriced_turns == 0);
    if let Some(h) = a.hotspots.data() {
        hotspots(h, priced, out);
    }
    if let Some(o) = a.overhead.data() {
        out.push(field("overhead", overhead_line(o, priced)));
    }
    if let Some(c) = a.context.data() {
        out.push(field(
            "context",
            format!(
                "peak {} tokens at {} · {} compactions",
                format_uint(c.peak_context_tokens),
                c.peak_turn_id.as_deref().unwrap_or("-"),
                c.compactions.len()
            ),
        ));
    }
    if let Some(f) = a.flow.data() {
        out.push(field(
            "flow",
            format!(
                "{} inferences · {} tool uses · {} subagents · {} rail(s)",
                f.inferences, f.tool_uses, f.subagents, f.rails
            ),
        ));
    }
    if let Some(root) = a.subagents.data() {
        out.push(field(
            "subagents",
            match root.children.len() {
                0 => "none".to_string(),
                n if priced => format!("{n} · cumulative {}", format_usd(root.cumulative_cost)),
                n => format!("{n} · cumulative cost unknown (unpriced model)"),
            },
        ));
    }
    if let Some(outcome) = a.quality.data().and_then(|q| q.outcome.as_ref()) {
        out.push(field(
            "outcome",
            format!("{} ({})", wire(&outcome.outcome), wire(&outcome.reason)),
        ));
    }
    if let Some(stops) = a.stop_reasons.data() {
        out.push(field("stop reasons", stop_line(stops)));
    }
}

/// Cost when every turn is priced, else the tokens behind it.
fn spend(usd: f64, tokens: f64, priced: bool) -> String {
    if priced {
        format_usd(usd)
    } else {
        format!("{} tokens", format_uint(tokens.round() as u64))
    }
}

/// `text` on one line of at most [`COMMAND_WIDTH`] characters.
fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= COMMAND_WIDTH {
        return flat;
    }
    let cut: String = flat.chars().take(COMMAND_WIDTH - 1).collect();
    format!("{}…", cut.trim_end())
}

fn hotspots(h: &HotspotsAttributionResult, priced: bool, out: &mut Vec<String>) {
    let method = h
        .sessions
        .first()
        .map(|s| wire(&s.attribution_method))
        .unwrap_or_default();
    out.push(field(
        "hotspots",
        if priced {
            format!(
                "{} of {} attributed ({method})",
                format_usd(h.attributed_total),
                format_usd(h.grand_total)
            )
        } else {
            format!("cost unknown (unpriced model) · ranked by tokens ({method})")
        },
    ));
    let mut bash: Vec<&BashAggregation> = h.bash.iter().collect();
    let mut files: Vec<&FileAggregation> = h.files.iter().collect();
    if !priced {
        bash.sort_by(|x, y| {
            (y.initial_tokens + y.persistence_tokens)
                .total_cmp(&(x.initial_tokens + x.persistence_tokens))
        });
        files.sort_by(|x, y| {
            (y.initial_tokens + y.persistence_tokens)
                .total_cmp(&(x.initial_tokens + x.persistence_tokens))
        });
    }
    for row in bash.into_iter().take(TOP) {
        out.push(format!(
            "  command     {} · {} calls · {}",
            one_line(row.command.as_deref().unwrap_or(&row.args_hash)),
            row.call_count,
            spend(
                row.total_cost,
                row.initial_tokens + row.persistence_tokens,
                priced
            )
        ));
    }
    for row in files.into_iter().take(TOP) {
        out.push(format!(
            "  file        {} · {}",
            row.path,
            spend(
                row.total_cost,
                row.initial_tokens + row.persistence_tokens,
                priced
            )
        ));
    }
}

fn overhead_line(o: &SessionOverheadReport, priced: bool) -> String {
    let tokens: u64 = o
        .attribution
        .per_file
        .iter()
        .map(|f| {
            let rides = f
                .attribution
                .session_costs
                .iter()
                .map(|s| s.riding_turns)
                .max()
                .unwrap_or(0);
            f.attribution.total_tokens * rides
        })
        .sum();
    let cost = if priced {
        format_usd(o.attribution.grand_total)
    } else {
        "cost unknown (unpriced model)".to_string()
    };
    format!(
        "{} instruction file(s) · {} tokens re-read · {cost} · {} trim recommendation(s)",
        o.attribution.files.len(),
        format_uint(tokens),
        o.trim.recommendations.len()
    )
}

/// The serde wire name of an enum value.
fn wire<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn stop_line(s: &StopReasonCounts) -> String {
    [
        ("end_turn", s.end_turn),
        ("tool_use", s.tool_use),
        ("max_tokens", s.max_tokens),
        ("pause_turn", s.pause_turn),
        ("stop_sequence", s.stop_sequence),
        ("refusal", s.refusal),
        ("silent", s.silent),
        ("none", s.none),
    ]
    .iter()
    .filter(|(_, n)| *n > 0)
    .map(|(label, n)| format!("{label} {n}"))
    .collect::<Vec<_>>()
    .join(" · ")
}

fn unavailable(a: &SessionAnalysis, out: &mut Vec<String>) {
    let reasons: Vec<(&str, &str)> = [
        ("metrics", a.metrics.reason()),
        ("activity", a.activity.reason()),
        ("hotspots", a.hotspots.reason()),
        ("overhead", a.overhead.reason()),
        ("subagents", a.subagents.reason()),
        ("flow", a.flow.reason()),
        ("context", a.context.reason()),
        ("quality", a.quality.reason()),
        ("stop reasons", a.stop_reasons.reason()),
    ]
    .into_iter()
    .filter_map(|(name, reason)| Some((name, reason?)))
    .collect();
    if !reasons.is_empty() {
        out.push(String::new());
        out.push("unavailable".to_string());
        out.extend(
            reasons
                .iter()
                .map(|(name, reason)| format!("  {name:<15}{reason}")),
        );
    }
    if !a.skipped_checks.is_empty() {
        out.push(String::new());
        out.push("skipped checks".to_string());
        out.extend(
            a.skipped_checks
                .iter()
                .map(|c| format!("  {:<15}{}", c.check, c.reason)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relayburn_sdk::{analyze_session, AnalyzeSessionOptions, Harness, SessionLocator};

    #[test]
    fn long_commands_render_on_one_bounded_line() {
        assert_eq!(one_line("git status\n  && ls"), "git status && ls");
        let long = format!("cargo test {}", "x".repeat(200));
        let shown = one_line(&long);
        assert_eq!(shown.chars().count(), COMMAND_WIDTH);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn unpriced_sessions_render_tokens_and_never_dollars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unpriced.jsonl");
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/claude/retry-loop.jsonl"),
        )
        .unwrap()
        .replace("claude-sonnet-4-6", "unpriced-house-model");
        std::fs::write(&path, text).unwrap();
        let analysis = analyze_session(AnalyzeSessionOptions::new(SessionLocator::Path {
            harness: Harness::ClaudeCode,
            path,
        }))
        .unwrap();
        let report = render(&analysis);
        assert!(!report.contains('$'), "{report}");
        assert!(report.contains("hotspots      cost unknown (unpriced model)"));
        assert!(report.contains("npm run build · 4 calls ·"));
        assert!(report.contains(" tokens\n"));
    }
}
