//! `burn overhead` (and `burn overhead trim`) — estimate context
//! overhead and optionally surface trim recommendations.
//!
//! Thin presenter over `relayburn_sdk::overhead` and
//! `relayburn_sdk::overhead_trim`. TS source of truth:
//! `packages/cli/src/commands/overhead.ts`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use relayburn_sdk::{
    context_delta as sdk_context_delta, describe_applies_to, overhead as sdk_overhead,
    overhead_trim as sdk_overhead_trim, ContextDelta, ContextDeltaOpts,
    ContextDeltaOwnerRail as OwnerRail, InterveningStep, OverheadFileSummary, OverheadOptions,
    OverheadPerFileEntry, OverheadResult, OverheadSectionCost, OverheadTrimOptions,
    OverheadTrimResult,
};

use crate::cli::{GlobalArgs, OverheadAction, OverheadArgs, OverheadDeltasArgs};
use crate::render::error::report_error;
use crate::render::format::{
    coerce_whole_f64_to_int, format_tokens, format_uint, format_usd, render_table,
};
use crate::render::json::render_json;
use crate::render::progress::TaskProgress;

pub fn run(globals: &GlobalArgs, args: OverheadArgs) -> i32 {
    match args.action {
        Some(OverheadAction::Trim(trim)) => {
            run_trim(globals, args.project, args.since, args.kind, trim.top)
        }
        Some(OverheadAction::Deltas(deltas)) => run_deltas(globals, deltas),
        None => run_report(globals, args.project, args.since, args.kind),
    }
}

fn run_report(
    globals: &GlobalArgs,
    project: Option<PathBuf>,
    since: Option<String>,
    kind: Option<crate::cli::OverheadKind>,
) -> i32 {
    let project_path = resolve_project(project.as_deref());
    let opts = OverheadOptions {
        project: Some(project_path.clone()),
        since: since.clone(),
        kind: kind.map(Into::into),
        ledger_home: globals.ledger_path.clone(),
    };
    let progress = TaskProgress::new(globals, "overhead");
    progress.set_task("analyzing overhead files");
    let result = match sdk_overhead(opts) {
        Ok(r) => r,
        Err(err) => {
            progress.finish_and_clear();
            return report_error(&err, globals);
        }
    };
    progress.finish_and_clear();

    if result.files.is_empty() {
        let msg = match kind {
            Some(k) => format!(
                "no {} overhead files found at {}\n",
                kind_to_str(k),
                project_path.display()
            ),
            None => format!(
                "no overhead files found at {} (looked for CLAUDE.md, .claude/CLAUDE.md, AGENTS.md)\n",
                project_path.display()
            ),
        };
        let _ = io::stderr().write_all(msg.as_bytes());
        return 1;
    }

    if globals.json {
        let mut value = match serde_json::to_value(&result) {
            Ok(v) => v,
            Err(err) => return report_error(&io::Error::other(err), globals),
        };
        coerce_whole_f64_to_int(&mut value);
        if let Err(err) = render_json(&value) {
            return report_error(&err, globals);
        }
        return 0;
    }

    if let Err(err) = render_human_report(&result, since.as_deref()) {
        return report_error(&err, globals);
    }
    0
}

fn run_trim(
    globals: &GlobalArgs,
    project: Option<PathBuf>,
    since: Option<String>,
    kind: Option<crate::cli::OverheadKind>,
    top: Option<u64>,
) -> i32 {
    let project_path = resolve_project(project.as_deref());
    let opts = OverheadTrimOptions {
        project: Some(project_path.clone()),
        since: since.clone(),
        kind: kind.map(Into::into),
        ledger_home: globals.ledger_path.clone(),
        top,
        include_diff: None,
    };
    let progress = TaskProgress::new(globals, "overhead");
    progress.set_task("finding trim candidates");
    let result = match sdk_overhead_trim(opts) {
        Ok(r) => r,
        Err(err) => {
            progress.finish_and_clear();
            return report_error(&err, globals);
        }
    };
    progress.finish_and_clear();

    if result.summary.files_analyzed == 0 {
        let msg = match kind {
            Some(k) => format!(
                "no {} overhead files found at {}\n",
                kind_to_str(k),
                project_path.display()
            ),
            None => format!(
                "no overhead files found at {} (looked for CLAUDE.md, .claude/CLAUDE.md, AGENTS.md)\n",
                project_path.display()
            ),
        };
        let _ = io::stderr().write_all(msg.as_bytes());
        return 1;
    }

    if globals.json {
        let mut value = match serde_json::to_value(&result) {
            Ok(v) => v,
            Err(err) => return report_error(&io::Error::other(err), globals),
        };
        coerce_whole_f64_to_int(&mut value);
        if let Err(err) = render_json(&value) {
            return report_error(&err, globals);
        }
        return 0;
    }

    if let Err(err) = render_human_trim(&result) {
        return report_error(&err, globals);
    }
    0
}

