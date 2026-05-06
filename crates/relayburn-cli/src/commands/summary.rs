//! `burn summary` — aggregate session usage and cost.
//!
//! Thin presenter over the `relayburn_sdk` query helpers. Mirrors the TS
//! `packages/cli/src/commands/summary.ts` byte-for-byte for the default
//! (group-by-model) flow that drives the golden snapshots; the wider TS
//! surface (`--by-provider`, `--by-tool`, `--by-subagent-type`,
//! `--by-relationship`, `--subagent-tree`, `--quality`) is enumerated as
//! flag wiring + a stub-mode error path so behavior gaps surface as a
//! human message rather than silently dropping the flag.
//!
//! ## Wiring
//!
//! 1. Open a [`relayburn_sdk::LedgerHandle`] honoring `--ledger-path` /
//!    `RELAYBURN_HOME`.
//! 2. Run [`relayburn_sdk::ingest_all`] against the same handle. The TS CLI
//!    does this unconditionally so the summary block in human mode always
//!    leads with `ingested N new sessions (+M turns)`. Output is captured
//!    by reference (no progress spinner — that's a TTY-only concern that
//!    breaks golden output).
//! 3. Pull turns with [`relayburn_sdk::Query`] filters lowered from CLI
//!    flags (`--since`, `--project`, `--session`).
//! 4. Aggregate into per-model rows (or per-provider with `--by-provider`),
//!    derive a slice-wide `CostBreakdown` via
//!    [`relayburn_sdk::sum_costs`], and capture coverage / fidelity via
//!    [`relayburn_sdk::summarize_fidelity`] +
//!    [`relayburn_sdk::summarize_replacement_savings`].
//! 5. Render JSON or human format.

use clap::Args;
use indexmap::IndexMap;
use relayburn_sdk::{
    aggregate_by_provider, cost_for_turn, ingest_all, load_pricing, normalize_since,
    summarize_fidelity, summarize_replacement_savings, sum_costs, AggregateByProviderOptions,
    CostBreakdown, Coverage, CoverageField, FidelityClass, FidelitySummary, Ledger,
    LedgerHandle, LedgerOpenOptions, ProviderAggregateRow, Query, RowCoverage, TurnRecord,
    UsageCostAggregateRow,
};
use serde_json::{json, Map, Value};

use crate::cli::GlobalArgs;
use crate::render::error::report_error;
use crate::render::format::{coerce_whole_f64_to_int, format_uint, format_usd, render_table};

/// Per-command flags for `burn summary`. Mirrors the TS surface in
/// `packages/cli/src/commands/summary.ts` so a TS user can carry their
/// muscle memory across.
#[derive(Debug, Clone, Args)]
pub struct SummaryArgs {
    /// Slice the ledger to events at or after `<since>`. Accepts either an
    /// ISO timestamp or a relative range (`24h`, `7d`, `4w`, `2m`).
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// Restrict to a single project (matches `project` or `projectKey`).
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<String>,

    /// Restrict to a single session id.
    #[arg(long, value_name = "SESSION_ID")]
    pub session: Option<String>,

    /// Group by effective provider instead of model.
    #[arg(long = "by-provider")]
    pub by_provider: bool,

    /// Group by tool, attributing each turn's ingest cost to the prior
    /// turn's `tool_use` blocks. Emits a `byTool` table; mutually
    /// exclusive with `--by-provider` / the subagent flags.
    #[arg(long = "by-tool")]
    pub by_tool: bool,

    /// Bucket by `subagent.subagentType`. Mutually exclusive with the
    /// other group-by flags.
    #[arg(long = "by-subagent-type")]
    pub by_subagent_type: bool,

    /// Bucket by `SessionRelationshipRecord.relationshipType` (or pass
    /// `subagent` to drill into the subagent leaf). Mutually exclusive
    /// with `--subagent-tree`.
    #[arg(long = "by-relationship", value_name = "MODE", num_args = 0..=1, default_missing_value = "")]
    pub by_relationship: Option<String>,

    /// Render the subagent spawn tree for a session id. Passing the flag
    /// without a value uses `--session`.
    #[arg(long = "subagent-tree", value_name = "SESSION_ID", num_args = 0..=1, default_missing_value = "")]
    pub subagent_tree: Option<String>,

