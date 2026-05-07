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
//! 3. Pull turns with [`relayburn_sdk::Query`] filters lowered from CLI
//!    flags (`--since`, `--project`, `--session`).
//! 4. Aggregate into per-model rows (or per-provider with `--by-provider`),
//!    derive a slice-wide `CostBreakdown` via
//!    [`relayburn_sdk::sum_costs`], and capture coverage / fidelity via
//!    [`relayburn_sdk::summarize_fidelity`] +
//!    [`relayburn_sdk::summarize_replacement_savings`].
//! 5. Render JSON or human format.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use clap::Args;
use indexmap::IndexMap;
use relayburn_sdk::{
    aggregate_by_provider, aggregate_subagent_type_stats, build_subagent_tree, compute_quality,
    cost_for_turn, ingest_all, load_pricing, normalize_since, provider_for, sum_costs,
    summarize_fidelity, summarize_replacement_savings, AggregateByProviderOptions,
    BuildSubagentTreeOptions, ComputeQualityOptions, ContentRecord, CostBreakdown, Coverage,
    CoverageField, EnrichedTurn, FidelityClass, FidelitySummary, Ledger, LedgerHandle,
    LedgerOpenOptions, OutcomeLabel, ProviderAggregateRow, QualityResult, Query, RawLedger,
    RelationshipType, RowCoverage, SessionRelationshipRecord, SubagentTreeNode, SubagentTypeStats,
    TurnRecord, UsageCostAggregateRow, UserTurnBlockKind, UserTurnRecord,
};
use serde::Serialize;
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
        && (args.by_subagent_type || args.by_relationship.is_some() || args.subagent_tree.is_some())
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
            eprintln!("burn: --by-relationship accepts only the optional value \"subagent\"");
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

    let opts = match globals.ledger_path.as_deref() {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    let mut handle = Ledger::open(opts)?;

    let ingest_report = run_ingest(&mut handle)?;

    let q = build_query(&args)?;
    let agent_session_ids = match args.agent.as_deref() {
        Some(agent_id) => Some(resolve_agent_session_tree(handle.raw(), agent_id)?),
        None => None,
    };

    if let Some(tree_flag) = args.subagent_tree.as_deref() {
        return render_subagent_tree_mode(
            globals,
            handle.raw(),
            tree_flag,
            &q,
            args.agent.as_deref(),
            agent_session_ids.as_ref(),
            provider_filter.as_ref(),
        );
    }

    let enriched = handle.raw().query_turns(&q)?;
    let enriched = filter_enriched_turns(
        enriched,
        args.agent.as_deref(),
        agent_session_ids.as_ref(),
        provider_filter.as_ref(),
    );
    let turns = turns_from_enriched(&enriched);
    let pricing = load_pricing(None);

    if args.by_subagent_type {
        return render_subagent_type_mode(globals, &turns, &pricing);
    }

    if let Some(rel_flag) = args.by_relationship.as_deref() {
        return render_relationship_mode(globals, handle.raw(), &turns, &pricing, &q, rel_flag);
    }

    if args.by_tool {
        return render_by_tool_mode(globals, handle.raw(), &turns, &ingest_report, &pricing);
    }

    let fidelity = summarize_fidelity(&turns);
    let savings = summarize_replacement_savings(&turns, None);
    let quality = if args.quality && !globals.json {
        Some(compute_quality_for_turns(handle.raw(), &turns)?)
    } else {
        None
    };

    if args.by_provider {
        let rows = aggregate_by_provider(&turns, AggregateByProviderOptions::new(&pricing));
        let provider_rows: Vec<UsageCostAggregateRow> =
            rows.into_iter().map(provider_to_aggregate_row).collect();
        emit_grouped(
            globals,
            true,
            &provider_rows,
            &turns,
            &ingest_report,
            &fidelity,
            &savings,
            quality.as_ref(),
        );
    } else {
        let rows = aggregate_by_model(&turns, &pricing);
        emit_grouped(
            globals,
            false,
            &rows,
            &turns,
            &ingest_report,
            &fidelity,
            &savings,
            quality.as_ref(),
        );
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

fn filter_enriched_turns(
    turns: Vec<EnrichedTurn>,
    agent_id: Option<&str>,
    agent_session_ids: Option<&HashSet<String>>,
    provider_filter: Option<&BTreeSet<String>>,
) -> Vec<EnrichedTurn> {
    turns
        .into_iter()
        .filter(|t| agent_passes(t, agent_id, agent_session_ids))
        .filter(|t| provider_passes(&t.turn, provider_filter))
        .collect()
}

fn agent_passes(
    t: &EnrichedTurn,
    agent_id: Option<&str>,
    session_ids: Option<&HashSet<String>>,
) -> bool {
    let Some(agent_id) = agent_id else {
        return true;
    };
    if t.enrichment.get("agentId").map(String::as_str) == Some(agent_id) {
        return true;
    }
    if t.enrichment.get("parentAgentId").map(String::as_str) == Some(agent_id) {
        return true;
    }
    session_ids
        .map(|ids| ids.contains(&t.turn.session_id))
        .unwrap_or(false)
}

fn provider_passes(t: &TurnRecord, provider_filter: Option<&BTreeSet<String>>) -> bool {
    let Some(filter) = provider_filter else {
        return true;
    };
    let provider = provider_for(t).provider.to_ascii_lowercase();
    filter.contains(&provider)
}

fn turns_from_enriched(enriched: &[EnrichedTurn]) -> Vec<TurnRecord> {
    enriched.iter().map(|e| e.turn.clone()).collect()
}

fn resolve_agent_session_tree(
    ledger: &RawLedger,
    agent_id: &str,
) -> anyhow::Result<HashSet<String>> {
    Ok(collect_agent_session_tree(
        &ledger.query_relationships(&Query::default())?,
        agent_id,
    ))
}

fn collect_agent_session_tree(
    relationships: &[SessionRelationshipRecord],
    agent_id: &str,
) -> HashSet<String> {
    let mut by_parent: HashMap<String, Vec<&SessionRelationshipRecord>> = HashMap::new();
    for r in relationships {
        if r.relationship_type != RelationshipType::Subagent {
            continue;
        }
        let Some(parent) = r.related_session_id.as_deref() else {
            continue;
        };
        if parent.is_empty() {
            continue;
        }
        by_parent.entry(parent.to_string()).or_default().push(r);
    }

    let mut sessions = HashSet::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([agent_id.to_string()]);
    while let Some(parent) = queue.pop_front() {
        if !seen.insert(parent.clone()) {
            continue;
        }
        for child in by_parent.get(&parent).into_iter().flatten() {
            sessions.insert(child.session_id.clone());
            queue.push_back(child.session_id.clone());
            if let Some(agent) = child.agent_id.as_ref() {
                if !agent.is_empty() {
                    queue.push_back(agent.clone());
                }
            }
        }
    }
    sessions
}

fn compute_quality_for_turns(
    ledger: &RawLedger,
    turns: &[TurnRecord],
) -> anyhow::Result<QualityResult> {
    let content_by_session = load_content_for_quality(ledger, turns)?;
    Ok(compute_quality(
        turns,
        &ComputeQualityOptions {
            content_by_session: Some(&content_by_session),
            now_ms: None,
        },
    ))
}

fn load_content_for_quality(
    ledger: &RawLedger,
    turns: &[TurnRecord],
) -> anyhow::Result<HashMap<String, Vec<ContentRecord>>> {
    let mut seen = HashSet::new();
    let mut out = HashMap::new();
    for t in turns {
        if !seen.insert(t.session_id.clone()) {
            continue;
        }
        let records = ledger.query_content(&Query {
            session_id: Some(t.session_id.clone()),
            ..Default::default()
        })?;
        if !records.is_empty() {
            out.insert(t.session_id.clone(), records);
        }
    }
    Ok(out)
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
    if let Some(workflow) = &args.workflow {
        let mut enrichment = std::collections::BTreeMap::new();
        enrichment.insert("workflowId".to_string(), workflow.clone());
        q.enrichment = Some(enrichment);
    }
    Ok(q)
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
    fidelity: &FidelitySummary,
    savings: &relayburn_sdk::ReplacementSavingsSummary,
    quality: Option<&QualityResult>,
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
    emit_human(
        by_provider,
        rows,
        ingest_report,
        &total_cost,
        fidelity,
        savings,
        quality,
    );
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

#[derive(Debug, Default, Clone)]
struct ToolAgg {
    calls: u64,
    cost: f64,
    sized_cost: f64,
    even_split_cost: f64,
}

#[derive(Debug, Default)]
struct UserTurnSizeBucket {
    tool_bytes_by_id: HashMap<String, u64>,
    total_bytes: u64,
}

fn render_by_tool_mode(
    globals: &GlobalArgs,
    ledger: &RawLedger,
    turns: &[TurnRecord],
    ingest_report: &relayburn_sdk::IngestReport,
    pricing: &relayburn_sdk::PricingTable,
) -> anyhow::Result<i32> {
    let user_turns_by_session = load_user_turns_for_by_tool(ledger, turns)?;
    let (by_tool, unattributed) = attribute_cost_to_tools(turns, pricing, &user_turns_by_session);
    let fidelity = summarize_fidelity(turns);
    let savings = summarize_replacement_savings(turns, None);
    let mut sorted: Vec<(String, ToolAgg)> = by_tool.into_iter().collect();
    sorted.sort_by(|a, b| {
        b.1.cost
            .partial_cmp(&a.1.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if globals.json {
        let by_tool_json: Vec<Value> = sorted
            .iter()
            .map(|(tool, agg)| {
                let mut row = Map::new();
                row.insert("tool".into(), json!(tool));
                row.insert("calls".into(), json!(agg.calls));
                row.insert("attributedCost".into(), json!(agg.cost));
                row.insert(
                    "attributionMethod".into(),
                    json!(tool_attribution_method(agg)),
                );
                if let Some(s) = savings.by_tool.get(tool) {
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
            "turns": turns.len(),
            "byTool": by_tool_json,
            "unattributed": unattributed,
            "fidelity": { "summary": fidelity_summary_to_json(&fidelity) },
        });
        if savings.calls > 0 {
            payload.as_object_mut().unwrap().insert(
                "replacementSavings".into(),
                replacement_savings_to_json(&savings),
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
        format_uint(turns.len() as u64)
    ));
    out.push(String::new());
    if sorted.is_empty() {
        out.push("no tool calls found for filters.".to_string());
        let mut text = out.join("\n");
        text.push('\n');
        print!("{text}");
        return Ok(0);
    }

    let has_savings = savings.calls > 0;
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
    for (tool, agg) in &sorted {
        let mut row = vec![tool.clone(), format_uint(agg.calls), format_usd(agg.cost)];
        if has_savings {
            let saved = savings
                .by_tool
                .get(tool)
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
        format_usd(unattributed),
    ));
    if has_savings {
        out.push(format_replacement_savings_line(&savings));
    }
    out.push(String::new());
    print!("{}", out.join("\n"));
    Ok(0)
}

fn load_user_turns_for_by_tool(
    ledger: &RawLedger,
    turns: &[TurnRecord],
) -> anyhow::Result<HashMap<String, Vec<UserTurnRecord>>> {
    let mut seen = HashSet::new();
    let mut out = HashMap::new();
    for t in turns {
        if !seen.insert(t.session_id.clone()) {
            continue;
        }
        let rows = ledger.query_user_turns(&Query {
            session_id: Some(t.session_id.clone()),
            ..Default::default()
        })?;
        if !rows.is_empty() {
            out.insert(t.session_id.clone(), rows);
        }
    }
    Ok(out)
}

fn attribute_cost_to_tools(
    turns: &[TurnRecord],
    pricing: &relayburn_sdk::PricingTable,
    user_turns_by_session: &HashMap<String, Vec<UserTurnRecord>>,
) -> (IndexMap<String, ToolAgg>, f64) {
    let mut by_tool: IndexMap<String, ToolAgg> = IndexMap::new();
    let mut unattributed = 0.0;
    let mut by_session: IndexMap<String, Vec<&TurnRecord>> = IndexMap::new();
    for t in turns {
        by_session.entry(t.session_id.clone()).or_default().push(t);
    }

    for (session_id, mut list) in by_session {
        list.sort_by_key(|t| t.turn_index);
        let user_turn_size_index = index_user_turn_block_sizes(
            user_turns_by_session
                .get(&session_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        );
        for i in 0..list.len() {
            let turn = list[i];
            let Some(c) = cost_for_turn(turn, pricing) else {
                continue;
            };
            let ingest_cost = c.input + c.cache_read + c.cache_create;

            for tc in &turn.tool_calls {
                by_tool.entry(tc.name.clone()).or_default().calls += 1;
            }

            if i == 0 {
                unattributed += ingest_cost;
                continue;
            }
            let prior = list[i - 1];
            if prior.tool_calls.is_empty() {
                unattributed += ingest_cost;
                continue;
            }

            let key = bridge_key(&prior.message_id, &turn.message_id);
            let sizes = user_turn_size_index.get(&key);
            let sized_bytes: u64 = match sizes {
                Some(s) => prior
                    .tool_calls
                    .iter()
                    .map(|tc| *s.tool_bytes_by_id.get(&tc.id).unwrap_or(&0))
                    .sum(),
                None => 0,
            };
            if let Some(sizes) = sizes.filter(|_| sized_bytes > 0) {
                let allocatable_cost = if sizes.total_bytes > 0 {
                    ingest_cost * (sized_bytes as f64 / sizes.total_bytes as f64).min(1.0)
                } else {
                    ingest_cost
                };
                unattributed += ingest_cost - allocatable_cost;
                let mut raw_shares: Vec<(String, f64)> = Vec::new();
                for tc in &prior.tool_calls {
                    let bytes = *sizes.tool_bytes_by_id.get(&tc.id).unwrap_or(&0);
                    if bytes == 0 {
                        continue;
                    }
                    raw_shares.push((
                        tc.name.clone(),
                        (bytes as f64 / sized_bytes as f64) * allocatable_cost,
                    ));
                }
                let raw_subtotal: f64 = raw_shares.iter().map(|(_, cost)| *cost).sum();
                let scale = if raw_subtotal > allocatable_cost && raw_subtotal > 0.0 {
                    allocatable_cost / raw_subtotal
                } else {
                    1.0
                };
                for (tool, cost) in raw_shares {
                    let share = cost * scale;
                    let agg = by_tool.entry(tool).or_default();
                    agg.cost += share;
                    agg.sized_cost += share;
                }
            } else {
                let share = ingest_cost / prior.tool_calls.len() as f64;
                for tc in &prior.tool_calls {
                    let agg = by_tool.entry(tc.name.clone()).or_default();
                    agg.cost += share;
                    agg.even_split_cost += share;
                }
            }
        }
    }

    (by_tool, unattributed)
}

fn index_user_turn_block_sizes(
    user_turns: &[UserTurnRecord],
) -> HashMap<String, UserTurnSizeBucket> {
    let mut out: HashMap<String, UserTurnSizeBucket> = HashMap::new();
    for user_turn in user_turns {
        let (Some(preceding), Some(following)) = (
            user_turn.preceding_message_id.as_ref(),
            user_turn.following_message_id.as_ref(),
        ) else {
            continue;
        };
        let bucket = out.entry(bridge_key(preceding, following)).or_default();
        for block in &user_turn.blocks {
            let bytes = block.byte_len;
            bucket.total_bytes += bytes;
            if block.kind != UserTurnBlockKind::ToolResult {
                continue;
            }
            let Some(tool_use_id) = block.tool_use_id.as_ref() else {
                continue;
            };
            *bucket
                .tool_bytes_by_id
                .entry(tool_use_id.clone())
                .or_default() += bytes;
        }
    }
    out
}

fn bridge_key(preceding_message_id: &str, following_message_id: &str) -> String {
    format!("{preceding_message_id}\0{following_message_id}")
}

fn tool_attribution_method(agg: &ToolAgg) -> &'static str {
    if agg.sized_cost == 0.0 && agg.even_split_cost == 0.0 {
        "unattributed"
    } else if agg.sized_cost >= agg.even_split_cost {
        "sized"
    } else {
        "even-split"
    }
}

fn render_subagent_type_mode(
    globals: &GlobalArgs,
    turns: &[TurnRecord],
    pricing: &relayburn_sdk::PricingTable,
) -> anyhow::Result<i32> {
    let stats = aggregate_subagent_type_stats(turns, &BuildSubagentTreeOptions::new(pricing));
    if globals.json {
        let mut value = serde_json::to_value(&stats)?;
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
    out.push(render_subagent_stats_table(&stats));
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

const RELATIONSHIP_ORDER: [RelationshipType; 4] = [
    RelationshipType::Root,
    RelationshipType::Continuation,
    RelationshipType::Fork,
    RelationshipType::Subagent,
];

#[derive(Debug, Clone)]
struct RelationshipMatch {
    relationship_type: RelationshipType,
    session_id: String,
    subagent_type: Option<String>,
    turn_count: u64,
    cost: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RelationshipStats {
    relationship_type: RelationshipType,
    count: u64,
    session_count: u64,
    turn_count: u64,
    total_cost: f64,
    median_cost: f64,
    p95_cost: f64,
    mean_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RelationshipSubagentStats {
    subagent_type: String,
    invocations: u64,
    turns: u64,
    total_cost: f64,
    median_cost: f64,
    p95_cost: f64,
    mean_cost: f64,
}

fn render_relationship_mode(
    globals: &GlobalArgs,
    ledger: &RawLedger,
    turns: &[TurnRecord],
    pricing: &relayburn_sdk::PricingTable,
    q: &Query,
    flag: &str,
) -> anyhow::Result<i32> {
    let relationships = ledger.query_relationships(&relationship_query_for_turn_slice(q))?;
    let matches = match_relationships_to_turns(&relationships, turns, pricing);
    let stats = aggregate_relationship_stats(&matches);

    if flag == "subagent" {
        return render_relationship_subagent_mode(globals, &stats, &matches);
    }
    if stats.is_empty() {
        return Ok(render_no_relationships(globals));
    }

    if globals.json {
        let mut value = json!({ "relationships": stats });
        coerce_whole_f64_to_int(&mut value);
        print_json(&value);
        return Ok(0);
    }

    let mut out = Vec::new();
    out.push(String::new());
    out.push(format!(
        "relationships: {}",
        format_uint(stats.iter().map(|s| s.session_count).sum()),
    ));
    out.push(String::new());
    let mut rows = vec![vec![
        "relationshipType".into(),
        "sessionCount".into(),
        "turnCount".into(),
        "total".into(),
        "median".into(),
        "p95".into(),
        "mean".into(),
    ]];
    for s in &stats {
        rows.push(vec![
            s.relationship_type.wire_str().to_string(),
            format_uint(s.session_count),
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

fn render_relationship_subagent_mode(
    globals: &GlobalArgs,
    stats: &[RelationshipStats],
    matches: &[RelationshipMatch],
) -> anyhow::Result<i32> {
    let subagent_stats = aggregate_relationship_subagent_stats(matches);
    if subagent_stats.is_empty() {
        return Ok(render_no_relationships(globals));
    }
    if globals.json {
        let subagent_relationships: Vec<&RelationshipStats> = stats
            .iter()
            .filter(|s| s.relationship_type == RelationshipType::Subagent)
            .collect();
        let mut value = json!({
            "relationships": subagent_relationships,
            "subagentTypes": subagent_stats,
        });
        coerce_whole_f64_to_int(&mut value);
        print_json(&value);
        return Ok(0);
    }

    let mut out = Vec::new();
    out.push(String::new());
    out.push(format!(
        "subagent invocations: {}",
        format_uint(subagent_stats.iter().map(|s| s.invocations).sum()),
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
    for s in &subagent_stats {
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

fn relationship_query_for_turn_slice(q: &Query) -> Query {
    Query {
        session_id: q.session_id.clone(),
        source: q.source,
        ..Default::default()
    }
}

struct RelationshipTurnIndex<'a> {
    all_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    main_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    sidechain_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    subagent_by_session_agent: HashMap<String, Vec<&'a TurnRecord>>,
}

fn match_relationships_to_turns(
    relationships: &[SessionRelationshipRecord],
    turns: &[TurnRecord],
    pricing: &relayburn_sdk::PricingTable,
) -> Vec<RelationshipMatch> {
    let index = build_relationship_turn_index(turns);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for r in relationships {
        let key = relationship_instance_key(r);
        if !seen.insert(key) {
            continue;
        }
        let matched_turns = turns_for_relationship(r, &index);
        if matched_turns.is_empty() {
            continue;
        }
        let cost = matched_turns
            .iter()
            .map(|t| cost_for_turn(t, pricing).map(|c| c.total).unwrap_or(0.0))
            .sum();
        out.push(RelationshipMatch {
            relationship_type: r.relationship_type,
            session_id: r.session_id.clone(),
            subagent_type: relationship_subagent_type(r, &matched_turns),
            turn_count: matched_turns.len() as u64,
            cost,
        });
    }
    out
}

fn build_relationship_turn_index(turns: &[TurnRecord]) -> RelationshipTurnIndex<'_> {
    let mut index = RelationshipTurnIndex {
        all_by_session: HashMap::new(),
        main_by_session: HashMap::new(),
        sidechain_by_session: HashMap::new(),
        subagent_by_session_agent: HashMap::new(),
    };
    for turn in turns {
        index
            .all_by_session
            .entry(turn.session_id.clone())
            .or_default()
            .push(turn);
        if is_main_thread_turn(turn) {
            index
                .main_by_session
                .entry(turn.session_id.clone())
                .or_default()
                .push(turn);
        }
        if turn
            .subagent
            .as_ref()
            .map(|s| s.is_sidechain)
            .unwrap_or(false)
        {
            index
                .sidechain_by_session
                .entry(turn.session_id.clone())
                .or_default()
                .push(turn);
        }
        if let Some(agent_id) = turn.subagent.as_ref().and_then(|s| s.agent_id.as_ref()) {
            if !agent_id.is_empty() {
                index
                    .subagent_by_session_agent
                    .entry(session_agent_key(&turn.session_id, agent_id))
                    .or_default()
                    .push(turn);
            }
        }
    }
    index
}

fn turns_for_relationship<'a>(
    r: &SessionRelationshipRecord,
    index: &'a RelationshipTurnIndex<'a>,
) -> Vec<&'a TurnRecord> {
    match r.relationship_type {
        RelationshipType::Root => index
            .main_by_session
            .get(&r.session_id)
            .cloned()
            .unwrap_or_default(),
        RelationshipType::Subagent => {
            if let Some(agent_id) = r.agent_id.as_ref().filter(|s| !s.is_empty()) {
                let key = session_agent_key(&r.session_id, agent_id);
                if let Some(direct) = index.subagent_by_session_agent.get(&key) {
                    if !direct.is_empty() {
                        return direct.clone();
                    }
                }
                if r.session_id == *agent_id {
                    return index
                        .all_by_session
                        .get(&r.session_id)
                        .cloned()
                        .unwrap_or_default();
                }
            }
            if let Some(sidechain) = index.sidechain_by_session.get(&r.session_id) {
                if !sidechain.is_empty() {
                    return sidechain.clone();
                }
            }
            if r.source.wire_str() == "spawn-env" {
                return index
                    .all_by_session
                    .get(&r.session_id)
                    .cloned()
                    .unwrap_or_default();
            }
            Vec::new()
        }
        RelationshipType::Continuation | RelationshipType::Fork => index
            .all_by_session
            .get(&r.session_id)
            .cloned()
            .unwrap_or_default(),
    }
}

fn aggregate_relationship_stats(matches: &[RelationshipMatch]) -> Vec<RelationshipStats> {
    let mut by_type: HashMap<RelationshipType, HashMap<String, (u64, f64)>> = HashMap::new();
    for m in matches {
        let by_session = by_type.entry(m.relationship_type).or_default();
        let current = by_session.entry(m.session_id.clone()).or_default();
        current.0 += m.turn_count;
        current.1 += m.cost;
    }

    let mut out = Vec::new();
    for relationship_type in RELATIONSHIP_ORDER {
        let Some(by_session) = by_type.get(&relationship_type) else {
            continue;
        };
        if by_session.is_empty() {
            continue;
        }
        let mut costs: Vec<f64> = by_session.values().map(|(_, cost)| *cost).collect();
        costs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let total_cost: f64 = costs.iter().sum();
        let session_count = by_session.len() as u64;
        out.push(RelationshipStats {
            relationship_type,
            count: session_count,
            session_count,
            turn_count: by_session.values().map(|(turns, _)| *turns).sum(),
            total_cost,
            median_cost: percentile(&costs, 0.5),
            p95_cost: percentile(&costs, 0.95),
            mean_cost: if session_count > 0 {
                total_cost / session_count as f64
            } else {
                0.0
            },
        });
    }
    out
}

fn aggregate_relationship_subagent_stats(
    matches: &[RelationshipMatch],
) -> Vec<RelationshipSubagentStats> {
    struct Agg {
        turns: u64,
        total: f64,
        costs: Vec<f64>,
    }
    let mut by_type: IndexMap<String, Agg> = IndexMap::new();
    for m in matches {
        if m.relationship_type != RelationshipType::Subagent {
            continue;
        }
        let ty = m
            .subagent_type
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string());
        let agg = by_type.entry(ty).or_insert_with(|| Agg {
            turns: 0,
            total: 0.0,
            costs: Vec::new(),
        });
        agg.turns += m.turn_count;
        agg.total += m.cost;
        agg.costs.push(m.cost);
    }

    let mut out = Vec::new();
    for (subagent_type, mut agg) in by_type {
        agg.costs
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let invocations = agg.costs.len() as u64;
        out.push(RelationshipSubagentStats {
            subagent_type,
            invocations,
            turns: agg.turns,
            total_cost: agg.total,
            median_cost: percentile(&agg.costs, 0.5),
            p95_cost: percentile(&agg.costs, 0.95),
            mean_cost: if invocations > 0 {
                agg.total / invocations as f64
            } else {
                0.0
            },
        });
    }
    out.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn relationship_subagent_type(
    relationship: &SessionRelationshipRecord,
    turns: &[&TurnRecord],
) -> Option<String> {
    if let Some(st) = &relationship.subagent_type {
        return Some(st.clone());
    }
    turns.iter().find_map(|t| {
        t.subagent
            .as_ref()
            .and_then(|s| s.subagent_type.as_ref())
            .cloned()
    })
}

fn relationship_instance_key(r: &SessionRelationshipRecord) -> String {
    [
        r.source.wire_str(),
        r.relationship_type.wire_str(),
        &r.session_id,
        r.related_session_id.as_deref().unwrap_or(""),
        r.agent_id.as_deref().unwrap_or(""),
        r.parent_tool_use_id.as_deref().unwrap_or(""),
    ]
    .join("\0")
}

fn session_agent_key(session_id: &str, agent_id: &str) -> String {
    format!("{session_id}\0{agent_id}")
}

fn is_main_thread_turn(turn: &TurnRecord) -> bool {
    match &turn.subagent {
        None => true,
        Some(sub) => !sub.is_sidechain || sub.agent_id.as_deref() == Some(&turn.session_id),
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank =
        ((p * sorted.len() as f64).ceil() as i64 - 1).clamp(0, sorted.len() as i64 - 1) as usize;
    sorted[rank]
}

fn render_subagent_tree_mode(
    globals: &GlobalArgs,
    ledger: &RawLedger,
    flag: &str,
    q: &Query,
    agent_filter: Option<&str>,
    agent_session_ids: Option<&HashSet<String>>,
    provider_filter: Option<&BTreeSet<String>>,
) -> anyhow::Result<i32> {
    let session_id = if !flag.is_empty() {
        flag.to_string()
    } else if let Some(session) = &q.session_id {
        session.clone()
    } else {
        eprintln!("burn: --subagent-tree requires a session id (positional or --session)");
        return Ok(2);
    };

    let relationships = collect_subagent_tree_relationships(ledger, &session_id, q)?;
    let enriched = load_subagent_tree_turns(ledger, &session_id, &relationships, q)?;
    let enriched =
        filter_enriched_turns(enriched, agent_filter, agent_session_ids, provider_filter);
    let turns = turns_from_enriched(&enriched);
    let pricing = load_pricing(None);
    let opts = BuildSubagentTreeOptions::new(&pricing).with_relationships(&relationships);
    let trees = build_subagent_tree(&turns, &opts);
    let root = trees
        .get(&session_id)
        .cloned()
        .or_else(|| find_tree_node(trees.values(), &session_id));
    let Some(root) = root else {
        println!("no turns found for session {session_id}");
        return Ok(0);
    };

    if globals.json {
        let mut value = serde_json::to_value(&root)?;
        coerce_whole_f64_to_int(&mut value);
        print_json(&value);
        return Ok(0);
    }

    let mut out = Vec::new();
    out.push(String::new());
    out.push(format!("session: {session_id}"));
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

fn collect_subagent_tree_relationships(
    ledger: &RawLedger,
    session_id: &str,
    q: &Query,
) -> anyhow::Result<Vec<SessionRelationshipRecord>> {
    let query_base = relationship_query_for_turn_slice(q);
    let mut out: IndexMap<String, SessionRelationshipRecord> = IndexMap::new();
    let mut seen_ids = HashSet::new();
    let mut queue = VecDeque::from([session_id.to_string()]);

    while let Some(id) = queue.pop_front() {
        if !seen_ids.insert(id.clone()) {
            continue;
        }
        let rows = ledger.query_relationships(&Query {
            session_id: Some(id),
            ..query_base.clone()
        })?;
        for r in rows {
            for next in relationship_connected_ids(&r) {
                if !next.is_empty() && !seen_ids.contains(&next) {
                    queue.push_back(next);
                }
            }
            out.insert(relationship_instance_key(&r), r);
        }
    }
    Ok(out.into_values().collect())
}

fn relationship_connected_ids(r: &SessionRelationshipRecord) -> Vec<String> {
    let mut ids = vec![r.session_id.clone()];
    if let Some(related) = &r.related_session_id {
        ids.push(related.clone());
    }
    if let Some(agent) = &r.agent_id {
        ids.push(agent.clone());
    }
    ids
}

fn load_subagent_tree_turns(
    ledger: &RawLedger,
    session_id: &str,
    relationships: &[SessionRelationshipRecord],
    q: &Query,
) -> anyhow::Result<Vec<EnrichedTurn>> {
    let mut session_ids = HashSet::from([session_id.to_string()]);
    for r in relationships {
        session_ids.insert(r.session_id.clone());
    }

    let mut by_key: IndexMap<String, EnrichedTurn> = IndexMap::new();
    for id in session_ids {
        let turns = ledger.query_turns(&Query {
            session_id: Some(id),
            ..q.clone()
        })?;
        for t in turns {
            let key = format!(
                "{}|{}|{}",
                t.turn.source.wire_str(),
                t.turn.session_id,
                t.turn.message_id,
            );
            by_key.insert(key, t);
        }
    }
    Ok(by_key.into_values().collect())
}

fn find_tree_node<'a>(
    trees: impl IntoIterator<Item = &'a SubagentTreeNode>,
    node_id: &str,
) -> Option<SubagentTreeNode> {
    for root in trees {
        if let Some(found) = find_node(root, node_id) {
            return Some(found.clone());
        }
    }
    None
}

fn find_node<'a>(node: &'a SubagentTreeNode, node_id: &str) -> Option<&'a SubagentTreeNode> {
    if node.node_id == node_id {
        return Some(node);
    }
    for child in &node.children {
        if let Some(found) = find_node(child, node_id) {
            return Some(found);
        }
    }
    None
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
            class.wire_str().to_string(),
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
            g.wire_str().to_string(),
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
        let av = a
            .get("estimatedTokensSaved")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let bv = b
            .get("estimatedTokensSaved")
            .and_then(Value::as_u64)
            .unwrap_or(0);
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
    quality: Option<&QualityResult>,
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

    if let Some(q) = quality {
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
    use relayburn_sdk::{RelationshipSourceKind, SourceKind, ToolCall, Usage, UserTurnBlock};

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
    fn collect_agent_session_tree_follows_nested_child_sessions_and_agent_ids() {
        let rels = vec![
            relationship("child-session", "root-agent", Some("child-agent")),
            relationship("grandchild-session", "child-session", Some("grand-agent")),
            relationship("great-grandchild-session", "child-agent", None),
        ];

        let sessions = collect_agent_session_tree(&rels, "root-agent");

        assert!(sessions.contains("child-session"));
        assert!(sessions.contains("grandchild-session"));
        assert!(sessions.contains("great-grandchild-session"));
        assert_eq!(sessions.len(), 3);
    }

    #[test]
    fn by_tool_attribution_uses_user_turn_block_byte_shares() {
        let pricing = load_pricing(None);
        let turns = vec![
            turn(
                0,
                "assistant-1",
                Usage::default(),
                vec![tool("call-read", "Read"), tool("call-edit", "Edit")],
            ),
            turn(
                1,
                "assistant-2",
                Usage {
                    input: 1_000,
                    ..Usage::default()
                },
                Vec::new(),
            ),
        ];
        let mut user_turns_by_session = HashMap::new();
        user_turns_by_session.insert(
            "session".to_string(),
            vec![UserTurnRecord {
                v: 1,
                source: SourceKind::ClaudeCode,
                session_id: "session".to_string(),
                user_uuid: "user-1".to_string(),
                ts: "2026-04-20T00:00:01.000Z".to_string(),
                preceding_message_id: Some("assistant-1".to_string()),
                following_message_id: Some("assistant-2".to_string()),
                blocks: vec![
                    tool_result_block("call-read", 75),
                    tool_result_block("call-edit", 25),
                ],
            }],
        );

        let (by_tool, unattributed) =
            attribute_cost_to_tools(&turns, &pricing, &user_turns_by_session);
        let read = by_tool.get("Read").expect("read agg");
        let edit = by_tool.get("Edit").expect("edit agg");

        assert_eq!(read.calls, 1);
        assert_eq!(edit.calls, 1);
        assert!(read.cost > edit.cost * 2.9);
        assert!(read.cost < edit.cost * 3.1);
        assert!(unattributed.abs() < 1e-12);
        assert_eq!(tool_attribution_method(read), "sized");
    }

    fn relationship(
        session_id: &str,
        related_session_id: &str,
        agent_id: Option<&str>,
    ) -> SessionRelationshipRecord {
        SessionRelationshipRecord {
            v: 1,
            source: RelationshipSourceKind::SpawnEnv,
            session_id: session_id.to_string(),
            related_session_id: Some(related_session_id.to_string()),
            relationship_type: RelationshipType::Subagent,
            ts: None,
            source_session_id: None,
            source_version: None,
            parent_tool_use_id: None,
            agent_id: agent_id.map(str::to_string),
            subagent_type: None,
            description: None,
        }
    }

    fn turn(
        turn_index: u64,
        message_id: &str,
        usage: Usage,
        tool_calls: Vec<ToolCall>,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "session".to_string(),
            session_path: None,
            message_id: message_id.to_string(),
            turn_index,
            ts: format!("2026-04-20T00:00:0{turn_index}.000Z"),
            model: "claude-sonnet-4-6".to_string(),
            project: None,
            project_key: None,
            usage,
            tool_calls,
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn tool(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            target: None,
            args_hash: "args".to_string(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn tool_result_block(tool_use_id: &str, byte_len: u64) -> UserTurnBlock {
        UserTurnBlock {
            kind: UserTurnBlockKind::ToolResult,
            tool_use_id: Some(tool_use_id.to_string()),
            byte_len,
            approx_tokens: 0,
            is_error: None,
        }
    }
}