fn resolve_project(project: Option<&Path>) -> PathBuf {
    match project {
        Some(p) => match std::fs::canonicalize(p) {
            Ok(c) => c,
            Err(_) => {
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    std::env::current_dir()
                        .map(|cwd| cwd.join(p))
                        .unwrap_or_else(|_| p.to_path_buf())
                }
            }
        },
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

fn kind_to_str(k: crate::cli::OverheadKind) -> &'static str {
    match k {
        crate::cli::OverheadKind::ClaudeMd => "claude-md",
        crate::cli::OverheadKind::AgentsMd => "agents-md",
    }
}

// ---------------------------------------------------------------------------
// Human renderers
// ---------------------------------------------------------------------------

fn render_human_report(result: &OverheadResult, since: Option<&str>) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    let since_label = since.unwrap_or("all time").to_string();

    // Build an array of lines and join with `\n` to mirror the TS sibling's
    // `out.join('\n')` semantics — the trailing-newline shape is sensitive to
    // this and the byte-equivalence golden snapshots compare on it.
    let mut lines: Vec<String> = Vec::new();
    lines.push(String::new());
    lines.push(format!("Overhead files in {}:", result.project));
    lines.push(String::new());

    for file_attr in &result.per_file {
        let parsed_file = result.files.iter().find(|f| f.path == file_attr.path);
        if let Some(pf) = parsed_file {
            push_file_block(pf, file_attr, &since_label, &mut lines);
            lines.push(String::new());
        }
    }

    lines.push(format!(
        "Grand total (all overhead files, {since_label}): {}",
        format_usd(result.grand_total),
    ));
    lines.push(String::new());

    let joined = lines.join("\n");
    handle.write_all(joined.as_bytes())?;
    handle.flush()?;
    Ok(())
}

fn push_file_block(
    parsed: &OverheadFileSummary,
    fa: &OverheadPerFileEntry,
    since_label: &str,
    lines: &mut Vec<String>,
) {
    let display = format_file_display(&parsed.path);
    let applies_to = describe_applies_to(&parsed.applies_to);
    lines.push(format!(
        "{display} — {} lines, ~{} tokens — applies to: {applies_to}",
        format_uint(parsed.total_lines),
        format_tokens(parsed.tokens),
    ));
    if parsed.tokens == 0 {
        lines.push("  (empty file — no attribution)".to_string());
        return;
    }
    let attribution = &fa.attribution;
    if attribution.session_count == 0 {
        lines.push("  no matching sessions in window.".to_string());
        return;
    }
    lines.push(format!(
        "  Cost per session:   avg {}, p95 {}",
        format_usd(attribution.per_session_avg),
        format_usd(attribution.per_session_p95),
    ));
    lines.push(format!(
        "  Cost over {since_label}: {} across {} session{}",
        format_usd(attribution.total_cost),
        format_uint(attribution.session_count),
        if attribution.session_count == 1 {
            ""
        } else {
            "s"
        },
    ));
    lines.push("  Sections ranked by cost:".to_string());
    if attribution.section_costs.is_empty() {
        lines.push("    (no sections)".to_string());
        return;
    }
    let table = render_section_table(&attribution.section_costs);
    // Indent each table line by four spaces, mirroring `indent(text, '    ')`
    // in the TS sibling.
    let indented = table
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    lines.push(indented);
}

fn format_file_display(path: &str) -> String {
    let p = Path::new(path);
    let basename = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let rel = match p.strip_prefix(&cwd) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
        _ => path.to_string(),
    };
    format!("{basename} ({rel})")
}

fn render_section_table(rows: &[OverheadSectionCost]) -> String {
    let mut data: Vec<Vec<String>> = Vec::with_capacity(rows.len() + 1);
    data.push(vec![
        "lines".to_string(),
        "heading".to_string(),
        "tokens".to_string(),
        "cost/session".to_string(),
        "%file".to_string(),
    ]);
    for r in rows {
        data.push(vec![
            format_line_range(r.section.start_line, r.section.end_line),
            r.section.heading.clone(),
            format_tokens(r.section.tokens),
            format_usd(r.cost_per_session),
            format!("{:.1}%", r.token_share * 100.0),
        ]);
    }
    render_table(&data)
}

