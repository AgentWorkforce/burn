//! Human-readable table rendering for `burn hotspots`.

use relayburn_sdk::{
    AttributionMethod, BashAggregation, BashVerbAggregation, FileAggregation,
    HotspotsAttributionResult, HotspotsExcludedBreakdown, HotspotsExcludedSourceRow,
    HotspotsResult, McpServerAggregation, SubagentAggregation, WasteFinding, WasteSeverity,
};

use crate::render::format::{format_uint, format_usd, render_table};

use super::*;

pub(super) fn emit_human(
    result: &HotspotsResult,
    limit: usize,
    findings_view: bool,
    rank_by: RankBy,
) {
    match result {
        HotspotsResult::Attribution(a) => emit_human_attribution(a, limit, rank_by),
        // The single-axis group_by surfaces aren't yet tied to a golden
        // snapshot (the snapshot covers the default attribution view),
        // so we render their tables on a best-effort basis with the same
        // shared-format helpers. If/when goldens land for these, the
        // renderers can be tightened; the JSON path is already exact.
        HotspotsResult::Bash {
            refused: Some(true),
            refusal_reason,
            ..
        } => {
            print_refusal(refusal_reason.as_deref());
        }
        HotspotsResult::Bash { rows, .. } => {
            let (heading, sorted) = sort_bash(rows, rank_by);
            print_section_table(
                heading,
                "(no Bash tool calls)",
                sorted.into_iter().take(limit).map(bash_row),
                &[
                    "command",
                    "calls",
                    "initial(tok)",
                    "persist(tok)",
                    "bytes",
                    "cost",
                ],
            );
        }
        HotspotsResult::BashVerb {
            refused: Some(true),
            refusal_reason,
            ..
        } => {
            print_refusal(refusal_reason.as_deref());
        }
        HotspotsResult::BashVerb { rows, .. } => {
            let (heading, sorted) = sort_bash_verb(rows, rank_by);
            print_section_table(
                heading,
                "(no Bash tool calls)",
                sorted.into_iter().take(limit).map(bash_verb_row),
                &[
                    "verb",
                    "calls",
                    "commands",
                    "initial(tok)",
                    "persist(tok)",
                    "avgRide",
                    "bytes",
                    "cost",
                    "examples",
                ],
            );
        }
        HotspotsResult::File {
            refused: Some(true),
            refusal_reason,
            ..
        } => {
            print_refusal(refusal_reason.as_deref());
        }
        HotspotsResult::File { rows, .. } => {
            let (heading, sorted) = sort_file(rows, rank_by);
            print_section_table(
                heading,
                "(no Read/Edit/Write tool calls)",
                sorted.into_iter().take(limit).map(|f| file_row(f, 0.0)),
                &[
                    "path",
                    "firstTurn",
                    "initial(tok)",
                    "persist(tok)",
                    "rideTurns",
                    "bytes",
                    "cost",
                    "%attr",
                ],
            );
        }
        HotspotsResult::Subagent {
            refused: Some(true),
            refusal_reason,
            ..
        } => {
            print_refusal(refusal_reason.as_deref());
        }
        HotspotsResult::Subagent { rows, .. } => {
            let (heading, sorted) = sort_subagent(rows, rank_by);
            print_section_table(
                heading,
                "(no Agent/Task tool calls)",
                sorted.into_iter().take(limit).map(subagent_row),
                &[
                    "subagent",
                    "calls",
                    "initial(tok)",
                    "persist(tok)",
                    "bytes",
                    "cost",
                ],
            );
        }
        HotspotsResult::Findings { findings, .. } => {
            if findings_view {
                emit_findings_unified(findings);
            } else {
                emit_findings_grouped(findings, limit);
            }
        }
    }
}

fn emit_findings_unified(findings: &[WasteFinding]) {
    let mut out: Vec<String> = Vec::new();
    out.push(String::new());
    out.push(format!("findings: {}", format_uint(findings.len() as u64)));
    out.push(String::new());
    if findings.is_empty() {
        out.push("  (no hotspot findings)".to_string());
        out.push(String::new());
        print!("{}", out.join("\n"));
        return;
    }
    let mut rows: Vec<Vec<String>> = vec![vec![
        "severity".into(),
        "kind".into(),
        "session".into(),
        "usd".into(),
        "title".into(),
    ]];
    for f in findings {
        let usd = f
            .estimated_savings
            .usd_per_session
            .map(format_usd)
            .unwrap_or_else(|| "—".to_string());
        rows.push(vec![
            severity_label(f.severity).to_string(),
            f.kind.clone(),
            f.session_id.chars().take(8).collect(),
            usd,
            truncate(&f.title, 80),
        ]);
    }
    out.push(render_table(&rows));
    out.push(String::new());
    print!("{}", out.join("\n"));
}