    /// Restrict the subagent tree / relationship views to a single agent
    /// id.
    #[arg(long, value_name = "AGENT_ID")]
    pub agent: Option<String>,

    /// Filter by enrichment workflow id.
    #[arg(long, value_name = "WORKFLOW_ID")]
    pub workflow: Option<String>,

    /// Provider filter (CSV of provider names; case-insensitive).
    #[arg(long, value_name = "PROVIDERS")]
    pub provider: Option<String>,

    /// Append a quality summary (one-shot rate, completion outcomes).
    #[arg(long)]
    pub quality: bool,

    /// Bypass the archive sidecar and stream the ledger. Kept for parity
    /// with the TS CLI and as an escape hatch for archive-corruption
    /// debugging.
    #[arg(long = "no-archive")]
    pub no_archive: bool,
}

pub fn run(globals: &GlobalArgs, args: SummaryArgs) -> i32 {
    match run_inner(globals, args) {
        Ok(code) => code,
        Err(err) => report_error(&err, globals),
    }
}

fn run_inner(globals: &GlobalArgs, args: SummaryArgs) -> anyhow::Result<i32> {
    // Mode exclusivity — mirror the TS CLI's stderr+exit2 contract so a
    // mis-typed combination of flags produces a clear message rather than
    // silently dropping one.
    if args.by_tool
        && (args.by_provider
            || args.by_subagent_type
            || args.by_relationship.is_some()
            || args.subagent_tree.is_some())
    {
        eprintln!(
            "burn: --by-tool cannot be combined with --by-provider/--by-subagent-type/--by-relationship/--subagent-tree"
        );
        return Ok(2);
    }
    if args.by_provider
        && (args.by_subagent_type
            || args.by_relationship.is_some()
            || args.subagent_tree.is_some())
    {
        eprintln!(
            "burn: --by-provider cannot be combined with --by-subagent-type/--by-relationship/--subagent-tree"
        );
        return Ok(2);
    }
    if args.by_subagent_type && (args.by_relationship.is_some() || args.subagent_tree.is_some()) {
        eprintln!(
            "burn: --by-subagent-type cannot be combined with --by-relationship/--subagent-tree"
        );
        return Ok(2);
    }
    if args.by_relationship.is_some() && args.subagent_tree.is_some() {
        eprintln!("burn: --by-relationship cannot be combined with --subagent-tree");
        return Ok(2);
    }
    if let Some(rel) = &args.by_relationship {
        if !rel.is_empty() && rel != "subagent" {
            eprintln!(
                "burn: --by-relationship accepts only the optional value \"subagent\""
            );
            return Ok(2);
        }
    }

    // The TS CLI exits 2 with a clear message for surfaces that haven't
    // been ported yet (`--by-tool`, `--by-subagent-type`,
    // `--by-relationship`, `--subagent-tree`, `--quality`, `--agent`,
    // `--workflow`, `--provider`). The Wave 2 D1 contract is the default
    // group-by-model + `--by-provider` flow; the rest land in follow-ups.
    if args.by_tool {
        eprintln!(
            "burn: --by-tool is not yet implemented in the Rust port (#248 D1 covers default + --by-provider; follow-ups will add by-tool / subagent / relationship / quality)"
        );
        return Ok(2);
    }
    if args.by_subagent_type
        || args.by_relationship.is_some()
        || args.subagent_tree.is_some()
        || args.agent.is_some()
        || args.quality
    {
        eprintln!(
            "burn: subagent / relationship / quality summary modes are not yet implemented in the Rust port"
        );
        return Ok(2);
    }
    if args.workflow.is_some() {
        eprintln!("burn: --workflow filter is not yet implemented in the Rust port");
        return Ok(2);
    }
    if args.provider.is_some() {
        eprintln!("burn: --provider filter is not yet implemented in the Rust port");
        return Ok(2);
    }

    let ledger_home = globals.ledger_path.clone();
    let opts = match &ledger_home {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    let mut handle = Ledger::open(opts)?;

    let ingest_report = run_ingest(&mut handle, ledger_home.as_deref())?;

    let q = build_query(&args)?;
    let turns: Vec<TurnRecord> = handle
        .raw()
        .query_turns(&q)?
        .into_iter()
        .map(|e| e.turn)
        .collect();

    let pricing = load_pricing(None);
    let fidelity = summarize_fidelity(&turns);
    let savings = summarize_replacement_savings(&turns, None);

    if args.by_provider {
        let rows = aggregate_by_provider(&turns, AggregateByProviderOptions::new(&pricing));
        let provider_rows: Vec<UsageCostAggregateRow> = rows
            .into_iter()
            .map(provider_to_aggregate_row)
            .collect();
        emit_grouped(
            globals,
            true,
            &provider_rows,
            &turns,
            &ingest_report,
            &pricing,
            &fidelity,
            &savings,
        );
    } else {
        let rows = aggregate_by_model(&turns, &pricing);
        emit_grouped(
            globals,
            false,
            &rows,
            &turns,
            &ingest_report,
            &pricing,
            &fidelity,
            &savings,
        );
    }
    Ok(0)
}

fn build_query(args: &SummaryArgs) -> anyhow::Result<Query> {
    let mut q = Query::default();
    if let Some(s) = &args.session {
        q.session_id = Some(s.clone());
    }
    if let Some(p) = &args.project {
        q.project = Some(p.clone());
    }
    if let Some(since) = normalize_since(args.since.as_deref())? {
        q.since = Some(since);
    }
    Ok(q)
}

/// Run an ingest sweep on the open handle. Builds a current-thread tokio
/// runtime so the otherwise-sync presenter can drive the async verb.
fn run_ingest(
    handle: &mut LedgerHandle,
    _ledger_home: Option<&std::path::Path>,
) -> anyhow::Result<relayburn_sdk::IngestReport> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let opts = relayburn_sdk::RawIngestOptions::default();
    rt.block_on(ingest_all(handle.raw_mut(), &opts))
}