fn render_human_trim(result: &OverheadTrimResult) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    if result.recommendations.is_empty() {
        return handle.write_all(
            "# no trim candidates — overhead files have no headed sections\n".as_bytes(),
        );
    }

    // Group by `file` while preserving insertion order.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<
        String,
        Vec<&relayburn_sdk::OverheadTrimRecommendation>,
    > = std::collections::HashMap::new();
    for rec in &result.recommendations {
        groups
            .entry(rec.file.clone())
            .or_insert_with(|| {
                order.push(rec.file.clone());
                Vec::new()
            })
            .push(rec);
    }

    // Mirror the TS sibling: build an array of lines/blocks and join with `\n`
    // so the trailing blank-vs-no-blank semantics match byte-for-byte.
    let mut lines: Vec<String> = Vec::new();
    lines.push("# burn overhead trim — projected savings if trimmed".to_string());
    lines.push("# (recommendations only; burn never modifies your overhead files)".to_string());
    lines.push(String::new());
    for file in &order {
        let recs = &groups[file];
        let first = recs[0];
        let basename = Path::new(&first.file)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| first.file.clone());
        let applies_to = describe_applies_to(&first.applies_to);
        lines.push(format!("# === {basename} (applies to: {applies_to}) ==="));
        lines.push(String::new());
        for rec in recs {
            lines.push(rec.diff.clone().unwrap_or_default());
            lines.push(String::new());
        }
    }

    let joined = lines.join("\n");
    handle.write_all(joined.as_bytes())?;
    handle.flush()?;
    Ok(())
}

fn format_line_range(start: u64, end: u64) -> String {
    let s = format!("{start:>4}");
    let e = format!("{end:>4}");
    format!("{s}-{e}")
}

// ---------------------------------------------------------------------------
// `burn overhead deltas` (#432)
// ---------------------------------------------------------------------------

fn run_deltas(globals: &GlobalArgs, args: OverheadDeltasArgs) -> i32 {
    let opts = ContextDeltaOpts {
        session: args.session.clone(),
        since: None,
        top: args.top,
        min_delta: args.min_delta,
        owner: args.owner.into(),
    };
    let progress = TaskProgress::new(globals, "overhead deltas");
    progress.set_task("computing context deltas");
    let deltas = match sdk_context_delta(opts, globals.ledger_path.clone()) {
        Ok(d) => d,
        Err(err) => {
            progress.finish_and_clear();
            return report_error(&err, globals);
        }
    };
    progress.finish_and_clear();

    if globals.json {
        let mut value = match serde_json::to_value(&deltas) {
            Ok(v) => v,
            Err(err) => return report_error(&io::Error::other(err), globals),
        };
        coerce_whole_f64_to_int(&mut value);
        if let Err(err) = render_json(&value) {
            return report_error(&err, globals);
        }
        return 0;
    }

    if let Err(err) = render_human_deltas(&deltas, args.explain) {
        return report_error(&err, globals);
    }
    0
}

fn render_human_deltas(deltas: &[ContextDelta], explain: bool) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    if deltas.is_empty() {
        return handle.write_all(b"# no context deltas above threshold\n");
    }

    let mut table: Vec<Vec<String>> = Vec::with_capacity(deltas.len() + 1);
    table.push(vec![
        "Inference".to_string(),
        "Owner".to_string(),
        "Delta".to_string(),
        "Cost".to_string(),
        "Driver".to_string(),
    ]);
    for d in deltas {
        let inf_label = format!("{}/inf{}", short_turn_label(&d.turn_id), d.inference_idx);
        let owner_label = match &d.owner_rail {
            OwnerRail::Main => "main".to_string(),
            OwnerRail::Subagent { agent_id } => format!("sub:{}", short_agent_label(agent_id)),
        };
        let delta_label = format_signed_tokens(d.delta_tokens);
        let cost_label = format_usd(d.attributed_cost_usd);
        let driver_label = driver_summary(&d.intervening);
        table.push(vec![
            inf_label,
            owner_label,
            delta_label,
            cost_label,
            driver_label,
        ]);
    }
    handle.write_all(render_table(&table).as_bytes())?;
    handle.write_all(b"\n")?;

    if explain {
        handle.write_all(b"\n")?;
        for d in deltas {
            let inf_label = format!("{}/inf{}", short_turn_label(&d.turn_id), d.inference_idx);
            let header = format!(
                "{inf_label} — {} steps, prior {} -> current {} tok\n",
                d.intervening.len(),
                format_tokens(d.prior_context_tokens),
                format_tokens(d.current_context_tokens),
            );
            handle.write_all(header.as_bytes())?;
            for step in &d.intervening {
                let line = format!("    - {}\n", explain_step(step));
                handle.write_all(line.as_bytes())?;
            }
        }
    }

    handle.write_all(
        b"\n# token / cost figures are approximate (bytes/4 for tool results,\n\
          # cache-read rate for cost). Compaction rows surface separately and\n\
          # never appear as negative deltas.\n",
    )?;
    handle.flush()?;
    Ok(())
}

