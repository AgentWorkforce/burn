//! `burn summary` — aggregate session usage and cost.
//!
//! Thin presenter over the `relayburn_sdk` query helpers. Mirrors the TS
//! `packages/cli/src/commands/summary.ts` surface: grouped model/provider
//! summaries, tool attribution, subagent views, relationship rollups, workflow
//! / agent / provider filters, and the optional quality footer.
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
//! 3. Lower CLI flags into [`relayburn_sdk::SummaryReportOptions`] and call
//!    the SDK-owned `summary_report` verb.
//! 4. Render the typed report as JSON or human output.

use std::collections::{BTreeMap, BTreeSet};

use clap::Args;
use relayburn_sdk::{
    ingest_all, summary_fidelity_summary_to_value, summary_replacement_savings_to_value,
    CostBreakdown, CoverageField, Enrichment, FidelityClass, FidelitySummary, Ledger, LedgerHandle,
    LedgerOpenOptions, OutcomeLabel, QualityResult, RelationshipType, SubagentTreeNode,
    SubagentTypeStats, SummaryByToolReport, SummaryGroupBy, SummaryGroupedReport,
    SummaryRelationshipReport, SummaryReport, SummaryReportMode, SummaryReportOptions,
    SummarySubagentTreeReport, UsageCostAggregateRow,
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

    /// Filter by folded enrichment tag. Repeatable; every tag must match.
    #[arg(long = "tag", value_name = "K=V")]
    pub tag: Vec<String>,

    /// Group totals by a folded enrichment tag value.
    #[arg(long = "group-by-tag", value_name = "KEY")]
    pub group_by_tag: Option<String>,

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
            || args.subagent_tree.is_some()
            || args.group_by_tag.is_some())
    {
        eprintln!(
            "burn: --by-tool cannot be combined with --by-provider/--by-subagent-type/--by-relationship/--subagent-tree/--group-by-tag"
        );
        return Ok(2);
    }
    if args.by_provider
        && (args.by_subagent_type
            || args.by_relationship.is_some()
            || args.subagent_tree.is_some()
            || args.group_by_tag.is_some())
    {
        eprintln!(
            "burn: --by-provider cannot be combined with --by-subagent-type/--by-relationship/--subagent-tree/--group-by-tag"
        );
        return Ok(2);
    }
    if args.by_subagent_type
        && (args.by_relationship.is_some()
            || args.subagent_tree.is_some()
            || args.group_by_tag.is_some())
    {
        eprintln!(
            "burn: --by-subagent-type cannot be combined with --by-relationship/--subagent-tree/--group-by-tag"
        );
        return Ok(2);
    }
    if args.by_relationship.is_some()
        && (args.subagent_tree.is_some() || args.group_by_tag.is_some())
    {
        eprintln!("burn: --by-relationship cannot be combined with --subagent-tree/--group-by-tag");
        return Ok(2);
    }
    if args.subagent_tree.is_some() && args.group_by_tag.is_some() {
        eprintln!("burn: --subagent-tree cannot be combined with --group-by-tag");
        return Ok(2);
    }
    if let Some(rel) = &args.by_relationship {
        if !rel.is_empty() && rel != "subagent" {
            eprintln!("burn: --by-relationship accepts only the optional value \"subagent\"");
            return Ok(2);
        }
    }
    if let Some(tag_key) = args.group_by_tag.as_deref() {
        if tag_key.is_empty() {
            eprintln!("burn: --group-by-tag requires a non-empty key");
            return Ok(2);
        }
    }

    let provider_filter = match parse_provider_filter(args.provider.as_deref()) {
        Ok(filter) => filter,
        Err(msg) => {
            eprintln!("{msg}");
            return Ok(2);
        }
    };
    let tag_filter: Enrichment = match parse_tag_filters(&args.tag) {
        Ok(filter) => filter,
        Err(err) => {
            eprintln!("{err}");
            return Ok(2);
        }
    };
    let subagent_tree_session_id = if let Some(tree_flag) = args.subagent_tree.as_deref() {
        if tree_flag.is_empty() && args.session.is_none() {
            eprintln!("burn: --subagent-tree requires a session id (positional or --session)");
            return Ok(2);
        }
        Some(if tree_flag.is_empty() {
            None
        } else {
            Some(tree_flag.to_string())
        })
    } else {
        None
    };

    let _archive_guard = ArchiveOverride::activate(args.no_archive);

    let opts = match globals.ledger_path.as_deref() {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    let mut handle = Ledger::open(opts)?;

    let ingest_report = run_ingest(&mut handle)?;

    let mode = if let Some(session_id) = subagent_tree_session_id {
        SummaryReportMode::SubagentTree { session_id }
    } else if args.by_tool {
        SummaryReportMode::ByTool
    } else if args.by_subagent_type {
        SummaryReportMode::BySubagentType
    } else if let Some(rel_flag) = args.by_relationship.as_deref() {
        SummaryReportMode::ByRelationship {
            subagent: rel_flag == "subagent",
        }
    } else {
        SummaryReportMode::Grouped {
            by_provider: args.by_provider,
        }
    };

    let report = handle.summary_report(SummaryReportOptions {
        session: args.session,
        project: args.project,
        since: args.since,
        workflow: args.workflow,
        tags: if tag_filter.is_empty() {
            None
        } else {
            Some(tag_filter)
        },
        group_by_tag: args.group_by_tag,
        agent: args.agent,
        providers: provider_filter.map(|providers| providers.into_iter().collect()),
        mode,
        include_quality: args.quality,
        ledger_home: None,
    })?;

    match report {
        SummaryReport::Grouped(report) => {
            emit_grouped(globals, &report, &ingest_report);
        }
        SummaryReport::ByTool(report) => {
            emit_ingest_prelude(globals, &ingest_report);
            return render_by_tool_report(globals, &report, &ingest_report);
        }
        SummaryReport::BySubagentType(report) => {
            emit_ingest_prelude(globals, &ingest_report);
            return render_subagent_type_report(globals, &report.stats);
        }
        SummaryReport::Relationship(report) => {
            emit_ingest_prelude(globals, &ingest_report);
            return render_relationship_report(globals, &report);
        }
        SummaryReport::SubagentTree(report) => {
            emit_ingest_prelude(globals, &ingest_report);
            return render_subagent_tree_report(globals, &report);
        }
    }
    Ok(0)
}

