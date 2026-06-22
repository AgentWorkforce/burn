//! `burn compare <model_a,model_b[,...]>` — per-(model, activity) cost
//! comparison table. Thin presenter over the `LedgerHandle::compare` /
//! `compare_timeseries` SDK verbs; the full pipeline (query → provider filter
//! → fidelity gate → pricing → per-cell aggregation) lives in the SDK so the
//! MCP server can reuse it. This file only adapts the verb's `CompareResult`
//! into the JSON / CSV / TTY wire shapes.
//!
//! TS source of truth: `packages/cli/src/commands/compare.ts`. The wire
//! shape (cells ordering, rounding rules, fidelity-summary key order)
//! mirrors that file byte-for-byte against the cli-golden snapshot.
//!
//! ## Pre-query ingest decision
//!
//! Unlike `burn summary` and `burn hotspots`, this command intentionally does
//! NOT run a pre-query `ingest_all` sweep. Two reasons:
//!
//! 1. The JSON envelope above (`analyzedTurns`, `models`, `categories`,
//!    `cells`, `fidelity`) is byte-equivalent with the TS golden snapshot;
//!    bolting on an `ingest` block to mirror summary would break that.
//! 2. Compare is a presenter verb — answering "given my ledger, which model
//!    won?". Callers who want the freshest answer can chain
//!    `burn ingest && burn compare`; the steady-state setup is to run
//!    `burn ingest --watch` once per host so every read verb sees current data.
//!
//! Filter wiring: `--project` / `--session` / `--since` / `--workflow` /
//! `--agent` / `--provider` all flow into the verb's `CompareOptions`. The
//! verb lowers `--workflow` / `--agent` through stamp enrichment
//! (`workflowId` / `agentId` keys, both required when both flags are passed)
//! and applies `--provider` as a post-query CSV allow-list resolved via
//! `provider_for`, matching `summary --by-provider`'s synthetic-rule-aware
//! classification.

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, Result};
use relayburn_sdk::{
    normalize_since, CompareCellResult, CompareExcludedBreakdown, CompareOptions, CompareResult,
    FidelityClass, FidelitySummary, Ledger, LedgerOpenOptions, UsageGranularity,
    DEFAULT_MIN_SAMPLE,
};
use serde_json::{json, Value};

use crate::cli::{CompareArgs, GlobalArgs};
use crate::render::error::report_error;
use crate::render::format::{format_uint, format_usd};
use crate::render::json::render_json;
use crate::render::progress::TaskProgress;

const FIDELITY_CHOICES: &[&str] = &[
    "full",
    "usage-only",
    "aggregate-only",
    "cost-only",
    "partial",
];

const NEEDS_MODELS_MSG: &str =
    "compare: needs at least 2 models. Run `burn summary --by-provider` (or `burn summary --by-tool`) to see which models have data.";

const NOTE_LIMIT: usize = 8;
const DASH: &str = "—";

pub fn run(globals: &GlobalArgs, args: CompareArgs) -> i32 {
    match run_inner(globals, args) {
        Ok(code) => code,
        Err(e) => report_error(&e, globals),
    }
}