fn emit_findings_grouped(findings: &[WasteFinding], limit: usize) {
    let mut out: Vec<String> = Vec::new();
    out.push(String::new());
    out.push(format!("findings: {}", format_uint(findings.len() as u64)));
    out.push(String::new());
    if findings.is_empty() {
        out.push("  (no hotspot findings)".to_string());
        out.push(String::new());
        print!("{}", out.join("\n"));
        return;
    }
    // Group by detector kind, preserving severity-sorted order of the
    // sdk-emitted slice. Within each group we cap at `limit`.
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<&str, Vec<&WasteFinding>> = BTreeMap::new();
    for f in findings {
        groups.entry(f.kind.as_str()).or_default().push(f);
    }
    for (kind, items) in &groups {
        out.push(format!("{} ({})", kind, format_uint(items.len() as u64)));
        let mut rows: Vec<Vec<String>> = vec![vec![
            "severity".into(),
            "session".into(),
            "usd".into(),
            "title".into(),
        ]];
        for f in items.iter().take(limit) {
            let usd = f
                .estimated_savings
                .usd_per_session
                .map(format_usd)
                .unwrap_or_else(|| "—".to_string());
            rows.push(vec![
                severity_label(f.severity).to_string(),
                f.session_id.chars().take(8).collect(),
                usd,
                truncate(&f.title, 70),
            ]);
        }
        out.push(render_table(&rows));
        out.push(String::new());
    }
    print!("{}", out.join("\n"));
}

fn severity_label(s: WasteSeverity) -> &'static str {
    match s {
        WasteSeverity::High => "high",
        WasteSeverity::Warn => "warn",
        WasteSeverity::Info => "info",
    }
}

fn print_refusal(reason: Option<&str>) {
    let mut out = String::new();
    out.push('\n');
    if let Some(r) = reason {
        out.push_str(r);
        out.push('\n');
    } else {
        out.push_str("hotspots refused for the matched slice.\n");
    }
    print!("{}", out);
}

fn print_section_table<I, F>(heading: &str, empty_msg: &str, rows: I, header: &[&str])
where
    I: Iterator<Item = F>,
    F: IntoIterator<Item = String>,
{
    let mut lines = Vec::<String>::new();
    lines.push(String::new());
    lines.push(heading.to_string());
    let body: Vec<Vec<String>> = rows.map(|r| r.into_iter().collect()).collect();
    if body.is_empty() {
        lines.push(format!("  {}", empty_msg));
    } else {
        let mut all_rows: Vec<Vec<String>> =
            vec![header.iter().map(|s| (*s).to_string()).collect()];
        all_rows.extend(body);
        lines.push(render_table(&all_rows));
    }
    lines.push(String::new());
    print!("{}", lines.join("\n"));
}

