//! `burn hotspots` — surface high-cost / high-overhead hotspots from
//! the ledger.
//!
//! Thin presenter over [`relayburn_sdk::hotspots`]. Mirrors
//! `packages/cli/src/commands/hotspots.ts` for the default attribution
//! flow that drives the golden snapshots; the broader TS surface
//! (`--patterns`, `--findings`, `--session` per-session view,
//! `--provider`, `--workflow`) is enumerated as flag wiring + a
//! stub-mode error path.
//!
//! ## Wiring
//!
//! 1. Open a [`relayburn_sdk::LedgerHandle`] honoring `--ledger-path` /
//!    `RELAYBURN_HOME`.
//! 2. Run [`relayburn_sdk::ingest_all`] silently (no progress spinner —
//!    that's a TTY-only concern that breaks golden output).
//! 3. Call [`relayburn_sdk::hotspots`] (verb-form) with the resolved
//!    [`relayburn_sdk::HotspotsOptions`]. The SDK enforces the coverage
//!    gate, picks the `Sized` vs `EvenSplit` attribution method per
//!    session, and emits the discriminated union; for the default flow
//!    we expect [`relayburn_sdk::HotspotsResult::Attribution`] and
//!    unwrap that branch.
//! 4. Render JSON or human format. JSON output drops the `kind`
//!    discriminator and emits the inner `HotspotsAttributionResult`
//!    shape directly (TS contract).

use clap::Args;
use relayburn_sdk::{
    hotspots as sdk_hotspots, ingest_all, AttributionMethod, BashAggregation,
    BashVerbAggregation, FileAggregation, HotspotsAttributionResult, HotspotsExcludedBreakdown,
    HotspotsExcludedSourceRow, HotspotsGroupBy, HotspotsOptions, HotspotsResult,
    HotspotsSessionTotal, Ledger, LedgerOpenOptions, SubagentAggregation, WasteFinding,
    WasteSeverity,
};
use serde_json::{json, Map, Value};

use crate::cli::GlobalArgs;
use crate::render::error::report_error;
use crate::render::format::{coerce_whole_f64_to_int, format_uint, format_usd, render_table};

const DEFAULT_TOP_N: usize = 10;

/// Per-command flags for `burn hotspots`. Mirrors the TS surface in
/// `packages/cli/src/commands/hotspots.ts`.
#[derive(Debug, Clone, Args)]
pub struct HotspotsArgs {
    /// Slice the ledger to events at or after `<since>`. ISO timestamp
    /// or relative range (`24h`, `7d`, `4w`, `2m`).
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// Restrict to a single project.
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<String>,

    /// Restrict to a single session id (or pass without a value to drop
    /// into the per-session attribution view).
    #[arg(long, value_name = "SESSION_ID", num_args = 0..=1, default_missing_value = "")]
    pub session: Option<String>,

    /// Filter by enrichment workflow id.
    #[arg(long, value_name = "WORKFLOW_ID")]
    pub workflow: Option<String>,

    /// Provider filter (CSV of provider names; case-insensitive).
    #[arg(long, value_name = "PROVIDERS")]
    pub provider: Option<String>,

    /// Show all rows in human mode instead of capping at the default
    /// top-N (10).
    #[arg(long)]
    pub all: bool,

    /// Group by a single dimension. Defaults to the full attribution
    /// view; pass `bash`, `bash-verb`, `file`, or `subagent` to focus
    /// a single rollup.
    #[arg(long = "group-by", value_name = "DIM")]
    pub group_by: Option<String>,

    /// Comma-separated waste-pattern detectors to run instead of the
    /// attribution view. Pass without a value to enable every detector.
    #[arg(long, value_name = "PATTERNS", num_args = 0..=1, default_missing_value = "")]
    pub patterns: Option<String>,

    /// Render the unified `findings` view rather than the per-detector
    /// summary. Implies `--patterns` if it isn't already set.
    #[arg(long)]
    pub findings: bool,

    /// Surface session relationship drift on top of the default attribution
    /// view. Currently a stub in the Rust port — the relationship drift
    /// query verb is not yet exposed by the SDK.
    #[arg(long = "explain-drift")]
    pub explain_drift: bool,
}

