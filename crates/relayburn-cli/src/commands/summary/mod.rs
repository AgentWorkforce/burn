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
//! 2. Optionally run [`relayburn_sdk::ingest_all`] against the same handle,
//!    gated on `--ingest`. `summary` is a read verb, and a pre-query sweep
//!    re-stats every session file under every harness store — seconds on a
//!    large OpenCode store (tens of thousands of sessions) — so it is
//!    **off by default**. The steady-state setup keeps the ledger fresh out
//!    of band: `burn ingest --watch` once per host, or the Claude Stop hook
//!    (`burn ingest --hook claude`) firing each turn. Pass `--ingest` for a
//!    one-off freshen; the human banner then leads with
//!    `ingested N new sessions (+M turns)` and a TTY gets a stderr spinner.
//!    When the sweep is skipped, an empty [`relayburn_sdk::IngestReport`]
//!    keeps the banner / JSON `ingest` block byte-identical to a no-op sweep
//!    (`ingested 0 new sessions`). See `burn compare` for the same decision.
//! 3. Lower CLI flags into [`relayburn_sdk::SummaryReportOptions`] and call
//!    the SDK-owned `summary_report` verb.
//! 4. Render the typed report as JSON or human output.

use std::collections::{BTreeMap, BTreeSet};

use clap::Args;
use relayburn_sdk::{
    ingest_all, Enrichment, Ledger, LedgerHandle, LedgerOpenOptions, SummaryReport,
    SummaryReportMode, SummaryReportOptions,
};

use crate::cli::GlobalArgs;
use crate::render::error::report_error;
use crate::render::progress::TaskProgress;

mod human;
mod json;

use human::*;
use json::*;