fn emit_human_attribution(a: &HotspotsAttributionResult, limit: usize, rank_by: RankBy) {
    let degraded = a.attribution_degraded;
    let approx_suffix = if degraded { " (approximate)" } else { "" };
    let rank_suffix = match rank_by {
        RankBy::Cost => "",
        RankBy::Bytes => " (ranked by bytes)",
    };
    let mut out: Vec<String> = Vec::new();
    out.push(String::new());
    out.push(format!("turns analyzed: {}", format_uint(a.turns_analyzed)));
    if let Some(notice) = coverage_notice(a) {
        out.push(notice);
    }
    out.push(format!(
        "session grand total: {}",
        format_usd(a.grand_total)
    ));

    if degraded {
        let total = a.sessions.len();
        let ev = a
            .sessions
            .iter()
            .filter(|s| matches!(s.attribution_method, AttributionMethod::EvenSplit))
            .count();
        let pct = if total > 0 {
            (ev as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        out.push(String::new());
        out.push(format!(
            "⚠ attribution is degraded: {} of {} sessions ({:.1}%) have no sized",
            format_uint(ev as u64),
            format_uint(total as u64),
            pct,
        ));
        out.push(
            "  tool-result data, so file / bash / subagent costs for those sessions are approximate"
                .to_string(),
        );
        out.push(
            "  (even-split over turn N+1 input/cacheCreate). Run 'burn state rebuild content'"
                .to_string(),
        );
        out.push("  to backfill source-derived sizes, or see 'burn state' for".to_string());
        out.push("  why capture is disabled.".to_string());
        out.push(String::new());
        out.push(format!(
            "attributed ≈ {}  (approximate — see above)",
            format_usd(a.attributed_total)
        ));
        out.push(format!(
            "unattributed {}  (output, system overhead, untracked)",
            format_usd(a.unattributed_total)
        ));
    } else {
        out.push(format!(
            "attributed to tool calls: {}  /  unattributed (output, system overhead, untracked): {}",
            format_usd(a.attributed_total),
            format_usd(a.unattributed_total),
        ));
        let total = a.sessions.len();
        let ev = a
            .sessions
            .iter()
            .filter(|s| matches!(s.attribution_method, AttributionMethod::EvenSplit))
            .count();
        if ev > 0 && ev == total {
            out.push(
                "note: no user-turn or content sidecar sizes found — using even-split (initial cost only). Run burn state rebuild content or enable content.store=full to improve attribution.".to_string(),
            );
        } else if ev > 0 {
            out.push(format!(
                "note: {}/{} sessions used even-split (no user-turn or content sidecar sizes).",
                ev, total
            ));
        }
    }
    out.push(String::new());

    let (file_heading, files_sorted) = sort_file(&a.files, rank_by);
    out.push(format!("{}{}{}", file_heading, rank_suffix, approx_suffix));
    if files_sorted.is_empty() {
        out.push("  (no Read/Edit/Write tool calls)".to_string());
    } else {
        let header: Vec<String> = vec![
            "path".into(),
            "firstTurn".into(),
            "initial(tok)".into(),
            "persist(tok)".into(),
            "rideTurns".into(),
            "bytes".into(),
            "cost".into(),
            "%attr".into(),
        ];
        let mut rows: Vec<Vec<String>> = vec![header];
        for f in files_sorted.iter().take(limit) {
            rows.push(file_row(f, a.attributed_total));
        }
        out.push(render_table(&rows));
    }
    out.push(String::new());

    let (verb_heading, verbs_sorted) = sort_bash_verb(&a.bash_verbs, rank_by);
    out.push(format!("{}{}{}", verb_heading, rank_suffix, approx_suffix));
    if verbs_sorted.is_empty() {
        out.push("  (no Bash tool calls)".to_string());
    } else {
        let header: Vec<String> = vec![
            "verb".into(),
            "calls".into(),
            "commands".into(),
            "initial(tok)".into(),
            "persist(tok)".into(),
            "avgRide".into(),
            "bytes".into(),
            "cost".into(),
            "examples".into(),
        ];
        let mut rows: Vec<Vec<String>> = vec![header];
        for b in verbs_sorted.iter().take(limit) {
            rows.push(bash_verb_row(b));
        }
        out.push(render_table(&rows));
    }
    out.push(String::new());

    let (bash_heading, bash_sorted) = sort_bash(&a.bash, rank_by);
    out.push(format!("{}{}{}", bash_heading, rank_suffix, approx_suffix));
    if bash_sorted.is_empty() {
        out.push("  (no Bash tool calls)".to_string());
    } else {
        let header: Vec<String> = vec![
            "command".into(),
            "calls".into(),
            "initial(tok)".into(),
            "persist(tok)".into(),
            "bytes".into(),
            "cost".into(),
        ];
        let mut rows: Vec<Vec<String>> = vec![header];
        for b in bash_sorted.iter().take(limit) {
            rows.push(bash_row(b));
        }
        out.push(render_table(&rows));
    }
    out.push(String::new());

    let (sub_heading, subs_sorted) = sort_subagent(&a.subagents, rank_by);
    out.push(format!("{}{}{}", sub_heading, rank_suffix, approx_suffix));
    if subs_sorted.is_empty() {
        out.push("  (no Agent/Task tool calls)".to_string());
    } else {
        let header: Vec<String> = vec![
            "subagent".into(),
            "calls".into(),
            "initial(tok)".into(),
            "persist(tok)".into(),
            "bytes".into(),
            "cost".into(),
        ];
        let mut rows: Vec<Vec<String>> = vec![header];
        for s in subs_sorted.iter().take(limit) {
            rows.push(subagent_row(s));
        }
        out.push(render_table(&rows));
    }
    out.push(String::new());

    if !a.mcp_servers.is_empty() {
        out.push(format!("Top MCP servers by cost{}", approx_suffix));
        let header: Vec<String> = vec![
            "server".into(),
            "calls".into(),
            "initial(tok)".into(),
            "persist(tok)".into(),
            "rideTurns".into(),
            "cost".into(),
            "topTools".into(),
        ];
        let mut rows: Vec<Vec<String>> = vec![header];
        for m in a.mcp_servers.iter().take(limit) {
            rows.push(mcp_server_row(m));
        }
        out.push(render_table(&rows));
        out.push(String::new());
    }

    print!("{}", out.join("\n"));
}

fn coverage_notice(a: &HotspotsAttributionResult) -> Option<String> {
    let analyzed = a.fidelity.analyzed;
    let excluded = a.fidelity.excluded;
    if excluded == 0 {
        return None;
    }
    let total = analyzed + excluded;
    // The TS shape is one inline clause per source kind, joined with " and ".
    // Each clause names the missing field(s) + the granularity bucket the
    // excluded turns carried, with the source name in parens. Sources are
    // walked in BTreeMap order for stable rendering. The breakdown is
    // computed by the SDK in the same pass that produced the rest of the
    // attribution result — no second ledger walk here.
    let clauses: Vec<String> = render_source_clauses(&a.fidelity.excluded_by_source);
    let suffix = if clauses.is_empty() {
        // Fall back to the SDK's aggregate counts if the breakdown is empty
        // (e.g. a turn without a fidelity record that the SDK still excluded).
        // Don't fabricate a source label.
        String::new()
    } else {
        format!(" for {}", clauses.join(" and "))
    };
    Some(format!(
        "analyzed {} of {} turns; {} excluded{}",
        format_uint(analyzed),
        format_uint(total),
        format_uint(excluded),
        suffix,
    ))
}

fn render_source_clauses(breakdown: &HotspotsExcludedBreakdown) -> Vec<String> {
    breakdown
        .sources
        .iter()
        .map(|(source, row)| render_inline_source_clause(source, row))
        .collect()
}

fn render_inline_source_clause(source: &str, row: &HotspotsExcludedSourceRow) -> String {
    let mut inner: Vec<String> = Vec::new();
    if !row.missing.is_empty() {
        let missing: Vec<&str> = row.missing.iter().map(String::as_str).collect();
        inner.push(format!("missing {}", missing.join(", ")));
    }
    if !row.granularities.is_empty() {
        let grans: Vec<&str> = row.granularities.iter().map(String::as_str).collect();
        inner.push(format!("{} granularity", grans.join("+")));
    }
    if inner.is_empty() {
        source.to_string()
    } else {
        format!("{} ({})", inner.join(", "), source)
    }
}

fn file_row(f: &FileAggregation, attributed: f64) -> Vec<String> {
    let pct = if attributed > 0.0 {
        (f.total_cost / attributed) * 100.0
    } else {
        0.0
    };
    vec![
        f.path.clone(),
        f.first_emit_turn_index.to_string(),
        format_uint(f.initial_tokens.round() as u64),
        format_uint(f.persistence_tokens.round() as u64),
        format_uint(f.riding_turns),
        format_bytes_cell(f.total_output_bytes, f.truncated_count),
        format_usd(f.total_cost),
        format!("{:.1}%", pct),
    ]
}

fn bash_row(b: &BashAggregation) -> Vec<String> {
    let label = match &b.command {
        Some(c) => truncate(c, 60),
        None => format!("(hash {})", &b.args_hash[..8.min(b.args_hash.len())]),
    };
    vec![
        label,
        format_uint(b.call_count),
        format_uint(b.initial_tokens.round() as u64),
        format_uint(b.persistence_tokens.round() as u64),
        format_bytes_cell(b.total_output_bytes, b.truncated_count),
        format_usd(b.total_cost),
    ]
}

fn bash_verb_row(b: &BashVerbAggregation) -> Vec<String> {
    vec![
        b.verb.clone(),
        format_uint(b.call_count),
        format_uint(b.distinct_commands),
        format_uint(b.initial_tokens.round() as u64),
        format_uint(b.persistence_tokens.round() as u64),
        format!("{:.1}", b.avg_persistence_turns),
        format_bytes_cell(b.total_output_bytes, b.truncated_count),
        format_usd(b.total_cost),
        truncate(
            &b.top_examples
                .iter()
                .map(|e| truncate(e, 40))
                .collect::<Vec<_>>()
                .join("; "),
            90,
        ),
    ]
}

fn subagent_row(s: &SubagentAggregation) -> Vec<String> {
    vec![
        s.subagent_type.clone(),
        format_uint(s.call_count),
        format_uint(s.initial_tokens.round() as u64),
        format_uint(s.persistence_tokens.round() as u64),
        format_bytes_cell(s.total_output_bytes, s.truncated_count),
        format_usd(s.total_cost),
    ]
}

fn mcp_server_row(m: &McpServerAggregation) -> Vec<String> {
    vec![
        m.server.clone(),
        format_uint(m.call_count),
        format_uint(m.initial_tokens.round() as u64),
        format_uint(m.persistence_tokens.round() as u64),
        format_uint(m.riding_turns),
        format_usd(m.total_cost),
        truncate(
            &m.top_tools
                .iter()
                .map(|t| truncate(t, 40))
                .collect::<Vec<_>>()
                .join("; "),
            90,
        ),
    ]
}

/// Render a byte count using IEC-style suffixes (KB / MB / GB) so 4 MB
/// Bash blowouts read as "4 MB" rather than a raw 7-digit integer. Zero
/// bytes render as `-` to make "no payload measured" visually distinct
/// from "0 byte payload". A trailing `*` appears when at least one call
/// in the bucket had a truncation marker.
fn format_bytes_cell(bytes: u64, truncated_count: u32) -> String {
    let base = if bytes == 0 {
        "-".to_string()
    } else {
        format_bytes(bytes)
    };
    if truncated_count > 0 {
        format!("{base}*")
    } else {
        base
    }
}

fn format_bytes(bytes: u64) -> String {
    // Decimal (SI) units — closer to what JS `humansize` defaults to and
    // what most CLI users expect for stdout payloads.
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ---- sort helpers ---------------------------------------------------------

fn sort_file(rows: &[FileAggregation], rank_by: RankBy) -> (&'static str, Vec<&FileAggregation>) {
    let mut out: Vec<&FileAggregation> = rows.iter().collect();
    match rank_by {
        RankBy::Cost => (
            "Top files by cumulative cost",
            // SDK already returns rows sorted by cost desc; preserve that.
            out,
        ),
        RankBy::Bytes => {
            out.sort_by_key(|b| std::cmp::Reverse(b.total_output_bytes));
            ("Top files by output bytes", out)
        }
    }
}

fn sort_bash(rows: &[BashAggregation], rank_by: RankBy) -> (&'static str, Vec<&BashAggregation>) {
    let mut out: Vec<&BashAggregation> = rows.iter().collect();
    match rank_by {
        RankBy::Cost => ("Top exact Bash commands by cost", out),
        RankBy::Bytes => {
            out.sort_by_key(|b| std::cmp::Reverse(b.total_output_bytes));
            ("Top exact Bash commands by output bytes", out)
        }
    }
}

fn sort_bash_verb(
    rows: &[BashVerbAggregation],
    rank_by: RankBy,
) -> (&'static str, Vec<&BashVerbAggregation>) {
    let mut out: Vec<&BashVerbAggregation> = rows.iter().collect();
    match rank_by {
        RankBy::Cost => ("Top Bash verbs by cost", out),
        RankBy::Bytes => {
            out.sort_by_key(|b| std::cmp::Reverse(b.total_output_bytes));
            ("Top Bash verbs by output bytes", out)
        }
    }
}

fn sort_subagent(
    rows: &[SubagentAggregation],
    rank_by: RankBy,
) -> (&'static str, Vec<&SubagentAggregation>) {
    let mut out: Vec<&SubagentAggregation> = rows.iter().collect();
    match rank_by {
        RankBy::Cost => ("Top subagent calls by cost", out),
        RankBy::Bytes => {
            out.sort_by_key(|b| std::cmp::Reverse(b.total_output_bytes));
            ("Top subagent calls by output bytes", out)
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    let mut out: String = chars.iter().take(n - 1).collect();
    out.push('…');
    out
}