fn run_inner(globals: &GlobalArgs, args: CompareArgs) -> Result<i32> {
    // 1. Parse positional models list (comma-separated, dedup, preserve order).
    //    Argument-validation failures route through `report_error` (the outer
    //    `run` catches the `Err` from this function) so `--json` mode emits
    //    the documented `{"error": ...}` envelope instead of plain stderr.
    //    `report_error` prepends `burn: ` for human stderr, so the messages
    //    here read as the natural-language continuation (no leading `burn`).
    let raw = match args.models.as_deref() {
        Some(s) => s,
        None => {
            return Err(anyhow!("{NEEDS_MODELS_MSG}"));
        }
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut models: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let m = part.trim();
        if m.is_empty() {
            continue;
        }
        if seen.insert(m.to_string()) {
            models.push(m.to_string());
        }
    }
    if models.len() < 2 {
        return Err(anyhow!("{NEEDS_MODELS_MSG}"));
    }

    // 2. Resolve --fidelity / --include-partial.
    let mut min_fidelity: FidelityClass = FidelityClass::UsageOnly;
    if let Some(raw) = args.fidelity.as_deref() {
        if !FIDELITY_CHOICES.contains(&raw) {
            return Err(anyhow!(
                "invalid --fidelity: {raw} (expected one of {})",
                FIDELITY_CHOICES.join(", ")
            ));
        }
        min_fidelity = parse_fidelity(raw)?;
    }
    if args.include_partial {
        if let Some(raw) = args.fidelity.as_deref() {
            if raw != "partial" {
                return Err(anyhow!("--include-partial conflicts with --fidelity {raw}"));
            }
        }
        min_fidelity = FidelityClass::Partial;
    }

    // 3. JSON / CSV mutual exclusion. `--json` is a global flag; `--csv` is
    //    per-command so the global JSON take-precedence rule in the TS CLI
    //    becomes "explicit conflict" here — same exit code, same message.
    if globals.json && args.csv {
        return Err(anyhow!(
            "--json and --csv are mutually exclusive; pick one."
        ));
    }

    // `--bucket` opts into a per-bucket time-series. Parse it up front (before
    // the ledger is opened) so a bad duration routes through the same error
    // envelope as the other argument validations. CSV time-series rendering
    // isn't implemented, so reject the combination rather than silently
    // falling back to TTY output.
    let bucket_secs = args
        .bucket
        .as_deref()
        .map(relayburn_sdk::parse_bucket)
        .transpose()?;
    if bucket_secs.is_some() && args.csv {
        return Err(anyhow!(
            "--bucket and --csv are mutually exclusive; pick one."
        ));
    }

    // 4. Provider filter. Lower-cased CSV; turns whose effective provider
    //    (resolved inside the verb via `provider_for`) get dropped after the
    //    ledger query but before the fidelity gate, matching the TS pipeline
    //    order. Parsed here so a malformed `--provider` routes through the
    //    same error envelope as the other argument validations.
    let provider_filter = parse_provider_filter(args.provider.as_deref())?;

    // 5. min-sample.
    let min_sample = args.min_sample.unwrap_or(DEFAULT_MIN_SAMPLE);
    if min_sample < 1 {
        return Err(anyhow!("invalid --min-sample: {min_sample}"));
    }

    // 6. `--no-archive` is accepted for TS CLI flag parity but is a no-op:
    //    the Rust SDK is SQLite-native and has no archive layer to bypass.
    let _ = args.no_archive;

    // 7. Validate `--since` up front so a malformed duration routes through
    //    the same error envelope as the other argument validations. The verb
    //    re-parses it internally from `opts.since`; this is purely the
    //    early-fail gate (the parsed value is discarded).
    let _ = normalize_since(args.since.as_deref())?;

    // 8. Open ledger.
    let progress = TaskProgress::new(globals, "compare");
    let ledger_opts = match globals.ledger_path.as_deref() {
        Some(p) => LedgerOpenOptions::with_home(p),
        None => LedgerOpenOptions::default(),
    };
    progress.set_task("opening ledger");
    let handle = Ledger::open(ledger_opts)?;

    // `--bucket` switches to a per-bucket time-series via the SDK verb.
    // Parsing/validation already happened above, before the ledger was opened.
    if let Some(bucket_secs) = bucket_secs {
        progress.set_task("building comparison time-series");
        let series = handle
            .compare_timeseries(
                CompareOptions {
                    models: models.clone(),
                    session: args.session.clone(),
                    project: args.project.clone(),
                    since: args.since.clone(),
                    workflow: args.workflow.clone(),
                    agent: args.agent.clone(),
                    provider: provider_filter.clone(),
                    min_sample: Some(min_sample),
                    min_fidelity: Some(min_fidelity),
                    ledger_home: None,
                },
                bucket_secs,
            )
            .inspect_err(|_| progress.finish_and_clear())?;
        progress.finish_and_clear();
        if globals.json {
            render_json(&series)?;
            return Ok(0);
        }
        if series.buckets.is_empty() {
            println!("(no data in range)");
            return Ok(0);
        }
        for bucket in &series.buckets {
            println!(
                "{}  {:>5} turns  {} models",
                bucket.start,
                bucket.result.analyzed_turns,
                bucket.result.models.len(),
            );
        }
        return Ok(0);
    }

    // 9. Run the comparison through the SDK verb. The verb owns the full
    //    pipeline — query → provider filter (`provider_for`) → fidelity
    //    summary (over the post-provider, pre-gate slice) → fidelity gate →
    //    pricing → per-cell aggregation — so the CLI is a pure presenter over
    //    the returned `CompareResult`.
    progress.set_task("building comparison");
    let result = handle
        .compare(CompareOptions {
            models: models.clone(),
            session: args.session.clone(),
            project: args.project.clone(),
            since: args.since.clone(),
            workflow: args.workflow.clone(),
            agent: args.agent.clone(),
            provider: provider_filter,
            min_sample: Some(min_sample),
            min_fidelity: Some(min_fidelity),
            ledger_home: None,
        })
        .inspect_err(|_| progress.finish_and_clear())?;
    progress.finish_and_clear();

    // 10. Render.
    if globals.json {
        let v = build_json(&result);
        render_json(&v)?;
        return Ok(0);
    }
    if args.csv {
        let csv = render_csv(&result);
        print!("{csv}");
        return Ok(0);
    }
    let tty = render_tty(&result);
    print!("{tty}");
    Ok(0)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Parse `--provider` CSV → lower-cased allow-list passed to the verb's
/// `CompareOptions.provider`. Mirrors the `summary --provider` parser: trim
/// entries, drop empties, lower-case for case-insensitive matches, and reject
/// an all-empty list with the same error shape
/// (`burn: --provider requires a value`). Returns `Ok(None)` when the flag
/// wasn't passed. The verb re-normalizes (trim / lowercase / dedupe)
/// internally, so this only needs to reject the all-empty case here to keep
/// the argument-validation error envelope.
fn parse_provider_filter(raw: Option<&str>) -> Result<Option<Vec<String>>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let providers: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if providers.is_empty() {
        return Err(anyhow!("--provider requires a value"));
    }
    Ok(Some(providers))
}