fn aggregate_by_model(
    turns: &[TurnRecord],
    pricing: &relayburn_sdk::PricingTable,
) -> Vec<UsageCostAggregateRow> {
    // First-seen iteration order matches TS `Map` semantics; the final
    // stable sort by descending cost keeps cross-language tie-breaks
    // consistent with the TS implementation.
    let mut by_model: IndexMap<String, UsageCostAggregateRow> = IndexMap::new();
    for t in turns {
        let key = if t.model.is_empty() {
            "unknown".to_string()
        } else {
            t.model.clone()
        };
        let row = by_model
            .entry(key.clone())
            .or_insert_with(|| empty_row(&key));
        row.turns += 1;
        row.usage.input += t.usage.input;
        row.usage.output += t.usage.output;
        row.usage.reasoning += t.usage.reasoning;
        row.usage.cache_read += t.usage.cache_read;
        row.usage.cache_create_5m += t.usage.cache_create_5m;
        row.usage.cache_create_1h += t.usage.cache_create_1h;
        accumulate_coverage(&mut row.coverage, t.fidelity.as_ref().map(|f| &f.coverage));
        if let Some(c) = cost_for_turn(t, pricing) {
            row.cost.total += c.total;
            row.cost.input += c.input;
            row.cost.output += c.output;
            row.cost.reasoning += c.reasoning;
            row.cost.cache_read += c.cache_read;
            row.cost.cache_create += c.cache_create;
        }
    }
    let mut rows: Vec<UsageCostAggregateRow> = by_model.into_values().collect();
    rows.sort_by(|a, b| {
        b.cost
            .total
            .partial_cmp(&a.cost.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

fn provider_to_aggregate_row(p: ProviderAggregateRow) -> UsageCostAggregateRow {
    UsageCostAggregateRow {
        label: p.label,
        turns: p.turns,
        usage: p.usage,
        cost: p.cost,
        coverage: p.coverage,
    }
}

fn empty_row(label: &str) -> UsageCostAggregateRow {
    UsageCostAggregateRow {
        label: label.to_string(),
        turns: 0,
        usage: relayburn_sdk::Usage::default(),
        cost: CostBreakdown {
            model: label.to_string(),
            total: 0.0,
            input: 0.0,
            output: 0.0,
            reasoning: 0.0,
            cache_read: 0.0,
            cache_create: 0.0,
        },
        coverage: RowCoverage::default(),
    }
}

const COVERAGE_FIELDS: [CoverageField; 5] = [
    CoverageField::Input,
    CoverageField::Output,
    CoverageField::Reasoning,
    CoverageField::CacheRead,
    CoverageField::CacheCreate,
];

fn accumulate_coverage(target: &mut RowCoverage, coverage: Option<&Coverage>) {
    for f in COVERAGE_FIELDS {
        let known = match coverage {
            None => true,
            Some(c) => match f {
                CoverageField::Input => c.has_input_tokens,
                CoverageField::Output => c.has_output_tokens,
                CoverageField::Reasoning => c.has_reasoning_tokens,
                CoverageField::CacheRead => c.has_cache_read_tokens,
                CoverageField::CacheCreate => c.has_cache_create_tokens,
            },
        };
        let slot = target.field_mut(f);
        if known {
            slot.known += 1;
        } else {
            slot.missing += 1;
        }
    }
}

fn cell_is_partial(c: &relayburn_sdk::FieldCoverage) -> bool {
    c.known > 0 && c.missing > 0
}

const PARTIAL_MARK: &str = "*";
const DASH: &str = "—";

/// Render one token-field cell. Three cases:
///   - every contributing turn reported the field → numeric value, no marker
///   - some turns reported, some didn't           → numeric value + `*`
///   - no turn reported                           → `—` (never `0`, which
///     would falsely claim a real zero from the source)
fn coverage_cell(value: u64, c: &relayburn_sdk::FieldCoverage) -> String {
    if c.known == 0 && c.missing > 0 {
        return DASH.to_string();
    }
    if c.known > 0 && c.missing > 0 {
        return format!("{}{}", format_uint(value), PARTIAL_MARK);
    }
    format_uint(value)
}

#[allow(clippy::too_many_arguments)]
fn emit_grouped(
    globals: &GlobalArgs,
    by_provider: bool,
    rows: &[UsageCostAggregateRow],
    turns: &[TurnRecord],
    ingest_report: &relayburn_sdk::IngestReport,
    _pricing: &relayburn_sdk::PricingTable,
    fidelity: &FidelitySummary,
    savings: &relayburn_sdk::ReplacementSavingsSummary,
) {
    let total_cost = sum_costs(rows.iter().map(|r| &r.cost));

    if globals.json {
        emit_json(
            by_provider,
            rows,
            turns,
            ingest_report,
            &total_cost,
            fidelity,
            savings,
        );
        return;
    }
    emit_human(by_provider, rows, ingest_report, &total_cost, fidelity, savings);
}

fn emit_json(
    by_provider: bool,
    rows: &[UsageCostAggregateRow],
    turns: &[TurnRecord],
    ingest_report: &relayburn_sdk::IngestReport,
    total_cost: &CostBreakdown,
    fidelity: &FidelitySummary,
    savings: &relayburn_sdk::ReplacementSavingsSummary,
) {
    let key = if by_provider { "byProvider" } else { "byModel" };
    let label_key = if by_provider { "provider" } else { "model" };

    let group_rows: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                label_key: r.label,
                "turns": r.turns,
                "usage": {
                    "input": r.usage.input,
                    "output": r.usage.output,
                    "reasoning": r.usage.reasoning,
                    "cacheRead": r.usage.cache_read,
                    "cacheCreate5m": r.usage.cache_create_5m,
                    "cacheCreate1h": r.usage.cache_create_1h,
                },
                "cost": cost_breakdown_to_json(&r.cost),
            })
        })
        .collect();

    let per_cell = build_per_cell_fidelity(rows, by_provider);

    let mut payload = Map::new();
    payload.insert(
        "ingest".into(),
        json!({
            "ingestedSessions": ingest_report.ingested_sessions,
            "appendedTurns": ingest_report.appended_turns,
        }),
    );
    payload.insert("turns".into(), json!(turns.len()));
    payload.insert("totalCost".into(), cost_breakdown_to_json(total_cost));
    payload.insert(key.into(), Value::Array(group_rows));
    payload.insert(
        "fidelity".into(),
        json!({
            "summary": fidelity_summary_to_json(fidelity),
            "perCell": per_cell,
        }),
    );
    if savings.calls > 0 {
        payload.insert(
            "replacementSavings".into(),
            replacement_savings_to_json(savings),
        );
    }

    let mut value = Value::Object(payload);
    coerce_whole_f64_to_int(&mut value);
    print_json(&value);
}

