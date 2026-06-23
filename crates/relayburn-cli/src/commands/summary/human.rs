//! Human-readable table rendering and ingest-prelude text for `burn summary`.

use relayburn_sdk::{
    summary_fidelity_summary_to_value, summary_replacement_savings_to_value, CoverageField,
    FidelityClass, FidelitySummary, OutcomeLabel, QualityResult, RelationshipType,
    StopReasonCounts, SubagentCounts, SubagentTreeNode, SubagentTypeStats, SummaryByToolReport,
    SummaryGroupBy, SummaryGroupedReport, SummaryRelationshipReport, SummarySubagentTreeReport,
    UsageCostAggregateRow,
};
use serde_json::{json, Map, Value};

use crate::cli::GlobalArgs;
use crate::render::format::{coerce_whole_f64_to_int, format_uint, format_usd, render_table};
use crate::render::json::render_json;

use super::*;

const COVERAGE_FIELDS: [CoverageField; 5] = [
    CoverageField::Input,
    CoverageField::Output,
    CoverageField::Reasoning,
    CoverageField::CacheRead,
    CoverageField::CacheCreate,
];

pub(super) fn cell_is_partial(c: &relayburn_sdk::FieldCoverage) -> bool {
    c.known > 0 && c.missing > 0
}

const PARTIAL_MARK: &str = "*";
const DASH: &str = "—";

pub(super) fn coverage_field_label(field: CoverageField) -> &'static str {
    match field {
        CoverageField::Input => "input",
        CoverageField::Output => "output",
        CoverageField::Reasoning => "reasoning",
        CoverageField::CacheRead => "cacheRead",
        CoverageField::CacheCreate => "cacheCreate",
    }
}

/// Render one token-field cell. Three cases:
///   - every contributing turn reported the field → numeric value, no marker
///   - some turns reported, some didn't           → numeric value + `*`
///   - no turn reported                           → `—` (never `0`, which
///     would falsely claim a real zero from the source)
pub(super) fn coverage_cell(value: u64, c: &relayburn_sdk::FieldCoverage) -> String {
    if c.known == 0 && c.missing > 0 {
        return DASH.to_string();
    }
    if c.known > 0 && c.missing > 0 {
        return format!("{}{}", format_uint(value), PARTIAL_MARK);
    }
    format_uint(value)
}

pub(super) fn emit_grouped(
    globals: &GlobalArgs,
    report: &SummaryGroupedReport,
    ingest_report: &relayburn_sdk::IngestReport,
) -> std::io::Result<()> {
    if globals.json {
        return emit_json(report, ingest_report);
    }
    emit_human(report, ingest_report);
    Ok(())
}

pub(super) fn emit_ingest_prelude(
    globals: &GlobalArgs,
    ingest_report: &relayburn_sdk::IngestReport,
) {
    if globals.json {
        return;
    }
    emit_human_ingest_prelude(ingest_report);
}

pub(super) fn emit_human_ingest_prelude(ingest_report: &relayburn_sdk::IngestReport) {
    print!("{}", ingest_prelude_text(ingest_report));
}

pub(super) fn ingest_prelude_text(ingest_report: &relayburn_sdk::IngestReport) -> String {
    format!(
        "\ningested {} new session{} (+{} turns)",
        ingest_report.ingested_sessions,
        if ingest_report.ingested_sessions == 1 {
            ""
        } else {
            "s"
        },
        format_uint(ingest_report.appended_turns as u64),
    ) + "\n"
}