fn parse_provider_filter(raw: Option<&str>) -> Result<Option<BTreeSet<String>>, &'static str> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let providers: BTreeSet<String> = raw
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if providers.is_empty() {
        return Err("burn: --provider requires a value");
    }
    Ok(Some(providers))
}

fn parse_tag_filters(tags: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for raw in tags {
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("burn: --tag expects k=v, got \"{raw}\""))?;
        if key.is_empty() {
            anyhow::bail!("burn: --tag key must be non-empty (got \"{raw}\")");
        }
        if let Some(existing) = out.get(key) {
            anyhow::bail!(
                "burn: duplicate --tag filter for key \"{key}\" ({existing:?} vs {value:?})"
            );
        }
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

/// Run an ingest sweep on the open handle. Builds a current-thread tokio
/// runtime so the otherwise-sync presenter can drive the async verb.
fn run_ingest(handle: &mut LedgerHandle) -> anyhow::Result<relayburn_sdk::IngestReport> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let opts = relayburn_sdk::RawIngestOptions::default();
    rt.block_on(ingest_all(handle.raw_mut(), &opts))
}

/// Drop-in for `RELAYBURN_ARCHIVE=0`. The Rust SDK is already SQLite-native,
/// but this preserves the TS CLI flag contract for any lower layer that checks
/// the env escape hatch.
struct ArchiveOverride {
    previous: Option<String>,
    activated: bool,
}

impl ArchiveOverride {
    fn activate(no_archive: bool) -> Self {
        if !no_archive {
            return Self {
                previous: None,
                activated: false,
            };
        }
        let previous = std::env::var("RELAYBURN_ARCHIVE").ok();
        std::env::set_var("RELAYBURN_ARCHIVE", "0");
        Self {
            previous,
            activated: true,
        }
    }
}

impl Drop for ArchiveOverride {
    fn drop(&mut self) {
        if !self.activated {
            return;
        }
        match self.previous.take() {
            Some(v) => std::env::set_var("RELAYBURN_ARCHIVE", v),
            None => std::env::remove_var("RELAYBURN_ARCHIVE"),
        }
    }
}

const COVERAGE_FIELDS: [CoverageField; 5] = [
    CoverageField::Input,
    CoverageField::Output,
    CoverageField::Reasoning,
    CoverageField::CacheRead,
    CoverageField::CacheCreate,
];

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