#[cfg(test)]
use relayburn_sdk::{
    CostBreakdown, QualityResult, SubagentCounts, SummaryGroupBy, SummaryGroupedReport,
    UsageCostAggregateRow,
};
#[cfg(test)]
use serde_json::json;

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

    /// Accepted for TS CLI flag parity; a no-op against the Rust SDK,
    /// which is SQLite-native and has no archive layer to bypass.
    #[arg(long = "no-archive")]
    pub no_archive: bool,

    /// Run a pre-query ingest sweep so the summary leads with freshly
    /// appended sessions. Off by default: `summary` is a read verb, and a
    /// full-store sweep re-stats every session file under every harness
    /// store — seconds on a large ledger. Keep the ledger current out of
    /// band with `burn ingest --watch` (or the Claude Stop hook); pass
    /// `--ingest` only for a one-off freshen.
    #[arg(long = "ingest")]
    pub ingest: bool,

    /// Emit a time-series instead of a single total: bucket the `--since`
    /// window into fixed-width windows and report per-bucket cost/usage.
    /// Duration grammar: `30s`, `5m` (minutes), `1h`, `12h`, `1d`, `7d`.
    /// Only valid for the default grouped (`byModel`/`--by-provider`) summary.
    #[arg(long, value_name = "DURATION")]
    pub bucket: Option<String>,
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

    // `--bucket` opts into a per-bucket time-series. Parse and validate it
    // (including the mode/flag combinations it supports) before opening the
    // ledger or running ingest, so a bad invocation fails fast.
    let bucket_secs = if let Some(bucket_raw) = args.bucket.as_deref() {
        if args.by_tool
            || args.by_subagent_type
            || args.by_relationship.is_some()
            || args.subagent_tree.is_some()
            || args.group_by_tag.is_some()
        {
            eprintln!(
                "burn: --bucket is only supported with the default grouped summary or --by-provider"
            );
            return Ok(2);
        }
        if args.quality {
            eprintln!("burn: --bucket is not supported with --quality");
            return Ok(2);
        }
        match relayburn_sdk::parse_bucket(bucket_raw) {
            Ok(secs) => Some(secs),
            Err(err) => {
                eprintln!("burn: {err}");
                return Ok(2);
            }
        }
    } else {
        None
    };

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

    // `--no-archive` is accepted for TS CLI flag parity but is a no-op:
    // the Rust SDK is SQLite-native and has no archive layer to bypass.
    let _ = args.no_archive;
    let progress = TaskProgress::new(globals, "summary");

    let opts = match globals.ledger_path.as_deref() {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    progress.set_task("opening ledger");
    let mut handle = Ledger::open(opts).inspect_err(|_| {
        progress.finish_and_clear();
    })?;

    // Read-verb default: skip the pre-query sweep (see the module doc and
    // `burn compare`). `--ingest` opts back into a one-off freshen; otherwise
    // an empty report keeps the banner / JSON `ingest` block identical to a
    // no-op sweep without paying for the full-store stat walk.
    let ingest_report = if args.ingest {
        run_ingest(&mut handle, &progress, globals.ledger_path.clone()).inspect_err(|_| {
            progress.finish_and_clear();
        })?
    } else {
        relayburn_sdk::IngestReport::empty()
    };

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

    let opts = SummaryReportOptions {
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
    };

    // `--bucket` switches to a per-bucket time-series of the grouped summary.
    // Parsing/validation already happened above, before the ledger was opened.
    if let Some(bucket_secs) = bucket_secs {
        progress.set_task("building summary time-series");
        let series = handle
            .summary_timeseries(opts, bucket_secs)
            .inspect_err(|_| {
                progress.finish_and_clear();
            })?;
        progress.finish_and_clear();
        return emit_summary_timeseries(globals, &series, &ingest_report);
    }

    progress.set_task("building summary");
    let report = handle.summary_report(opts).inspect_err(|_| {
        progress.finish_and_clear();
    })?;
    progress.finish_and_clear();

    match report {
        SummaryReport::Grouped(report) => {
            emit_grouped(globals, &report, &ingest_report)?;
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

/// Run an ingest sweep on the open handle.
fn run_ingest(
    handle: &mut LedgerHandle,
    progress: &TaskProgress,
    ledger_home: Option<std::path::PathBuf>,
) -> anyhow::Result<relayburn_sdk::IngestReport> {
    progress.set_task("refreshing ledger");
    let opts = progress.ingest_options(ledger_home);
    ingest_all(handle.raw_mut(), &opts).inspect_err(|_| {
        progress.finish_and_clear();
    })
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
            stop_reasons: relayburn_sdk::StopReasonCounts::default(),
            subagents: SubagentCounts::default(),
            quality: Some(QualityResult::default()),
            unpriced_turns: 0,
            unpriced_models: Vec::new(),
        };

        let value = grouped_json_value(&report, &relayburn_sdk::IngestReport::empty());

        assert_eq!(value["quality"], json!({"outcomes": [], "oneShot": []}));
    }

    #[test]
    fn subagents_line_renders_only_when_counts_nonzero() {
        // Empty bucket → skipped, line absent (so old summaries keep
        // their byte-identical shape against the existing golden).
        let empty = SubagentCounts::default();
        assert!(empty.is_empty());

        // Non-zero bucket → human line includes both paired+orphan
        // counts. Issue #435 explicitly wants both numbers even when
        // one is zero, so a slash-command-only session showing only
        // orphans is still scannable.
        let counts = SubagentCounts {
            paired: 2,
            orphan: 1,
        };
        assert_eq!(
            format_subagents_line(&counts),
            "subagents: 2 paired, 1 orphan"
        );
    }

    #[test]
    fn subagents_json_payload_includes_total_and_omits_when_empty() {
        // Empty bucket → key absent in JSON so `summary.json | jq` for
        // pre-#435 callers still passes without a `?.` guard.
        let mut report = SummaryGroupedReport {
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
            stop_reasons: relayburn_sdk::StopReasonCounts::default(),
            subagents: SubagentCounts::default(),
            quality: None,
            unpriced_turns: 0,
            unpriced_models: Vec::new(),
        };
        let value = grouped_json_value(&report, &relayburn_sdk::IngestReport::empty());
        assert!(
            value.get("subagents").is_none(),
            "subagents key must be omitted when counts are zero; got {value}"
        );

        report.subagents = SubagentCounts {
            paired: 2,
            orphan: 1,
        };
        let value = grouped_json_value(&report, &relayburn_sdk::IngestReport::empty());
        assert_eq!(
            value["subagents"],
            json!({"paired": 2, "orphan": 1, "total": 3}),
            "non-empty subagent counts must surface `paired`/`orphan`/`total`"
        );
    }

    #[test]
    fn partial_footer_names_gap_and_says_totals_include_all_turns() {
        fn row(label: &str, coverage: relayburn_sdk::RowCoverage) -> UsageCostAggregateRow {
            UsageCostAggregateRow {
                label: label.to_string(),
                turns: 0,
                usage: relayburn_sdk::Usage::default(),
                cost: CostBreakdown {
                    model: String::new().into(),
                    total: 0.0,
                    input: 0.0,
                    output: 0.0,
                    reasoning: 0.0,
                    cache_read: 0.0,
                    cache_create: 0.0,
                },
                coverage,
            }
        }

        let mut claude_coverage = relayburn_sdk::RowCoverage::default();
        claude_coverage.input.known = 3;
        claude_coverage.output.known = 3;
        claude_coverage.reasoning.missing = 3;

        let mut codex_coverage = relayburn_sdk::RowCoverage::default();
        codex_coverage.input.known = 1;
        codex_coverage.output.known = 1;
        codex_coverage.reasoning.known = 1;

        assert_eq!(
            format_partial_footer(&[
                row("claude-sonnet-4-6", claude_coverage),
                row("gpt-5-codex", codex_coverage),
            ]),
            "* partial token coverage: largest gap is reasoning (missing on 3 of 4 turns); totals still include all turns",
        );
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