pub(super) fn render_by_tool_report(
    globals: &GlobalArgs,
    report: &SummaryByToolReport,
    ingest_report: &relayburn_sdk::IngestReport,
) -> anyhow::Result<i32> {
    if globals.json {
        let by_tool_json: Vec<Value> = report
            .rows
            .iter()
            .map(|item| {
                let mut row = Map::new();
                row.insert("tool".into(), json!(item.tool));
                row.insert("calls".into(), json!(item.calls));
                row.insert("attributedCost".into(), json!(item.attributed_cost));
                row.insert(
                    "attributionMethod".into(),
                    json!(item.attribution_method.wire_str()),
                );
                if let Some(s) = item.savings.as_ref() {
                    row.insert(
                        "savings".into(),
                        json!({
                            "calls": s.calls,
                            "collapsedCalls": s.collapsed_calls,
                            "estimatedTokensSaved": s.estimated_tokens_saved,
                        }),
                    );
                }
                Value::Object(row)
            })
            .collect();
        let mut payload = json!({
            "ingest": {
                "ingestedSessions": ingest_report.ingested_sessions,
                "appendedTurns": ingest_report.appended_turns,
            },
            "turns": report.turn_count,
            "byTool": by_tool_json,
            "unattributed": report.unattributed_cost,
            "fidelity": { "summary": summary_fidelity_summary_to_value(&report.fidelity) },
        });
        if report.replacement_savings.calls > 0 {
            payload.as_object_mut().unwrap().insert(
                "replacementSavings".into(),
                summary_replacement_savings_to_value(&report.replacement_savings),
            );
        }
        coerce_whole_f64_to_int(&mut payload);
        render_json(&payload)?;
        return Ok(0);
    }

    let mut out = Vec::new();
    out.push(String::new());
    out.push(format!(
        "turns analyzed: {}",
        format_uint(report.turn_count)
    ));
    out.push(String::new());
    if report.rows.is_empty() {
        out.push("no tool calls found for filters.".to_string());
        let mut text = out.join("\n");
        text.push('\n');
        print!("{text}");
        return Ok(0);
    }

    let has_savings = report.replacement_savings.calls > 0;
    let mut rows: Vec<Vec<String>> = if has_savings {
        vec![vec![
            "tool".into(),
            "calls".into(),
            "attributedCost".into(),
            "savedTokens".into(),
        ]]
    } else {
        vec![vec!["tool".into(), "calls".into(), "attributedCost".into()]]
    };
    for item in &report.rows {
        let mut row = vec![
            item.tool.clone(),
            format_uint(item.calls),
            format_usd(item.attributed_cost),
        ];
        if has_savings {
            let saved = item
                .savings
                .as_ref()
                .map(|s| format_uint(s.estimated_tokens_saved))
                .unwrap_or_else(|| "-".to_string());
            row.push(saved);
        }
        rows.push(row);
    }
    out.push(render_table(&rows));
    out.push(String::new());
    out.push(
        "attributedCost = turn N ingest cost assigned to turn N-1 tool_use blocks by user-turn byte size when available, otherwise split evenly.".to_string(),
    );
    out.push(format!(
        "unattributed cost (no prior tool call or non-tool user text): {}",
        format_usd(report.unattributed_cost),
    ));
    if has_savings {
        out.push(format_replacement_savings_line(&report.replacement_savings));
    }
    out.push(String::new());
    print!("{}", out.join("\n"));
    Ok(0)
}

pub(super) fn render_subagent_type_report(
    globals: &GlobalArgs,
    stats: &[SubagentTypeStats],
) -> anyhow::Result<i32> {
    if globals.json {
        let mut value = serde_json::to_value(stats)?;
        coerce_whole_f64_to_int(&mut value);
        render_json(&value)?;
        return Ok(0);
    }

    let mut out = Vec::new();
    out.push(String::new());
    out.push(format!(
        "subagent invocations: {}",
        format_uint(stats.iter().map(|s| s.invocations).sum()),
    ));
    out.push(String::new());
    if stats.is_empty() {
        out.push("  (no subagent turns in range)".to_string());
        out.push(String::new());
        print!("{}", out.join("\n"));
        return Ok(0);
    }
    out.push(render_subagent_stats_table(stats));
    out.push(String::new());
    print!("{}", out.join("\n"));
    Ok(0)
}

pub(super) fn render_subagent_stats_table(stats: &[SubagentTypeStats]) -> String {
    let mut rows = vec![vec![
        "subagentType".into(),
        "invocations".into(),
        "turns".into(),
        "total".into(),
        "median".into(),
        "p95".into(),
        "mean".into(),
    ]];
    for s in stats {
        rows.push(vec![
            s.subagent_type.clone(),
            format_uint(s.invocations),
            format_uint(s.turns),
            format_usd(s.total_cost),
            format_usd(s.median_cost),
            format_usd(s.p95_cost),
            format_usd(s.mean_cost),
        ]);
    }
    render_table(&rows)
}