fn parse_fidelity(s: &str) -> Result<FidelityClass> {
    match s {
        "full" => Ok(FidelityClass::Full),
        "usage-only" => Ok(FidelityClass::UsageOnly),
        "aggregate-only" => Ok(FidelityClass::AggregateOnly),
        "cost-only" => Ok(FidelityClass::CostOnly),
        "partial" => Ok(FidelityClass::Partial),
        other => Err(anyhow!("invalid fidelity class: {other}")),
    }
}

// ---------------------------------------------------------------------------
// number formatting (matches packages/cli/src/format.ts)
// ---------------------------------------------------------------------------

fn format_pct(rate: f64) -> String {
    // `Math.round(p * 100)` — round half to even on f64; matches JS for
    // the corpus we compare against (the `Math.round` half-to-even
    // exception is below the 1e-9 precision we care about here).
    let pct = (rate * 100.0).round() as i64;
    format!("{pct}%")
}

/// `Number(n.toFixed(d))` — produce the shortest decimal string for the
/// rounded value. Drops trailing zeros, mirroring JS `Number(...).toString()`.
fn to_fixed(n: f64, digits: usize) -> String {
    let s = format!("{n:.*}", digits);
    // For "0.00" / "1.00" → strip the trailing zeros, but keep at least
    // the integer part. Mirrors JS: `Number("1.00").toString() === "1"`.
    trim_trailing_zeros(&s)
}