fn print_json(value: &Value) {
    let mut out = serde_json::to_string_pretty(value).unwrap_or_default();
    out.push('\n');
    print!("{}", out);
}

fn cost_breakdown_to_json(c: &CostBreakdown) -> Value {
    json!({
        "model": c.model,
        "total": c.total,
        "input": c.input,
        "output": c.output,
        "reasoning": c.reasoning,
        "cacheRead": c.cache_read,
        "cacheCreate": c.cache_create,
    })
}

/// Emit the FidelitySummary in TS-CLI key order so JSON output is
/// byte-equivalent to the snapshot. The SDK exposes the summary via
/// `HashMap` so iteration order is non-deterministic; we materialize each
/// section in a hand-ordered `serde_json::Map`.
fn fidelity_summary_to_json(s: &FidelitySummary) -> Value {
    let mut by_class = Map::new();
    for class in [
        FidelityClass::Full,
        FidelityClass::UsageOnly,
        FidelityClass::AggregateOnly,
        FidelityClass::CostOnly,
        FidelityClass::Partial,
    ] {
        by_class.insert(
            fidelity_class_key(class).to_string(),
            json!(*s.by_class.get(&class).unwrap_or(&0)),
        );
    }

    let mut by_granularity = Map::new();
    for g in [
        relayburn_sdk::UsageGranularity::PerTurn,
        relayburn_sdk::UsageGranularity::PerMessage,
        relayburn_sdk::UsageGranularity::PerSessionAggregate,
        relayburn_sdk::UsageGranularity::CostOnly,
    ] {
        by_granularity.insert(
            granularity_key(g).to_string(),
            json!(*s.by_granularity.get(&g).unwrap_or(&0)),
        );
    }

    let mut missing = Map::new();
    for field in [
        "hasInputTokens",
        "hasOutputTokens",
        "hasReasoningTokens",
        "hasCacheReadTokens",
        "hasCacheCreateTokens",
        "hasToolCalls",
        "hasToolResultEvents",
        "hasSessionRelationships",
        "hasRawContent",
    ] {
        missing.insert(
            field.to_string(),
            json!(*s.missing_coverage.get(field).unwrap_or(&0)),
        );
    }

    let mut out = Map::new();
    out.insert("total".into(), json!(s.total));
    out.insert("byClass".into(), Value::Object(by_class));
    out.insert("byGranularity".into(), Value::Object(by_granularity));
    out.insert("missingCoverage".into(), Value::Object(missing));
    out.insert("unknown".into(), json!(s.unknown));
    Value::Object(out)
}