const NO_RELATIONSHIPS_MESSAGE: &str =
    "no SessionRelationshipRecord rows found for the matched slice; ingest a session with execution-graph wiring or run `burn state rebuild` once relationship backfill is available";

pub(super) fn render_relationship_report(
    globals: &GlobalArgs,
    report: &SummaryRelationshipReport,
) -> anyhow::Result<i32> {
    if !report.subagent_types.is_empty() {
        return render_relationship_subagent_report(globals, report);
    }
    if report.relationships.is_empty() {
        return render_no_relationships(globals);
    }

    if globals.json {
        let mut value = json!({ "relationships": report.relationships });
        coerce_whole_f64_to_int(&mut value);
        render_json(&value)?;
        return Ok(0);
    }

    let mut out = Vec::new();
    out.push(String::new());
    out.push(format!(
        "relationships: {}",
        format_uint(report.relationships.iter().map(|s| s.count).sum()),
    ));
    out.push(String::new());
    let mut rows = vec![vec![
        "relationshipType".into(),
        "count".into(),
        "turnCount".into(),
        "total".into(),
        "median".into(),
        "p95".into(),
        "mean".into(),
    ]];
    for s in &report.relationships {
        rows.push(vec![
            s.relationship_type.wire_str().to_string(),
            format_uint(s.count),
            format_uint(s.turn_count),
            format_usd(s.total_cost),
            format_usd(s.median_cost),
            format_usd(s.p95_cost),
            format_usd(s.mean_cost),
        ]);
    }
    out.push(render_table(&rows));
    out.push(String::new());
    print!("{}", out.join("\n"));
    Ok(0)
}

pub(super) fn render_relationship_subagent_report(
    globals: &GlobalArgs,
    report: &SummaryRelationshipReport,
) -> anyhow::Result<i32> {
    if report.subagent_types.is_empty() {
        if globals.json {
            let mut value = json!({
                "relationships": [],
                "subagentTypes": [],
                "message": NO_RELATIONSHIPS_MESSAGE,
            });
            coerce_whole_f64_to_int(&mut value);
            render_json(&value)?;
            return Ok(0);
        }
        return render_no_relationships(globals);
    }
    if globals.json {
        let mut value = json!({
            "relationships": report.relationships,
            "subagentTypes": report.subagent_types,
        });
        coerce_whole_f64_to_int(&mut value);
        render_json(&value)?;
        return Ok(0);
    }

    let mut out = Vec::new();
    out.push(String::new());
    out.push(format!(
        "subagent invocations: {}",
        format_uint(report.subagent_types.iter().map(|s| s.invocations).sum()),
    ));
    out.push(String::new());
    let mut rows = vec![vec![
        "subagentType".into(),
        "invocations".into(),
        "turns".into(),
        "total".into(),
        "median".into(),
        "p95".into(),
        "mean".into(),
    ]];
    for s in &report.subagent_types {
        rows.push(vec![
            s.subagent_type.clone(),
            format_uint(s.invocations),
            format_uint(s.turns),
            format_usd(s.total_cost),
            format_usd(s.median_cost),
            format_usd(s.p95_cost),
            format_usd(s.mean_cost),
        ]);
    }
    out.push(render_table(&rows));
    out.push(String::new());
    print!("{}", out.join("\n"));
    Ok(0)
}

pub(super) fn render_no_relationships(globals: &GlobalArgs) -> anyhow::Result<i32> {
    if globals.json {
        render_json(&json!({
            "relationships": [],
            "message": NO_RELATIONSHIPS_MESSAGE,
        }))?;
    } else {
        println!("{NO_RELATIONSHIPS_MESSAGE}");
    }
    Ok(0)
}