const PATTERN_KINDS: &[&str] = &[
    "retry-loop",
    "failure-run",
    "cancellation-run",
    "compaction",
    "edit-revert",
    "edit-heavy",
    "opencode-skill-recall",
    "opencode-skill-pruning",
    "opencode-system-prompt",
    "ghost-surface",
    "tool-output-bloat",
    "tool-call-pattern",
];

fn resolve_pattern_selection(raw: &str) -> Result<Vec<String>, String> {
    if raw.is_empty() {
        return Ok(PATTERN_KINDS.iter().map(|s| (*s).to_string()).collect());
    }
    let mut out: Vec<String> = Vec::new();
    for piece in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !PATTERN_KINDS.iter().any(|k| *k == piece) {
            return Err(format!(
                "unknown --patterns value \"{}\". Valid: {}",
                piece,
                PATTERN_KINDS.join(", ")
            ));
        }
        if !out.iter().any(|s| s == piece) {
            out.push(piece.to_string());
        }
    }
    if out.is_empty() {
        return Ok(PATTERN_KINDS.iter().map(|s| (*s).to_string()).collect());
    }
    Ok(out)
}

pub fn run(globals: &GlobalArgs, args: HotspotsArgs) -> i32 {
    match run_inner(globals, args) {
        Ok(code) => code,
        Err(err) => report_error(&err, globals),
    }
}