fn fidelity_class_key(c: FidelityClass) -> &'static str {
    match c {
        FidelityClass::Full => "full",
        FidelityClass::UsageOnly => "usage-only",
        FidelityClass::AggregateOnly => "aggregate-only",
        FidelityClass::CostOnly => "cost-only",
        FidelityClass::Partial => "partial",
    }
}

fn granularity_key(g: relayburn_sdk::UsageGranularity) -> &'static str {
    match g {
        relayburn_sdk::UsageGranularity::PerTurn => "per-turn",
        relayburn_sdk::UsageGranularity::PerMessage => "per-message",
        relayburn_sdk::UsageGranularity::PerSessionAggregate => "per-session-aggregate",
        relayburn_sdk::UsageGranularity::CostOnly => "cost-only",
    }
}

fn build_per_cell_fidelity(rows: &[UsageCostAggregateRow], by_provider: bool) -> Value {
    let cells: Vec<Value> = rows
        .iter()
        .map(|r| {
            let cache_create = &r.coverage.cache_create;
            let fields = [
                ("input", &r.coverage.input),
                ("output", &r.coverage.output),
                ("reasoning", &r.coverage.reasoning),
                ("cacheRead", &r.coverage.cache_read),
                ("cacheCreate", cache_create),
            ];
            let mut fields_map = Map::new();
            let mut partial = false;
            for (name, c) in fields {
                if cell_is_partial(c) || (c.known == 0 && c.missing > 0) {
                    partial = true;
                }
                fields_map.insert(
                    name.to_string(),
                    json!({
                        "known": c.known,
                        "missing": c.missing,
                    }),
                );
            }
            json!({
                "label": r.label,
                "partial": partial,
                "fields": Value::Object(fields_map),
            })
        })
        .collect();
    json!({
        "groupBy": if by_provider { "provider" } else { "model" },
        "cells": cells,
    })
}