pub(super) fn render_subagent_tree_report(
    globals: &GlobalArgs,
    report: &SummarySubagentTreeReport,
) -> anyhow::Result<i32> {
    if globals.json {
        let root = match report.root.as_ref() {
            Some(root) => serde_json::to_value(root)?,
            None => Value::Null,
        };
        let mut value = json!({
            "sessionId": report.session_id.as_str(),
            "root": root,
        });
        coerce_whole_f64_to_int(&mut value);
        render_json(&value)?;
        return Ok(0);
    }

    let Some(root) = report.root.as_ref() else {
        println!("no turns found for session {}", report.session_id);
        return Ok(0);
    };

    let mut out = Vec::new();
    out.push(String::new());
    out.push(format!("session: {}", report.session_id));
    out.push(format!(
        "total: {} across {} turn{}",
        format_usd(root.cumulative_cost),
        format_uint(root.cumulative_turns),
        if root.cumulative_turns == 1 { "" } else { "s" },
    ));
    out.push(String::new());
    out.extend(render_tree(root));
    out.push(String::new());
    print!("{}", out.join("\n"));
    Ok(0)
}

pub(super) fn render_tree(root: &SubagentTreeNode) -> Vec<String> {
    let mut out = Vec::new();
    out.push(render_node_line(root, ""));
    render_children(root, "", &mut out);
    out
}

pub(super) fn render_children(node: &SubagentTreeNode, prefix: &str, out: &mut Vec<String>) {
    let n = node.children.len();
    for (i, child) in node.children.iter().enumerate() {
        let is_last = i == n - 1;
        let branch = if is_last { "└─ " } else { "├─ " };
        out.push(render_node_line(child, &format!("{prefix}{branch}")));
        let child_prefix = if is_last {
            format!("{prefix}   ")
        } else {
            format!("{prefix}│  ")
        };
        render_children(child, &child_prefix, out);
    }
}

pub(super) fn render_node_line(node: &SubagentTreeNode, indent: &str) -> String {
    let relationship = if node.relationship_type != RelationshipType::Root
        && node.relationship_type != RelationshipType::Subagent
    {
        format!(" [{}]", node.relationship_type.wire_str())
    } else {
        String::new()
    };
    let model = if node.models.is_empty() {
        String::new()
    } else {
        format!(" ({})", node.models.join(", "))
    };
    format!(
        "{}{}{}{}  {}  [{} turn{}]",
        indent,
        node.label,
        relationship,
        model,
        format_usd(node.cumulative_cost),
        format_uint(node.cumulative_turns),
        if node.cumulative_turns == 1 { "" } else { "s" },
    )
}