fn trim_trailing_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// rounding for JSON output (Number(n.toFixed(d)))
// ---------------------------------------------------------------------------

/// JSON-friendly rounded number. Returns a `serde_json::Value::Number`
/// that prints without trailing zeros — matches `JSON.stringify(Number(n.toFixed(d)))`.
/// Whole-number results render as integers (`1`, not `1.0`); fractional
/// results render as the shortest decimal needed.
fn round_json(n: f64, digits: usize) -> Value {
    let s = format!("{n:.*}", digits);
    let parsed: f64 = s.parse().unwrap_or(0.0);
    f64_to_json(parsed)
}

/// Serialize an f64 with JS `JSON.stringify` semantics: integral values
/// render as integers, fractional values render via Ryu.
fn f64_to_json(n: f64) -> Value {
    if n.is_nan() || n.is_infinite() {
        // Match JS: NaN / Infinity become `null` in JSON.
        return Value::Null;
    }
    if n == 0.0 {
        // Both +0.0 and -0.0 become 0.
        return Value::from(0u64);
    }
    if n.fract() == 0.0 && n.abs() < (i64::MAX as f64) {
        return Value::from(n as i64);
    }
    // `serde_json::Number::from_f64` always emits a JSON number; the
    // pretty-printer uses Ryu's shortest representation for finite f64.
    Value::from(n)
}

/// Like `f64_to_json` but for `Option<f64>` — `None` → `null`.
fn opt_f64_to_json(n: Option<f64>) -> Value {
    match n {
        Some(v) => f64_to_json(v),
        None => Value::Null,
    }
}