fn run_inner(globals: &GlobalArgs, args: HotspotsArgs) -> anyhow::Result<i32> {
    // The TS surface treats `--session` (no value) as "drop into the
    // per-session aggregate / gap report." That view weaves session
    // relationships, tool-result chronology, and per-session attribution —
    // none of which the SDK exposes yet. Keep it a clear stub.
    if matches!(args.session.as_deref(), Some("")) {
        eprintln!(
            "burn: per-session aggregate view (`--session` with no id) is not yet implemented in the Rust port. Pass a session id to filter the standard hotspots view."
        );
        return Ok(2);
    }
    if args.explain_drift {
        eprintln!(
            "burn: --explain-drift is not yet implemented in the Rust port (relationship-drift query verb hasn't landed in relayburn-sdk yet)."
        );
        return Ok(2);
    }

    // `--findings` standalone means "render findings unified view"; pin it
    // to `--patterns` (all detectors) so the resolver below sees a value.
    let patterns_arg: Option<&str> = if args.patterns.is_some() {
        args.patterns.as_deref()
    } else if args.findings {
        Some("")
    } else {
        None
    };
    let patterns_selection: Option<Vec<String>> = match patterns_arg {
        None => None,
        Some(raw) => match resolve_pattern_selection(raw) {
            Ok(sel) => Some(sel),
            Err(msg) => {
                eprintln!("burn: {msg}");
                return Ok(2);
            }
        },
    };

    let group_by = match args.group_by.as_deref() {
        None => None,
        Some("attribution") => Some(HotspotsGroupBy::Attribution),
        Some("bash") => Some(HotspotsGroupBy::Bash),
        Some("bash-verb") => Some(HotspotsGroupBy::BashVerb),
        Some("file") => Some(HotspotsGroupBy::File),
        Some("subagent") => Some(HotspotsGroupBy::Subagent),
        Some(other) => {
            eprintln!(
                "burn: unknown --group-by value \"{}\". Valid: attribution, bash, bash-verb, file, subagent",
                other
            );
            return Ok(2);
        }
    };

    if group_by.is_some() && patterns_selection.is_some() {
        eprintln!(
            "burn: --group-by and --patterns/--findings are mutually exclusive (group-by selects an attribution rollup; patterns/findings drive the detector view)."
        );
        return Ok(2);
    }

    let provider_filter: Option<Vec<String>> = args.provider.as_deref().and_then(|raw| {
        let parts: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        (!parts.is_empty()).then_some(parts)
    });

    // Open + ingest. We open the handle locally so ingest sees the same
    // sealed `RELAYBURN_HOME` the verb call does.
    let ledger_home = globals.ledger_path.clone();
    let opts = match &ledger_home {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    let mut handle = Ledger::open(opts)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let raw_opts = relayburn_sdk::RawIngestOptions::default();
    rt.block_on(ingest_all(handle.raw_mut(), &raw_opts))?;
    drop(handle);

    let session_filter = match args.session.as_deref() {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    };

    let result = sdk_hotspots(HotspotsOptions {
        session: session_filter,
        project: args.project.clone(),
        since: args.since.clone(),
        group_by,
        patterns: patterns_selection,
        workflow: args.workflow.clone(),
        provider: provider_filter,
        ledger_home,
    })?;

    if globals.json {
        emit_json(&result);
        return Ok(0);
    }
    let limit = if args.all { usize::MAX } else { DEFAULT_TOP_N };
    emit_human(&result, limit, args.findings);
    Ok(0)
}

fn emit_json(result: &HotspotsResult) {
    let mut value = hotspots_result_to_json(result);
    coerce_whole_f64_to_int(&mut value);
    let mut out = serde_json::to_string_pretty(&value).unwrap_or_default();
    out.push('\n');
    print!("{}", out);
}

fn hotspots_result_to_json(result: &HotspotsResult) -> Value {
    match result {
        HotspotsResult::Attribution(a) => attribution_to_json(a),
        HotspotsResult::Bash {
            rows,
            refused,
            refusal_reason,
        } => json!({
            "rows": rows.iter().map(bash_to_json).collect::<Vec<_>>(),
            "refused": refused,
            "refusalReason": refusal_reason,
        }),
        HotspotsResult::BashVerb {
            rows,
            refused,
            refusal_reason,
        } => json!({
            "rows": rows.iter().map(bash_verb_to_json).collect::<Vec<_>>(),
            "refused": refused,
            "refusalReason": refusal_reason,
        }),
        HotspotsResult::File {
            rows,
            refused,
            refusal_reason,
        } => json!({
            "rows": rows.iter().map(file_to_json).collect::<Vec<_>>(),
            "refused": refused,
            "refusalReason": refusal_reason,
        }),
        HotspotsResult::Subagent {
            rows,
            refused,
            refusal_reason,
        } => json!({
            "rows": rows.iter().map(subagent_to_json).collect::<Vec<_>>(),
            "refused": refused,
            "refusalReason": refusal_reason,
        }),
        HotspotsResult::Findings { findings, summary } => json!({
            "findings": findings,
            "summary": summary,
        }),
    }
}

fn attribution_to_json(a: &HotspotsAttributionResult) -> Value {
    let mut out = Map::new();
    out.insert("turnsAnalyzed".into(), json!(a.turns_analyzed));
    out.insert("grandTotal".into(), json!(a.grand_total));
    out.insert("attributedTotal".into(), json!(a.attributed_total));
    out.insert("unattributedTotal".into(), json!(a.unattributed_total));
    out.insert("attributionDegraded".into(), json!(a.attribution_degraded));
    out.insert(
        "sessions".into(),
        Value::Array(a.sessions.iter().map(session_total_to_json).collect()),
    );
    out.insert(
        "files".into(),
        Value::Array(a.files.iter().map(file_to_json).collect()),
    );
    out.insert(
        "bashVerbs".into(),
        Value::Array(a.bash_verbs.iter().map(bash_verb_to_json).collect()),
    );
    out.insert(
        "bash".into(),
        Value::Array(a.bash.iter().map(bash_to_json).collect()),
    );
    out.insert(
        "subagents".into(),
        Value::Array(a.subagents.iter().map(subagent_to_json).collect()),
    );
    out.insert(
        "fidelity".into(),
        json!({
            "analyzed": a.fidelity.analyzed,
            "excluded": a.fidelity.excluded,
            "summary": reorder_fidelity_summary(&a.fidelity.summary),
            "refused": a.fidelity.refused,
        }),
    );
    if let Some(refused) = a.refused {
        out.insert("refused".into(), json!(refused));
    }
    if let Some(reason) = a.refusal_reason.as_ref() {
        out.insert("refusalReason".into(), json!(reason));
    }
    Value::Object(out)
}

fn session_total_to_json(s: &HotspotsSessionTotal) -> Value {
    json!({
        "sessionId": s.session_id,
        "grandCost": s.grand_cost,
        "attributedCost": s.attributed_cost,
        "unattributedCost": s.unattributed_cost,
        "attributionMethod": attribution_method_key(s.attribution_method),
    })
}

/// Re-order the SDK-emitted fidelity summary so the JSON keys match the
/// TS-CLI snapshot ordering. The SDK builds `byClass` /
/// `byGranularity` / `missingCoverage` from `HashMap`s so iteration
/// order is non-deterministic; we reach into the `Value`, pull out the
/// numbers, and reassemble the object in the canonical order the TS
/// implementation uses (which is also the iteration order of the
/// upstream enum).
fn reorder_fidelity_summary(summary: &Value) -> Value {
    use serde_json::Map;
    let Some(obj) = summary.as_object() else {
        return summary.clone();
    };
    let mut out = Map::new();
    out.insert(
        "total".into(),
        obj.get("total").cloned().unwrap_or(json!(0)),
    );

    let mut by_class = Map::new();
    let class_block = obj.get("byClass").and_then(|v| v.as_object());
    for key in [
        "full",
        "usage-only",
        "aggregate-only",
        "cost-only",
        "partial",
    ] {
        let v = class_block
            .and_then(|m| m.get(key))
            .cloned()
            .unwrap_or(json!(0));
        by_class.insert(key.to_string(), v);
    }
    out.insert("byClass".into(), Value::Object(by_class));

    let mut by_granularity = Map::new();
    let gran_block = obj.get("byGranularity").and_then(|v| v.as_object());
    for key in [
        "per-turn",
        "per-message",
        "per-session-aggregate",
        "cost-only",
    ] {
        let v = gran_block
            .and_then(|m| m.get(key))
            .cloned()
            .unwrap_or(json!(0));
        by_granularity.insert(key.to_string(), v);
    }
    out.insert("byGranularity".into(), Value::Object(by_granularity));

    let mut missing = Map::new();
    let missing_block = obj.get("missingCoverage").and_then(|v| v.as_object());
    for key in [
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
        let v = missing_block
            .and_then(|m| m.get(key))
            .cloned()
            .unwrap_or(json!(0));
        missing.insert(key.to_string(), v);
    }
    out.insert("missingCoverage".into(), Value::Object(missing));
    out.insert(
        "unknown".into(),
        obj.get("unknown").cloned().unwrap_or(json!(0)),
    );
    Value::Object(out)
}

fn attribution_method_key(m: AttributionMethod) -> &'static str {
    match m {
        AttributionMethod::Sized => "sized",
        AttributionMethod::EvenSplit => "even-split",
    }
}