fn emit_grouped(
    globals: &GlobalArgs,
    report: &SummaryGroupedReport,
    ingest_report: &relayburn_sdk::IngestReport,
) {
    if globals.json {
        emit_json(report, ingest_report);
        return;
    }
    emit_human(report, ingest_report);
}

fn emit_ingest_prelude(globals: &GlobalArgs, ingest_report: &relayburn_sdk::IngestReport) {
    if globals.json {
        return;
    }
    emit_human_ingest_prelude(ingest_report);
}

fn emit_human_ingest_prelude(ingest_report: &relayburn_sdk::IngestReport) {
    print!("{}", ingest_prelude_text(ingest_report));
}

fn ingest_prelude_text(ingest_report: &relayburn_sdk::IngestReport) -> String {
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

fn emit_json(report: &SummaryGroupedReport, ingest_report: &relayburn_sdk::IngestReport) {
    let value = grouped_json_value(report, ingest_report);
    print_json(&value);
}

fn grouped_json_value(
    report: &SummaryGroupedReport,
    ingest_report: &relayburn_sdk::IngestReport,
) -> Value {
    let key = report.group_by.json_key();
    let label_key = report.group_by.wire_str();

    let group_rows: Vec<Value> = report
        .rows
        .iter()
        .enumerate()
        .map(|(idx, r)| {
            let mut row = if report.group_by == SummaryGroupBy::Tag {
                json!({
                    "tag": report.tag_key.as_deref().unwrap_or(""),
                    "value": report.tag_values.get(idx).cloned().flatten(),
                })
            } else {
                json!({
                    label_key: r.label,
                })
            };
            let obj = row.as_object_mut().unwrap();
            obj.insert("turns".into(), json!(r.turns));
            obj.insert(
                "usage".into(),
                json!({
                    "input": r.usage.input,
                    "output": r.usage.output,
                    "reasoning": r.usage.reasoning,
                    "cacheRead": r.usage.cache_read,
                    "cacheCreate5m": r.usage.cache_create_5m,
                    "cacheCreate1h": r.usage.cache_create_1h,
                }),
            );
            obj.insert("cost".into(), cost_breakdown_to_json(&r.cost));
            row
        })
        .collect();

    let mut payload = Map::new();
    payload.insert(
        "ingest".into(),
        json!({
            "ingestedSessions": ingest_report.ingested_sessions,
            "appendedTurns": ingest_report.appended_turns,
        }),
    );
    payload.insert("turns".into(), json!(report.turn_count));
    payload.insert(
        "totalCost".into(),
        cost_breakdown_to_json(&report.total_cost),
    );
    payload.insert(key.into(), Value::Array(group_rows));
    payload.insert(
        "fidelity".into(),
        json!({
            "summary": summary_fidelity_summary_to_value(&report.fidelity),
            "perCell": report.per_cell_fidelity.clone(),
        }),
    );
    if report.replacement_savings.calls > 0 {
        payload.insert(
            "replacementSavings".into(),
            summary_replacement_savings_to_value(&report.replacement_savings),
        );
    }
    if let Some(quality) = report.quality.as_ref() {
        payload.insert("quality".into(), json!(quality));
    }

    let mut value = Value::Object(payload);
    coerce_whole_f64_to_int(&mut value);
    value
}

fn print_json(value: &Value) {
    let mut out = serde_json::to_string_pretty(value).unwrap_or_default();
    out.push('\n');
    print!("{}", out);
}

fn cost_breakdown_to_json(c: &CostBreakdown) -> Value {
    json!({
        "model": c.model.as_ref(),
        "total": c.total,
        "input": c.input,
        "output": c.output,
        "reasoning": c.reasoning,
        "cacheRead": c.cache_read,
        "cacheCreate": c.cache_create,
    })
}

fn render_by_tool_report(
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
        print_json(&payload);
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

fn render_subagent_type_report(
    globals: &GlobalArgs,
    stats: &[SubagentTypeStats],
) -> anyhow::Result<i32> {
    if globals.json {
        let mut value = serde_json::to_value(stats)?;
        coerce_whole_f64_to_int(&mut value);
        print_json(&value);
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

fn render_subagent_stats_table(stats: &[SubagentTypeStats]) -> String {
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

fn render_relationship_report(
    globals: &GlobalArgs,
    report: &SummaryRelationshipReport,
) -> anyhow::Result<i32> {
    if !report.subagent_types.is_empty() {
        return render_relationship_subagent_report(globals, report);
    }
    if report.relationships.is_empty() {
        return Ok(render_no_relationships(globals));
    }

    if globals.json {
        let mut value = json!({ "relationships": report.relationships });
        coerce_whole_f64_to_int(&mut value);
        print_json(&value);
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

fn render_relationship_subagent_report(
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
            print_json(&value);
            return Ok(0);
        }
        return Ok(render_no_relationships(globals));
    }
    if globals.json {
        let mut value = json!({
            "relationships": report.relationships,
            "subagentTypes": report.subagent_types,
        });
        coerce_whole_f64_to_int(&mut value);
        print_json(&value);
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

fn render_no_relationships(globals: &GlobalArgs) -> i32 {
    if globals.json {
        print_json(&json!({
            "relationships": [],
            "message": NO_RELATIONSHIPS_MESSAGE,
        }));
    } else {
        println!("{NO_RELATIONSHIPS_MESSAGE}");
    }
    0
}

fn render_subagent_tree_report(
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
        print_json(&value);
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
    out.extend(render_tree(&root));
    out.push(String::new());
    print!("{}", out.join("\n"));
    Ok(0)
}

fn render_tree(root: &SubagentTreeNode) -> Vec<String> {
    let mut out = Vec::new();
    out.push(render_node_line(root, ""));
    render_children(root, "", &mut out);
    out
}

fn render_children(node: &SubagentTreeNode, prefix: &str, out: &mut Vec<String>) {
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

fn render_node_line(node: &SubagentTreeNode, indent: &str) -> String {
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

fn emit_human(report: &SummaryGroupedReport, ingest_report: &relayburn_sdk::IngestReport) {
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
}

fn render_quality(q: &QualityResult) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_provider_filter_trims_and_lowercases_csv() {
        let got = parse_provider_filter(Some(" Anthropic,OPENAI ,,"))
            .unwrap()
            .unwrap();
        assert!(got.contains("anthropic"));
        assert!(got.contains("openai"));
        assert_eq!(got.len(), 2);
        assert_eq!(
            parse_provider_filter(Some(" , ")),
            Err("burn: --provider requires a value"),
        );
    }

    #[test]
    fn parse_tag_filters_requires_kv_with_non_empty_key() {
        let got = parse_tag_filters(&["persona=code-reviewer".to_string()]).unwrap();
        assert_eq!(
            got.get("persona").map(String::as_str),
            Some("code-reviewer")
        );

        let missing_eq = parse_tag_filters(&["persona".to_string()]).unwrap_err();
        assert!(format!("{missing_eq}").contains("--tag expects k=v"));

        let empty_key = parse_tag_filters(&["=value".to_string()]).unwrap_err();
        assert!(format!("{empty_key}").contains("--tag key must be non-empty"));

        let duplicate = parse_tag_filters(&[
            "persona=code-reviewer".to_string(),
            "persona=builder".to_string(),
        ])
        .unwrap_err();
        assert!(format!("{duplicate}").contains("duplicate --tag filter"));
    }

    #[test]
    fn grouped_json_includes_quality_when_report_has_it() {
        let report = SummaryGroupedReport {
            group_by: SummaryGroupBy::Model,
            tag_key: None,
            tag_values: Vec::new(),
            turn_count: 0,
            rows: Vec::new(),
            total_cost: CostBreakdown {
                model: String::new().into(),
                total: 0.0,
                input: 0.0,
                output: 0.0,
                reasoning: 0.0,
                cache_read: 0.0,
                cache_create: 0.0,
            },
            fidelity: relayburn_sdk::summarize_fidelity(&[]),
            per_cell_fidelity: json!({"groupBy": "model"}),
            replacement_savings: relayburn_sdk::ReplacementSavingsSummary::default(),
            quality: Some(QualityResult::default()),
        };

        let value = grouped_json_value(&report, &relayburn_sdk::IngestReport::empty());

        assert_eq!(value["quality"], json!({"outcomes": [], "oneShot": []}));
    }

    #[test]
    fn ingest_prelude_text_matches_human_banner() {
        let report = relayburn_sdk::IngestReport {
            ingested_sessions: 1,
            appended_turns: 2_000,
            ..relayburn_sdk::IngestReport::empty()
        };

        assert_eq!(
            ingest_prelude_text(&report),
            "\ningested 1 new session (+2,000 turns)\n",
        );
    }
}