fn short_turn_label(turn_id: &str) -> String {
    // Turn ids on Claude are `msg-...` UUIDs; trim to a short prefix
    // for the table. Keep the original for JSON output.
    let trimmed = turn_id.trim_start_matches("msg_");
    let trimmed = trimmed.trim_start_matches("msg-");
    if trimmed.len() > 8 {
        format!("T{}", &trimmed[..8])
    } else {
        format!("T{trimmed}")
    }
}

fn short_agent_label(agent_id: &str) -> String {
    let trimmed = agent_id.trim_start_matches("agent-");
    if trimmed.len() > 8 {
        trimmed[..8].to_string()
    } else {
        trimmed.to_string()
    }
}

fn format_signed_tokens(n: i64) -> String {
    let sign = if n > 0 { "+" } else { "" };
    format!("{sign}{}", format_tokens(n.unsigned_abs()))
}

fn driver_summary(steps: &[InterveningStep]) -> String {
    if steps.is_empty() {
        return "(no intervening leaves)".to_string();
    }
    // Largest step by approx_tokens, with a "N steps" suffix when more
    // than one. Compaction rows always win their summary because
    // freeing tokens is the most explanatory signal.
    if let Some(comp) = steps
        .iter()
        .find(|s| matches!(s, InterveningStep::Compaction { .. }))
    {
        return comp.driver_label();
    }
    let largest = steps
        .iter()
        .max_by_key(|s| s.approx_tokens())
        .expect("non-empty");
    let extra = steps.len().saturating_sub(1);
    if extra == 0 {
        largest.driver_label()
    } else {
        format!(
            "{} (+{extra} more step{})",
            largest.driver_label(),
            if extra == 1 { "" } else { "s" }
        )
    }
}

fn explain_step(step: &InterveningStep) -> String {
    match step {
        InterveningStep::ToolResult {
            tool_use_id,
            tool_name,
            approx_tokens,
            approx_bytes,
            truncated,
        } => format!(
            "tool_result {tool_name} (id={tool_use_id}): ~{} tok / {} bytes{}",
            format_tokens(*approx_tokens),
            format_uint(*approx_bytes),
            if *truncated { " [truncated]" } else { "" },
        ),
        InterveningStep::UserPrompt {
            approx_tokens,
            has_system_reminder,
        } => format!(
            "user prompt: ~{} tok{}",
            format_tokens(*approx_tokens),
            if *has_system_reminder {
                " (with system-reminder)"
            } else {
                ""
            },
        ),
        InterveningStep::SystemReminder {
            source,
            approx_tokens,
        } => format!(
            "system-reminder ({source:?}): ~{} tok",
            format_tokens(*approx_tokens),
        ),
        InterveningStep::Compaction { tokens_freed } => {
            format!("compaction: -{} tok freed", format_tokens(*tokens_freed))
        }
        InterveningStep::Other => "other".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_line_range_pads_to_four() {
        assert_eq!(format_line_range(7, 11), "   7-  11");
        assert_eq!(format_line_range(100, 200), " 100- 200");
    }

    #[test]
    fn short_turn_label_trims_msg_prefix() {
        assert_eq!(short_turn_label("msg_abcdef1234"), "Tabcdef12");
        assert_eq!(short_turn_label("msg-deadbeef"), "Tdeadbeef");
        assert_eq!(short_turn_label("xyz"), "Txyz");
    }

    #[test]
    fn driver_summary_singles_out_compaction() {
        let steps = vec![
            InterveningStep::ToolResult {
                tool_use_id: "tu-1".into(),
                tool_name: "Bash".into(),
                approx_tokens: 100,
                approx_bytes: 400,
                truncated: false,
            },
            InterveningStep::Compaction {
                tokens_freed: 5000,
            },
        ];
        let s = driver_summary(&steps);
        assert!(s.contains("compaction"));
    }

    #[test]
    fn driver_summary_picks_largest_step() {
        let steps = vec![
            InterveningStep::ToolResult {
                tool_use_id: "tu-1".into(),
                tool_name: "Bash".into(),
                approx_tokens: 100,
                approx_bytes: 400,
                truncated: false,
            },
            InterveningStep::ToolResult {
                tool_use_id: "tu-2".into(),
                tool_name: "Read".into(),
                approx_tokens: 5000,
                approx_bytes: 20_000,
                truncated: false,
            },
        ];
        let s = driver_summary(&steps);
        assert!(s.contains("Read"), "got {s}");
        assert!(s.contains("more"), "got {s}");
    }

    #[test]
    fn format_signed_tokens_handles_positive_and_zero() {
        assert_eq!(format_signed_tokens(0), "0");
        assert!(format_signed_tokens(5_000).starts_with('+'));
    }
}