/// Like `round_json` but for `Option<f64>`.
fn round_opt(n: Option<f64>, digits: usize) -> Value {
    match n {
        Some(v) => round_json(v, digits),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// cell lookup helpers
// ---------------------------------------------------------------------------

/// Empty-cell stand-in for `(model, category)` pairs the verb didn't emit.
/// `build_compare_table` seeds every pair, so in practice this is only a
/// defensive fallback (mirrors the analyze-layer `empty_cell`).
fn empty_cell(model: &str, category: &str) -> CompareCellResult {
    CompareCellResult {
        model: model.to_string(),
        category: category.to_string(),
        turns: 0,
        edit_turns: 0,
        one_shot_turns: 0,
        priced_turns: 0,
        total_cost: 0.0,
        cost_per_turn: None,
        one_shot_rate: None,
        cache_hit_rate: None,
        median_retries: None,
        no_data: true,
        insufficient_sample: false,
    }
}

/// Index the verb's flat `cells` vec by `(model, category)` so the renderers
/// can look up the same per-cell data the nested analyze-layer `CompareTable`
/// used to provide.
fn index_cells(result: &CompareResult) -> HashMap<(&str, &str), &CompareCellResult> {
    result
        .cells
        .iter()
        .map(|c| ((c.model.as_str(), c.category.as_str()), c))
        .collect()
}

// ---------------------------------------------------------------------------
// JSON envelope
// ---------------------------------------------------------------------------

fn build_json(result: &CompareResult) -> Value {
    let index = index_cells(result);
    // Cells in (model × category) iteration order; matches the TS
    // `for m of models / for cat of categories` walk.
    let mut cells: Vec<Value> = Vec::with_capacity(result.models.len() * result.categories.len());
    for m in &result.models {
        for cat in &result.categories {
            let owned;
            let c = match index.get(&(m.as_str(), cat.as_str())) {
                Some(c) => *c,
                None => {
                    owned = empty_cell(m, cat);
                    &owned
                }
            };
            cells.push(json!({
                "model": m,
                "category": cat,
                "turns": c.turns,
                "editTurns": c.edit_turns,
                "oneShotTurns": c.one_shot_turns,
                "pricedTurns": c.priced_turns,
                "totalCost": round_json(c.total_cost, 6),
                "costPerTurn": round_opt(c.cost_per_turn, 6),
                "oneShotRate": round_opt(c.one_shot_rate, 4),
                "cacheHitRate": round_opt(c.cache_hit_rate, 4),
                "medianRetries": opt_f64_to_json(c.median_retries),
                "noData": c.no_data,
                "insufficientSample": c.insufficient_sample,
            }));
        }
    }

    // `totals` keys must come out in `models` order (the TS `Object`
    // preserves insertion order). Build with a serde_json::Map so the
    // `preserve_order` feature on serde_json keeps insertion order.
    let mut totals = serde_json::Map::new();
    for m in &result.models {
        let totals_for = result.totals.get(m);
        let (turns, total_cost) = totals_for
            .map(|t| (t.turns, t.total_cost))
            .unwrap_or((0, 0.0));
        totals.insert(
            m.clone(),
            json!({
                "turns": turns,
                "totalCost": f64_to_json(total_cost),
            }),
        );
    }

    let excluded = &result.fidelity.excluded;
    json!({
        "analyzedTurns": result.analyzed_turns,
        "minSample": result.min_sample,
        "models": &result.models,
        "categories": &result.categories,
        "totals": Value::Object(totals),
        "cells": cells,
        "fidelity": {
            "minimum": result.fidelity.minimum.wire_str(),
            "excluded": {
                "total": excluded.total,
                "aggregateOnly": excluded.aggregate_only,
                "costOnly": excluded.cost_only,
                "partial": excluded.partial,
                "usageOnly": excluded.usage_only,
            },
            "summary": fidelity_summary_to_value(&result.fidelity.summary),
        }
    })
}

/// Build the fidelity-summary JSON sub-object with the same key order
/// the TS path emits (literal `{ full, usage-only, aggregate-only,
/// cost-only, partial }` order, preserved via serde_json's
/// `preserve_order` feature).
fn fidelity_summary_to_value(s: &FidelitySummary) -> Value {
    let mut by_class = serde_json::Map::new();
    for key in &[
        "full",
        "usage-only",
        "aggregate-only",
        "cost-only",
        "partial",
    ] {
        let cls = parse_fidelity(key).unwrap();
        let n = s.by_class.get(&cls).copied().unwrap_or(0);
        by_class.insert((*key).to_string(), Value::from(n));
    }
    let mut by_granularity = serde_json::Map::new();
    for key in &[
        "per-turn",
        "per-message",
        "per-session-aggregate",
        "cost-only",
    ] {
        let g = match *key {
            "per-turn" => UsageGranularity::PerTurn,
            "per-message" => UsageGranularity::PerMessage,
            "per-session-aggregate" => UsageGranularity::PerSessionAggregate,
            "cost-only" => UsageGranularity::CostOnly,
            _ => unreachable!(),
        };
        let n = s.by_granularity.get(&g).copied().unwrap_or(0);
        by_granularity.insert((*key).to_string(), Value::from(n));
    }
    // missingCoverage: keys are camelCase; iterate in the same fixed order
    // the TS `emptyFidelitySummary()` literal uses so JSON shape is stable.
    let coverage_keys = &[
        "hasInputTokens",
        "hasOutputTokens",
        "hasReasoningTokens",
        "hasCacheReadTokens",
        "hasCacheCreateTokens",
        "hasToolCalls",
        "hasToolResultEvents",
        "hasSessionRelationships",
        "hasRawContent",
    ];
    let mut missing = serde_json::Map::new();
    for k in coverage_keys {
        let n = s.missing_coverage.get(*k).copied().unwrap_or(0);
        missing.insert((*k).to_string(), Value::from(n));
    }

    let mut out = serde_json::Map::new();
    out.insert("total".to_string(), Value::from(s.total));
    out.insert("byClass".to_string(), Value::Object(by_class));
    out.insert("byGranularity".to_string(), Value::Object(by_granularity));
    out.insert("missingCoverage".to_string(), Value::Object(missing));
    out.insert("unknown".to_string(), Value::from(s.unknown));
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

fn render_csv(result: &CompareResult) -> String {
    let index = index_cells(result);
    let header = [
        "model",
        "category",
        "turns",
        "editTurns",
        "oneShotTurns",
        "pricedTurns",
        "totalCost",
        "costPerTurn",
        "oneShotRate",
        "cacheHitRate",
        "medianRetries",
        "noData",
        "insufficientSample",
    ];
    let mut rows: Vec<String> = Vec::new();
    rows.push(header.join(","));
    for m in &result.models {
        for cat in &result.categories {
            let owned;
            let c = match index.get(&(m.as_str(), cat.as_str())) {
                Some(c) => *c,
                None => {
                    owned = empty_cell(m, cat);
                    &owned
                }
            };
            let row = vec![
                csv_cell(m),
                csv_cell(cat),
                c.turns.to_string(),
                c.edit_turns.to_string(),
                c.one_shot_turns.to_string(),
                c.priced_turns.to_string(),
                num_csv(c.total_cost, 6),
                c.cost_per_turn.map(|v| num_csv(v, 6)).unwrap_or_default(),
                c.one_shot_rate.map(|v| num_csv(v, 4)).unwrap_or_default(),
                c.cache_hit_rate.map(|v| num_csv(v, 4)).unwrap_or_default(),
                c.median_retries
                    .map(|v| {
                        // `String(n)` for numbers; JS prints integers as-is.
                        if v.fract() == 0.0 {
                            (v as i64).to_string()
                        } else {
                            v.to_string()
                        }
                    })
                    .unwrap_or_default(),
                if c.no_data { "true" } else { "false" }.to_string(),
                if c.insufficient_sample {
                    "true"
                } else {
                    "false"
                }
                .to_string(),
            ];
            rows.push(row.join(","));
        }
    }
    format!("{}\n", rows.join("\n"))
}

fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn num_csv(n: f64, digits: usize) -> String {
    to_fixed(n, digits)
}

// ---------------------------------------------------------------------------
// TTY
// ---------------------------------------------------------------------------

fn cell_fields(c: &CompareCellResult) -> [String; 3] {
    if c.no_data {
        return [DASH.to_string(), DASH.to_string(), DASH.to_string()];
    }
    let turns = format_uint(c.turns);
    let cost = c
        .cost_per_turn
        .map(format_usd)
        .unwrap_or_else(|| DASH.to_string());
    let one_shot = c
        .one_shot_rate
        .map(format_pct)
        .unwrap_or_else(|| DASH.to_string());
    [turns, cost, one_shot]
}

fn render_tty(result: &CompareResult) -> String {
    let minimum = result.fidelity.minimum;
    let index = index_cells(result);
    // `(model, category)` lookup with an empty-cell fallback for pairs the
    // verb didn't emit (it always emits the full grid, so this is defensive).
    let cell_for = |m: &str, cat: &str| -> CompareCellResult {
        index
            .get(&(m, cat))
            .map(|c| (*c).clone())
            .unwrap_or_else(|| empty_cell(m, cat))
    };

    let mut lines: Vec<String> = Vec::new();
    lines.push(String::new());
    lines.push(format!(
        "turns analyzed: {}",
        format_uint(result.analyzed_turns)
    ));

    let excluded = &result.fidelity.excluded;
    if excluded.total > 0 {
        lines.push(format_excluded_note(excluded, minimum));
    }
    lines.push(String::new());

    if result.models.is_empty() || result.categories.is_empty() {
        lines
            .push("no data to compare (need turns spanning ≥1 model and ≥1 activity).".to_string());
        lines.push(String::new());
        return lines.join("\n");
    }

    let sub_header = build_sub_header(&result.models);

    let mut data_rows: Vec<Vec<String>> = Vec::new();
    for cat in &result.categories {
        let mut row: Vec<String> = vec![cat.clone()];
        for m in &result.models {
            let cell = cell_for(m, cat);
            let [a, b, c] = cell_fields(&cell);
            row.push(a);
            row.push(b);
            row.push(c);
        }
        data_rows.push(row);
    }

    let mut widths = vec![0usize; sub_header.len()];
    for row in std::iter::once(&sub_header).chain(data_rows.iter()) {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(display_width(cell));
        }
    }

    const SEP: &str = "  ";

    // Widen the last column of each model's group to fit the (possibly
    // longer) display name. Mirrors the TS path's group-line padding.
    for mi in 0..result.models.len() {
        let start = 1 + mi * 3;
        let group_width =
            widths[start] + SEP.len() + widths[start + 1] + SEP.len() + widths[start + 2];
        let name = display_model_name(&result.models[mi]);
        let name_w = display_width(name);
        if name_w > group_width {
            widths[start + 2] += name_w - group_width;
        }
    }

    // Group-name line.
    let mut group_line: Vec<String> = vec![pad_end("", widths[0])];
    for mi in 0..result.models.len() {
        let start = 1 + mi * 3;
        let group_width =
            widths[start] + SEP.len() + widths[start + 1] + SEP.len() + widths[start + 2];
        let name = display_model_name(&result.models[mi]);
        group_line.push(pad_end(name, group_width));
    }
    lines.push(rstrip(&group_line.join(SEP)));

    // Sub-header.
    lines.push(render_row(&sub_header, &widths, SEP));

    // Data rows.
    for row in &data_rows {
        lines.push(render_row(row, &widths, SEP));
    }

    // Coverage notes.
    let mut notes: Vec<String> = Vec::new();
    for cat in &result.categories {
        let any_has_data = result.models.iter().any(|m| !cell_for(m, cat).no_data);
        if !any_has_data {
            continue;
        }
        for m in &result.models {
            let cell = cell_for(m, cat);
            if cell.no_data {
                notes.push(format!(
                    "no {} data in '{cat}' — no comparison available.",
                    display_model_name(m)
                ));
            } else if cell.insufficient_sample {
                notes.push(format!(
                    "low {} sample in '{cat}' ({} turns < {}) — treat as indicative.",
                    display_model_name(m),
                    cell.turns,
                    result.min_sample
                ));
            }
        }
    }
    if !notes.is_empty() {
        lines.push(String::new());
        let shown = notes.iter().take(NOTE_LIMIT);
        for n in shown {
            lines.push(format!("  {n}"));
        }
        if notes.len() > NOTE_LIMIT {
            lines.push(format!(
                "  … and {} more coverage gaps.",
                notes.len() - NOTE_LIMIT
            ));
        }
    }

    // Per-model totals.
    lines.push(String::new());
    for m in &result.models {
        let (turns, total_cost_raw) = result
            .totals
            .get(m)
            .map(|t| (t.turns, t.total_cost))
            .unwrap_or((0, 0.0));
        let total_cost = if turns > 0 {
            format_usd(total_cost_raw)
        } else {
            DASH.to_string()
        };
        lines.push(format!(
            "{}: {} turns, {} total",
            display_model_name(m),
            format_uint(turns),
            total_cost
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn build_sub_header(models: &[String]) -> Vec<String> {
    let mut row: Vec<String> = vec!["Activity".to_string()];
    for _ in models {
        row.push("Turns".to_string());
        row.push("Cost/turn".to_string());
        row.push("1-shot".to_string());
    }
    row
}

fn render_row(row: &[String], widths: &[usize], sep: &str) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(row.len());
    for (i, cell) in row.iter().enumerate() {
        parts.push(pad_end(cell, widths[i]));
    }
    rstrip(&parts.join(sep))
}

fn pad_end(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    let pad = " ".repeat(width - w);
    format!("{s}{pad}")
}

fn rstrip(s: &str) -> String {
    s.trim_end_matches(' ').to_string()
}

/// `String.length` in JS counts UTF-16 code units, but for the corpus
/// this CLI ships against (ASCII model names, ASCII activity labels),
/// `chars().count()` is byte-equivalent. We use it instead of byte length
/// to keep the dash sentinel (`—`, U+2014, 3 bytes UTF-8 / 1 UTF-16
/// unit) aligning the way the TS path expects.
fn display_width(s: &str) -> usize {
    s.chars().count()
}

fn display_model_name(m: &str) -> &str {
    match m.find('/') {
        Some(i) => &m[i + 1..],
        None => m,
    }
}

fn format_excluded_note(excluded: &CompareExcludedBreakdown, minimum: FidelityClass) -> String {
    let mut parts: Vec<String> = Vec::new();
    if excluded.aggregate_only > 0 {
        parts.push(format!("{} aggregate-only", excluded.aggregate_only));
    }
    if excluded.cost_only > 0 {
        parts.push(format!("{} cost-only", excluded.cost_only));
    }
    if excluded.partial > 0 {
        parts.push(format!("{} partial", excluded.partial));
    }
    if excluded.usage_only > 0 {
        parts.push(format!("{} usage-only", excluded.usage_only));
    }
    let breakdown = if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    };
    let noun = if excluded.total == 1 { "turn" } else { "turns" };
    format!(
        "excluded {} {noun} below {} fidelity{breakdown}",
        format_uint(excluded.total),
        minimum.wire_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_pct_rounds_to_int() {
        assert_eq!(format_pct(0.0), "0%");
        assert_eq!(format_pct(0.5), "50%");
        assert_eq!(format_pct(1.0), "100%");
        assert_eq!(format_pct(2.0 / 3.0), "67%");
    }

    #[test]
    fn round_json_matches_js_to_fixed() {
        // Whole numbers come out as integers (no `.0` suffix).
        let v = round_json(1.0, 4);
        assert_eq!(v.to_string(), "1");
        // Non-whole shorter than digit cap drops trailing zeros.
        let v = round_json(0.5, 4);
        assert_eq!(v.to_string(), "0.5");
        // Rounds to 6 digits.
        let v = round_json(0.0112499999, 6);
        assert_eq!(v.to_string(), "0.01125");
    }

    #[test]
    fn parse_provider_filter_trims_lowercases_and_drops_empties() {
        // The CLI parser trims / lowercases / drops empties; deduping is left
        // to the verb's `normalize_provider_filter`, so the raw entries
        // (including the repeat) flow through as a `Vec`.
        let got = parse_provider_filter(Some(" Anthropic,OPENAI ,, anthropic"))
            .unwrap()
            .unwrap();
        assert_eq!(got, vec!["anthropic", "openai", "anthropic"]);
    }

    #[test]
    fn parse_provider_filter_returns_none_when_flag_absent() {
        assert!(parse_provider_filter(None).unwrap().is_none());
    }

    #[test]
    fn parse_provider_filter_rejects_all_empty_input() {
        let err = parse_provider_filter(Some(" , ,, ")).unwrap_err();
        assert!(format!("{err}").contains("--provider requires a value"));
    }

    #[test]
    fn parse_fidelity_known_classes() {
        assert!(matches!(
            parse_fidelity("full").unwrap(),
            FidelityClass::Full
        ));
        assert!(matches!(
            parse_fidelity("usage-only").unwrap(),
            FidelityClass::UsageOnly
        ));
        assert!(parse_fidelity("nope").is_err());
    }

    #[test]
    fn display_model_name_strips_provider_prefix() {
        assert_eq!(
            display_model_name("anthropic/claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(display_model_name("claude-haiku-4-5"), "claude-haiku-4-5");
    }
}