pub(super) fn emit_human(
    report: &SummaryGroupedReport,
    ingest_report: &relayburn_sdk::IngestReport,
) {
    let mut lines: Vec<String> = Vec::new();
    emit_human_ingest_prelude(ingest_report);
    lines.push(String::new());

    lines.push(format!(
        "turns analyzed: {}",
        format_uint(report.turn_count)
    ));
    lines.push(String::new());

    if report.rows.is_empty() {
        lines.push("no turns match the current filters.".to_string());
        let mut out = lines.join("\n");
        out.push('\n');
        print!("{}", out);
        return;
    }

    let header_label = if report.group_by == SummaryGroupBy::Tag {
        "value"
    } else {
        report.group_by.wire_str()
    };
    let header = vec![
        header_label.to_string(),
        "turns".into(),
        "input".into(),
        "output".into(),
        "reasoning".into(),
        "cacheRead".into(),
        "cacheCreate".into(),
        "cost".into(),
    ];
    let mut rendered: Vec<Vec<String>> = vec![header];
    let mut any_partial = false;
    for r in &report.rows {
        if cell_is_partial(&r.coverage.input)
            || cell_is_partial(&r.coverage.output)
            || cell_is_partial(&r.coverage.reasoning)
            || cell_is_partial(&r.coverage.cache_read)
            || cell_is_partial(&r.coverage.cache_create)
        {
            any_partial = true;
        }
        rendered.push(vec![
            r.label.clone(),
            format_uint(r.turns),
            coverage_cell(r.usage.input, &r.coverage.input),
            coverage_cell(r.usage.output, &r.coverage.output),
            coverage_cell(r.usage.reasoning, &r.coverage.reasoning),
            coverage_cell(r.usage.cache_read, &r.coverage.cache_read),
            coverage_cell(
                r.usage.cache_create_5m + r.usage.cache_create_1h,
                &r.coverage.cache_create,
            ),
            format_usd(r.cost.total),
        ]);
    }
    lines.push(render_table(&rendered));
    lines.push(String::new());
    lines.push(format!(
        "total cost: {}",
        format_usd(report.total_cost.total)
    ));
    lines.push(format!(
        "  input {} / output {} / reasoning {} / cacheRead {} / cacheCreate {}",
        format_usd(report.total_cost.input),
        format_usd(report.total_cost.output),
        format_usd(report.total_cost.reasoning),
        format_usd(report.total_cost.cache_read),
        format_usd(report.total_cost.cache_create),
    ));
    lines.push(String::new());

    if report.replacement_savings.calls > 0 {
        lines.push(format_replacement_savings_line(&report.replacement_savings));
        lines.push(String::new());
    }

    if !report.stop_reasons.is_empty() {
        lines.push(format_stop_reasons_line(&report.stop_reasons));
        lines.push(String::new());
    }

    if !report.subagents.is_empty() {
        // `subagents: X paired, Y orphan` — paired sidecars resolved
        // via `toolUseResult.agentId`; orphans are the `UnattachedGroup`
        // bucket (slash-command synthetic dispatches and crash-mid-
        // dispatch sidecars). See AgentWorkforce/burn#435.
        lines.push(format_subagents_line(&report.subagents));
        lines.push(String::new());
    }

    if any_partial {
        lines.push(format_partial_footer(&report.rows));
        lines.push(String::new());
    }

    if let Some(notice) = render_fidelity_notice(&report.fidelity) {
        lines.push(notice);
        lines.push(String::new());
    }

    if let Some(q) = report.quality.as_ref() {
        lines.push(render_quality(q));
        lines.push(String::new());
    }

    let out = lines.join("\n");
    // TS uses `process.stdout.write(lines.join('\n'))` — no trailing newline.
    print!("{}", out);

    if report.unpriced_turns > 0 {
        let models = report.unpriced_models.join(", ");
        eprintln!(
            "warning: {} turn(s) had no pricing for model(s): {} — their cost is reported as $0.",
            report.unpriced_turns, models,
        );
        eprintln!(
            "         Update the snapshot (pnpm run pricing:update) or add an override at $RELAYBURN_HOME/models.dev.json.",
        );
    }
}

pub(super) fn render_quality(q: &QualityResult) -> String {
    if q.outcomes.is_empty() {
        return "quality: (no sessions)".to_string();
    }
    let mut completed = 0u64;
    let mut abandoned = 0u64;
    let mut errored = 0u64;
    let mut unknown = 0u64;
    for outcome in &q.outcomes {
        match outcome.outcome {
            OutcomeLabel::Completed => completed += 1,
            OutcomeLabel::Abandoned => abandoned += 1,
            OutcomeLabel::Errored => errored += 1,
            OutcomeLabel::Unknown => unknown += 1,
        }
    }
    let mut edit_turns = 0u64;
    let mut one_shot_turns = 0u64;
    for metric in &q.one_shot {
        edit_turns += metric.edit_turns;
        one_shot_turns += metric.one_shot_turns;
    }
    let one_shot_line = if edit_turns == 0 {
        "  one-shot rate: n/a (no edit turns)".to_string()
    } else {
        format!(
            "  one-shot rate: {:.1}% across {} edit turns",
            (one_shot_turns as f64 / edit_turns as f64) * 100.0,
            format_uint(edit_turns),
        )
    };
    [
        format!(
            "quality — sessions: {}",
            format_uint(q.outcomes.len() as u64)
        ),
        format!(
            "  outcomes: {} completed / {} abandoned / {} errored / {} unknown",
            format_uint(completed),
            format_uint(abandoned),
            format_uint(errored),
            format_uint(unknown),
        ),
        one_shot_line,
    ]
    .join("\n")
}