fn replacement_savings_to_json(savings: &relayburn_sdk::ReplacementSavingsSummary) -> Value {
    let mut by_tool: Vec<Value> = savings
        .by_tool
        .iter()
        .map(|(name, agg)| {
            json!({
                "tool": name,
                "calls": agg.calls,
                "collapsedCalls": agg.collapsed_calls,
                "estimatedTokensSaved": agg.estimated_tokens_saved,
            })
        })
        .collect();
    by_tool.sort_by(|a, b| {
        let av = a.get("estimatedTokensSaved").and_then(Value::as_u64).unwrap_or(0);
        let bv = b.get("estimatedTokensSaved").and_then(Value::as_u64).unwrap_or(0);
        bv.cmp(&av)
    });
    json!({
        "calls": savings.calls,
        "collapsedCalls": savings.collapsed_calls,
        "estimatedTokensSaved": savings.estimated_tokens_saved,
        "byTool": by_tool,
    })
}

fn emit_human(
    by_provider: bool,
    rows: &[UsageCostAggregateRow],
    ingest_report: &relayburn_sdk::IngestReport,
    total_cost: &CostBreakdown,
    fidelity: &FidelitySummary,
    savings: &relayburn_sdk::ReplacementSavingsSummary,
) {
    let mut lines: Vec<String> = Vec::new();
    lines.push(String::new());
    lines.push(format!(
        "ingested {} new session{} (+{} turns)",
        ingest_report.ingested_sessions,
        if ingest_report.ingested_sessions == 1 {
            ""
        } else {
            "s"
        },
        format_uint(ingest_report.appended_turns as u64),
    ));
    lines.push(String::new());

    let total_turns: u64 = rows.iter().map(|r| r.turns).sum();
    lines.push(format!("turns analyzed: {}", format_uint(total_turns)));
    lines.push(String::new());

    if rows.is_empty() {
        lines.push("no turns match the current filters.".to_string());
        let mut out = lines.join("\n");
        out.push('\n');
        print!("{}", out);
        return;
    }

    let header_label = if by_provider { "provider" } else { "model" };
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
    for r in rows {
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
    lines.push(format!("total cost: {}", format_usd(total_cost.total)));
    lines.push(format!(
        "  input {} / output {} / reasoning {} / cacheRead {} / cacheCreate {}",
        format_usd(total_cost.input),
        format_usd(total_cost.output),
        format_usd(total_cost.reasoning),
        format_usd(total_cost.cache_read),
        format_usd(total_cost.cache_create),
    ));
    lines.push(String::new());

    if savings.calls > 0 {
        lines.push(format_replacement_savings_line(savings));
        lines.push(String::new());
    }

    if any_partial {
        lines.push(format_partial_footer(rows));
        lines.push(String::new());
    }

    if let Some(notice) = render_fidelity_notice(fidelity) {
        lines.push(notice);
        lines.push(String::new());
    }

    let out = lines.join("\n");
    // TS uses `process.stdout.write(lines.join('\n'))` — no trailing newline.
    print!("{}", out);
}

fn format_replacement_savings_line(s: &relayburn_sdk::ReplacementSavingsSummary) -> String {
    let call_word = if s.calls == 1 { "call" } else { "calls" };
    format!(
        "estimated savings from replacement tools: ~{} tokens across {} {} ({} collapsed vanilla calls)",
        format_uint(s.estimated_tokens_saved),
        format_uint(s.calls),
        call_word,
        format_uint(s.collapsed_calls),
    )
}

/// Footer note explaining the `*` marker. Numerator is the worst-covered
/// axis: for each coverage field, sum its `missing` across every row, then
/// take the max. Denominator is the cross-row sum of `known + missing` for
/// `input` (the canonical token field; if a record has any per-turn
/// coverage at all, it carries input).
fn format_partial_footer(rows: &[UsageCostAggregateRow]) -> String {
    let mut total: u64 = 0;
    for r in rows {
        total += r.coverage.input.known + r.coverage.input.missing;
    }
    let mut missing: u64 = 0;
    for f in COVERAGE_FIELDS {
        let mut field_missing: u64 = 0;
        for r in rows {
            field_missing += r.coverage.field(f).missing;
        }
        if field_missing > missing {
            missing = field_missing;
        }
    }
    format!(
        "{} partial coverage: {} of {} turns omitted per-turn token data",
        PARTIAL_MARK,
        format_uint(missing),
        format_uint(total),
    )
}

fn render_fidelity_notice(f: &FidelitySummary) -> Option<String> {
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