fn file_to_json(f: &FileAggregation) -> Value {
    json!({
        "path": f.path,
        "toolCallCount": f.tool_call_count,
        "initialTokens": f.initial_tokens,
        "persistenceTokens": f.persistence_tokens,
        "ridingTurns": f.riding_turns,
        "totalCost": f.total_cost,
        "firstEmitTs": f.first_emit_ts,
        "firstEmitTurnIndex": f.first_emit_turn_index,
    })
}

fn bash_to_json(b: &BashAggregation) -> Value {
    let mut out = Map::new();
    out.insert("argsHash".into(), json!(b.args_hash));
    if let Some(c) = &b.command {
        out.insert("command".into(), json!(c));
    }
    out.insert("callCount".into(), json!(b.call_count));
    out.insert("totalCost".into(), json!(b.total_cost));
    out.insert("initialTokens".into(), json!(b.initial_tokens));
    out.insert("persistenceTokens".into(), json!(b.persistence_tokens));
    Value::Object(out)
}

fn bash_verb_to_json(b: &BashVerbAggregation) -> Value {
    json!({
        "verb": b.verb,
        "callCount": b.call_count,
        "distinctCommands": b.distinct_commands,
        "totalCost": b.total_cost,
        "initialTokens": b.initial_tokens,
        "persistenceTokens": b.persistence_tokens,
        "avgPersistenceTurns": b.avg_persistence_turns,
        "topExamples": b.top_examples,
    })
}

fn subagent_to_json(s: &SubagentAggregation) -> Value {
    json!({
        "subagentType": s.subagent_type,
        "callCount": s.call_count,
        "totalCost": s.total_cost,
        "initialTokens": s.initial_tokens,
        "persistenceTokens": s.persistence_tokens,
    })
}

// ---------- human rendering ----------