/// Human-readable outcome line for `burn summary`, e.g.
/// `Turn outcomes: 142 end_turn, 3 max_tokens, 1 refusal, 0 pause`.
///
/// Always renders `end_turn` / `max_tokens` / `refusal` / `pause` because
/// users want to see the zero — "no refusals" is a meaningful signal.
/// Other buckets (`tool_use`, `stop_sequence`, `silent`, `none`) appear
/// only when non-zero so the line stays scannable. Labels stay snake_case
/// to match the historical Anthropic spelling the issue specified.
pub(super) fn format_stop_reasons_line(s: &StopReasonCounts) -> String {
    let mut parts: Vec<String> = vec![
        format!("{} end_turn", format_uint(s.end_turn)),
        format!("{} max_tokens", format_uint(s.max_tokens)),
        format!("{} refusal", format_uint(s.refusal)),
        format!("{} pause", format_uint(s.pause_turn)),
    ];
    if s.tool_use > 0 {
        parts.push(format!("{} tool_use", format_uint(s.tool_use)));
    }
    if s.stop_sequence > 0 {
        parts.push(format!("{} stop_sequence", format_uint(s.stop_sequence)));
    }
    if s.silent > 0 {
        parts.push(format!("{} silent", format_uint(s.silent)));
    }
    if s.none > 0 {
        parts.push(format!("{} none", format_uint(s.none)));
    }
    format!("Turn outcomes: {}", parts.join(", "))
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

pub(super) fn format_replacement_savings_line(
    s: &relayburn_sdk::ReplacementSavingsSummary,
) -> String {
    let call_word = if s.calls == 1 { "call" } else { "calls" };
    format!(
        "estimated savings from replacement tools: ~{} tokens across {} {} ({} collapsed vanilla calls)",
        format_uint(s.estimated_tokens_saved),
        format_uint(s.calls),
        call_word,
        format_uint(s.collapsed_calls),
    )
}

/// Footer note explaining the `*` marker. Numerator is the token field with
/// the largest missing count: for each coverage field, sum its `missing`
/// across every row, then take the max. Denominator is the cross-row sum of
/// `known + missing` for `input` (the canonical token field; if a record has
/// any per-turn coverage at all, it carries input).
pub(super) fn format_partial_footer(rows: &[UsageCostAggregateRow]) -> String {
    let mut total: u64 = 0;
    for r in rows {
        total += r.coverage.input.known + r.coverage.input.missing;
    }
    let mut missing: u64 = 0;
    let mut fields: Vec<&'static str> = Vec::new();
    for f in COVERAGE_FIELDS {
        let mut field_missing: u64 = 0;
        for r in rows {
            field_missing += r.coverage.field(f).missing;
        }
        match field_missing.cmp(&missing) {
            std::cmp::Ordering::Greater => {
                missing = field_missing;
                fields.clear();
                fields.push(coverage_field_label(f));
            }
            std::cmp::Ordering::Equal if field_missing > 0 => {
                fields.push(coverage_field_label(f));
            }
            _ => {}
        }
    }
    let field = fields.join("/");
    format!(
        "{} partial token coverage: largest gap is {} (missing on {} of {} turns); totals still include all turns",
        PARTIAL_MARK,
        field,
        format_uint(missing),
        format_uint(total),
    )
}

pub(super) fn render_fidelity_notice(f: &FidelitySummary) -> Option<String> {
    let usage_only = *f.by_class.get(&FidelityClass::UsageOnly).unwrap_or(&0);
    let aggregate_only = *f.by_class.get(&FidelityClass::AggregateOnly).unwrap_or(&0);
    let cost_only = *f.by_class.get(&FidelityClass::CostOnly).unwrap_or(&0);
    let partial = *f.by_class.get(&FidelityClass::Partial).unwrap_or(&0);
    let full = *f.by_class.get(&FidelityClass::Full).unwrap_or(&0);
    let non_full = usage_only + aggregate_only + cost_only + partial;
    if non_full == 0 && f.unknown == 0 {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if full > 0 {
        parts.push(format!("{} full", full));
    }
    if usage_only > 0 {
        parts.push(format!("{} usage-only", usage_only));
    }
    if aggregate_only > 0 {
        parts.push(format!("{} aggregate-only", aggregate_only));
    }
    if cost_only > 0 {
        parts.push(format!("{} cost-only", cost_only));
    }
    if partial > 0 {
        parts.push(format!("{} partial", partial));
    }
    if f.unknown > 0 {
        parts.push(format!("{} unknown", f.unknown));
    }
    Some(format!(
        "fidelity: {} (use --json for per-field coverage)",
        parts.join(" / ")
    ))
}