fn emit_human(result: &HotspotsResult, limit: usize, findings_view: bool) {
    match result {
        HotspotsResult::Attribution(a) => emit_human_attribution(a, limit),
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
        HotspotsResult::Bash { rows, .. } => print_section_table(
            "Top exact Bash commands by cost",
            "(no Bash tool calls)",
            rows.iter().take(limit).map(bash_row),
            &["command", "calls", "initial(tok)", "persist(tok)", "cost"],
        ),
        HotspotsResult::BashVerb {
            refused: Some(true),
            refusal_reason,
            ..
        } => {
            print_refusal(refusal_reason.as_deref());
        }
        HotspotsResult::BashVerb { rows, .. } => print_section_table(
            "Top Bash verbs by cost",
            "(no Bash tool calls)",
            rows.iter().take(limit).map(bash_verb_row),
            &[
                "verb",
                "calls",
                "commands",
                "initial(tok)",
                "persist(tok)",
                "avgRide",
                "cost",
                "examples",
            ],
        ),
        HotspotsResult::File {
            refused: Some(true),
            refusal_reason,
            ..
        } => {
            print_refusal(refusal_reason.as_deref());
        }
        HotspotsResult::File { rows, .. } => print_section_table(
            "Top files by cumulative cost",
            "(no Read/Edit/Write tool calls)",
            rows.iter().take(limit).map(|f| file_row(f, 0.0)),
            &[
                "path",
                "firstTurn",
                "initial(tok)",
                "persist(tok)",
                "rideTurns",
                "cost",
                "%attr",
            ],
        ),
        HotspotsResult::Subagent {
            refused: Some(true),
            refusal_reason,
            ..
        } => {
            print_refusal(refusal_reason.as_deref());
        }
        HotspotsResult::Subagent { rows, .. } => print_section_table(
            "Top subagent calls by cost",
            "(no Agent/Task tool calls)",
            rows.iter().take(limit).map(subagent_row),
            &["subagent", "calls", "initial(tok)", "persist(tok)", "cost"],
        ),
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
        let mut rows: Vec<Vec<String>> =
            vec![vec!["severity".into(), "session".into(), "usd".into(), "title".into()]];
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

fn emit_human_attribution(a: &HotspotsAttributionResult, limit: usize) {
    let degraded = a.attribution_degraded;
    let approx_suffix = if degraded { " (approximate)" } else { "" };
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

    out.push(format!("Top files by cumulative cost{}", approx_suffix));
    if a.files.is_empty() {
        out.push("  (no Read/Edit/Write tool calls)".to_string());
    } else {
        let header: Vec<String> = vec![
            "path".into(),
            "firstTurn".into(),
            "initial(tok)".into(),
            "persist(tok)".into(),
            "rideTurns".into(),
            "cost".into(),
            "%attr".into(),
        ];
        let mut rows: Vec<Vec<String>> = vec![header];
        for f in a.files.iter().take(limit) {
            rows.push(file_row(f, a.attributed_total));
        }
        out.push(render_table(&rows));
    }
    out.push(String::new());

    out.push(format!("Top Bash verbs by cost{}", approx_suffix));
    if a.bash_verbs.is_empty() {
        out.push("  (no Bash tool calls)".to_string());
    } else {
        let header: Vec<String> = vec![
            "verb".into(),
            "calls".into(),
            "commands".into(),
            "initial(tok)".into(),
            "persist(tok)".into(),
            "avgRide".into(),
            "cost".into(),
            "examples".into(),
        ];
        let mut rows: Vec<Vec<String>> = vec![header];
        for b in a.bash_verbs.iter().take(limit) {
            rows.push(bash_verb_row(b));
        }
        out.push(render_table(&rows));
    }
    out.push(String::new());

    out.push(format!("Top exact Bash commands by cost{}", approx_suffix));
    if a.bash.is_empty() {
        out.push("  (no Bash tool calls)".to_string());
    } else {
        let header: Vec<String> = vec![
            "command".into(),
            "calls".into(),
            "initial(tok)".into(),
            "persist(tok)".into(),
            "cost".into(),
        ];
        let mut rows: Vec<Vec<String>> = vec![header];
        for b in a.bash.iter().take(limit) {
            rows.push(bash_row(b));
        }
        out.push(render_table(&rows));
    }
    out.push(String::new());

    out.push(format!("Top subagent calls by cost{}", approx_suffix));
    if a.subagents.is_empty() {
        out.push("  (no Agent/Task tool calls)".to_string());
    } else {
        let header: Vec<String> = vec![
            "subagent".into(),
            "calls".into(),
            "initial(tok)".into(),
            "persist(tok)".into(),
            "cost".into(),
        ];
        let mut rows: Vec<Vec<String>> = vec![header];
        for s in a.subagents.iter().take(limit) {
            rows.push(subagent_row(s));
        }
        out.push(render_table(&rows));
    }
    out.push(String::new());

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
        format_usd(s.total_cost),
    ]
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
