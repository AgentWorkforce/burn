//! `burn compare <model_a,model_b[,...]>` — per-(model, activity) cost
//! comparison table. Thin presenter over the
//! `relayburn_sdk::analyze::compare` building blocks (`build_compare_table`
//! plus `compare_from_archive`); the heavy lifting lives in the SDK so the
//! MCP server can reuse it.
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
//! Filter wiring: `--project` / `--session` / `--since` lower into the
//! `Query` struct directly; `--workflow` / `--agent` fold through
//! `Query.enrichment` (`workflowId` / `agentId` keys, both required when
//! both flags are passed); `--provider` is a post-query CSV allow-list
//! resolved via `provider_for`, matching `summary --by-provider`'s
//! synthetic-rule-aware classification.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, Result};
use relayburn_sdk::{
    build_compare_table, has_minimum_fidelity, load_pricing, provider_for, summarize_fidelity,
    AnalyzeCompareOptions as CompareOptions, CompareCell, CompareTable, EnrichedTurn,
    FidelityClass, FidelitySummary, Ledger, LedgerOpenOptions, ProviderFilter, Query,
    UsageGranularity, DEFAULT_MIN_SAMPLE,
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

const FIDELITY_ORDER: &[&str] = &[
    "cost-only",
    "aggregate-only",
    "partial",
    "usage-only",
    "full",
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
                return Err(anyhow!(
                    "--include-partial conflicts with --fidelity {raw}"
                ));
            }
        }
        min_fidelity = FidelityClass::Partial;
    }

    // 3. JSON / CSV mutual exclusion. `--json` is a global flag; `--csv` is
    //    per-command so the global JSON take-precedence rule in the TS CLI
    //    becomes "explicit conflict" here — same exit code, same message.
    if globals.json && args.csv {
        return Err(anyhow!("--json and --csv are mutually exclusive; pick one."));
    }

    // 4. Provider filter. Lower-cased CSV; turns whose effective provider
    //    (per `provider_for`) isn't in the set get dropped after the ledger
    //    query but before the fidelity gate, matching the TS pipeline order.
    let provider_filter = parse_provider_filter(args.provider.as_deref())?;

    // 5. min-sample.
    let min_sample = args.min_sample.unwrap_or(DEFAULT_MIN_SAMPLE);
    if min_sample < 1 {
        return Err(anyhow!("invalid --min-sample: {min_sample}"));
    }

    // 6. `--no-archive` is accepted for TS CLI flag parity but is a no-op:
    //    the Rust SDK is SQLite-native and has no archive layer to bypass.
    let _ = args.no_archive;

    // 7. Build the Query.
    let mut q = Query::default();
    if let Some(s) = normalize_since(args.since.as_deref())? {
        q.since = Some(s);
    }
    if let Some(p) = args.project.as_deref() {
        q.project = Some(p.to_string());
    }
    if let Some(s) = args.session.as_deref() {
        q.session_id = Some(s.to_string());
    }
    // `workflow` / `agent` fold through stamp enrichment. Both keys live in
    // the same `Enrichment` map (`workflowId`, `agentId`), and the ledger's
    // `Query.enrichment` predicate requires every key/value pair to match —
    // so passing both narrows to the intersection.
    let mut enrichment = BTreeMap::new();
    if let Some(workflow) = args.workflow.as_deref() {
        enrichment.insert("workflowId".to_string(), workflow.to_string());
    }
    if let Some(agent) = args.agent.as_deref() {
        enrichment.insert("agentId".to_string(), agent.to_string());
    }
    if !enrichment.is_empty() {
        q.enrichment = Some(enrichment);
    }

    // 8. Open ledger and walk turns.
    let progress = TaskProgress::new(globals, "compare");
    let ledger_opts = match globals.ledger_path.as_deref() {
        Some(p) => LedgerOpenOptions::with_home(p),
        None => LedgerOpenOptions::default(),
    };
    progress.set_task("opening ledger");
    let handle = Ledger::open(ledger_opts)?;
    progress.set_task("loading turns");
    let queried_turns: Vec<EnrichedTurn> = handle.raw().query_turns(&q)?;

    // 9. Drop turns whose effective provider isn't in the allow-list. The
    //    provider is resolved via `provider_for` (synthetic-rule first,
    //    then `provider/model` prefix, then collector-implied) so the CLI
    //    matches `summary --by-provider` semantics 1:1.
    let filtered_by_provider: Vec<EnrichedTurn> = match provider_filter.as_ref() {
        Some(filter) => queried_turns
            .into_iter()
            .filter(|et| {
                let provider = provider_for(&et.turn).provider;
                filter.contains(&provider.to_ascii_lowercase())
            })
            .collect(),
        None => queried_turns,
    };

    // 10. Fidelity summary is computed BEFORE the fidelity gate so the
    //     `summary` block in the JSON envelope reflects the queried slice.
    let fidelity_summary = summarize_fidelity(
        &filtered_by_provider
            .iter()
            .map(|et| et.turn.clone())
            .collect::<Vec<_>>(),
    );
    let filtered_turns: Vec<EnrichedTurn> = if matches!(min_fidelity, FidelityClass::Partial) {
        filtered_by_provider
    } else {
        filtered_by_provider
            .into_iter()
            .filter(|et| has_minimum_fidelity(et.turn.fidelity.as_ref(), min_fidelity))
            .collect()
    };
    let analyzed_turns = filtered_turns.len();

    // 11. Build the compare table.
    let pricing = load_pricing(None);
    let opts = CompareOptions {
        pricing: &pricing,
        models: Some(models.clone()),
        min_sample: Some(min_sample),
    };
    progress.set_task("building comparison");
    let table = build_compare_table(&filtered_turns, &opts);
    progress.finish_and_clear();

    // 12. Render.
    if globals.json {
        let v = build_json(&table, analyzed_turns, min_fidelity, &fidelity_summary);
        render_json(&v)?;
        return Ok(0);
    }
    if args.csv {
        let csv = render_csv(&table);
        print!("{csv}");
        return Ok(0);
    }
    let tty = render_tty(
        &table,
        analyzed_turns,
        min_fidelity,
        &fidelity_summary,
    );
    print!("{tty}");
    Ok(0)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Parse `--provider` CSV → lower-cased `ProviderFilter`. Mirrors the
/// `summary --provider` parser: trim entries, drop empties, lower-case for
/// case-insensitive matches, and reject an all-empty list with the same
/// error shape (`burn: --provider requires a value`). Returns `Ok(None)`
/// when the flag wasn't passed.
fn parse_provider_filter(raw: Option<&str>) -> Result<Option<ProviderFilter>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let providers: ProviderFilter = raw
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

/// Normalize `--since` exactly like the TS CLI's `parseSinceArg` does:
///
/// - Relative ranges (`7d`, `24h`, `4w`, `30m`) → `now - delta` rendered
///   as a fully canonical UTC ISO string with milliseconds
///   (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
/// - ISO inputs (with or without an offset, with or without fractional
///   seconds) get parsed and re-rendered as UTC `...Z` with milliseconds.
///   This matters because `Ledger::query_turns` applies `since` via
///   lexicographic comparison against stored `...mmmZ` timestamps:
///     * an offset like `2026-05-06T00:00:00-07:00` would otherwise sort
///       before any ledger row regardless of the actual instant, and
///     * a no-fraction `...12Z` would sort before `...12.500Z` even
///       though `...12.500Z` is a later instant.
///   Re-emitting as UTC with `.000Z` (or the original sub-second
///   precision) keeps the lex order stable against the ledger.
/// - Garbage → error.
fn normalize_since(since: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = since else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    if let Some((n, unit)) = parse_relative(raw) {
        let secs_back = match unit {
            'h' => n * 3_600,
            'd' => n * 86_400,
            'w' => n * 7 * 86_400,
            'm' => n * 30 * 86_400,
            _ => unreachable!(),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let when = now.saturating_sub(secs_back);
        return Ok(Some(format_iso_z_ms(when, 0)));
    }
    if let Some(canonical) = normalize_iso_to_utc_z(raw) {
        return Ok(Some(canonical));
    }
    Err(anyhow!(
        "invalid since: {raw} (expected ISO timestamp or relative range like 7d)"
    ))
}

fn parse_relative(s: &str) -> Option<(u64, char)> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let unit = bytes[bytes.len() - 1] as char;
    if !matches!(unit, 'h' | 'd' | 'w' | 'm') {
        return None;
    }
    let num = &s[..s.len() - 1];
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = num.parse().ok()?;
    Some((n, unit))
}

/// Parse an ISO 8601 / RFC 3339 timestamp and re-emit it as a fully
/// canonical UTC `YYYY-MM-DDTHH:MM:SS.mmmZ` string. Handles:
///
/// - `YYYY-MM-DD` (date-only — assumed midnight UTC).
/// - `YYYY-MM-DDTHH:MM:SS` (offset-less — assumed UTC).
/// - `YYYY-MM-DDTHH:MM:SS.fff` (fractional seconds, any width 1–9).
/// - `Z` suffix.
/// - `+HH:MM` / `-HH:MM` offsets.
///
/// Returns `None` for inputs that don't look ISO-shaped, so the caller
/// can surface a usage error. Emits with millisecond precision: any
/// sub-millisecond fractional digits are truncated, matching JS
/// `Date.toISOString()` rounding behavior closely enough for ledger
/// `since` lex-ordering. Whole-second inputs are widened to `.000Z`.
fn normalize_iso_to_utc_z(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    // YYYY-MM-DD prefix.
    if !(bytes[0..4].iter().all(|c| c.is_ascii_digit())
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(|c| c.is_ascii_digit())
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Defaults for date-only inputs.
    let mut hour: u32 = 0;
    let mut minute: u32 = 0;
    let mut second: u32 = 0;
    let mut millis: u32 = 0;
    let mut offset_minutes: i32 = 0;

    if bytes.len() > 10 {
        // Expect a time component starting with 'T' or ' '.
        if !(bytes[10] == b'T' || bytes[10] == b't' || bytes[10] == b' ') {
            return None;
        }
        // HH:MM:SS at offsets 11..19.
        if bytes.len() < 19 {
            return None;
        }
        if !(bytes[11..13].iter().all(|c| c.is_ascii_digit())
            && bytes[13] == b':'
            && bytes[14..16].iter().all(|c| c.is_ascii_digit())
            && bytes[16] == b':'
            && bytes[17..19].iter().all(|c| c.is_ascii_digit()))
        {
            return None;
        }
        hour = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
        minute = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
        second = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;
        if hour > 23 || minute > 59 || second > 60 {
            return None;
        }

        // Optional fractional seconds.
        let mut idx = 19;
        if idx < bytes.len() && (bytes[idx] == b'.' || bytes[idx] == b',') {
            idx += 1;
            let frac_start = idx;
            while idx < bytes.len() && bytes[idx].is_ascii_digit() {
                idx += 1;
            }
            if idx == frac_start {
                return None;
            }
            // Truncate to milliseconds: take the first 3 digits, pad
            // with zeros if shorter, ignore the rest.
            let mut frac_str = String::from(std::str::from_utf8(&bytes[frac_start..idx]).ok()?);
            if frac_str.len() > 3 {
                frac_str.truncate(3);
            }
            while frac_str.len() < 3 {
                frac_str.push('0');
            }
            millis = frac_str.parse().ok()?;
        }

        // Optional offset.
        if idx < bytes.len() {
            match bytes[idx] {
                b'Z' | b'z' => {
                    if idx + 1 != bytes.len() {
                        return None;
                    }
                }
                b'+' | b'-' => {
                    let sign: i32 = if bytes[idx] == b'-' { -1 } else { 1 };
                    idx += 1;
                    if bytes.len() < idx + 5 {
                        return None;
                    }
                    if !(bytes[idx..idx + 2].iter().all(|c| c.is_ascii_digit())
                        && bytes[idx + 2] == b':'
                        && bytes[idx + 3..idx + 5].iter().all(|c| c.is_ascii_digit()))
                    {
                        return None;
                    }
                    let oh: i32 = std::str::from_utf8(&bytes[idx..idx + 2])
                        .ok()?
                        .parse()
                        .ok()?;
                    let om: i32 = std::str::from_utf8(&bytes[idx + 3..idx + 5])
                        .ok()?
                        .parse()
                        .ok()?;
                    if oh > 23 || om > 59 {
                        return None;
                    }
                    offset_minutes = sign * (oh * 60 + om);
                    if idx + 5 != bytes.len() {
                        return None;
                    }
                }
                _ => return None,
            }
        }
    }

    // Convert (year, month, day, h, m, s, offset) → unix seconds.
    let days = ymd_to_days(year, month, day)?;
    let local_secs: i64 = days * 86_400 + (hour as i64) * 3_600 + (minute as i64) * 60 + (second as i64);
    // Subtract the offset to land on UTC seconds: `local = utc + offset`,
    // so `utc = local - offset`. Offset is in minutes.
    let utc_secs: i64 = local_secs - (offset_minutes as i64) * 60;
    Some(format_iso_z_ms_signed(utc_secs, millis))
}

/// Format Unix-seconds as `YYYY-MM-DDTHH:MM:SS.mmmZ`. Always emits the
/// milliseconds component so the resulting string sorts correctly against
/// ledger rows that always carry sub-second precision.
fn format_iso_z_ms(secs: u64, millis: u32) -> String {
    format_iso_z_ms_signed(secs as i64, millis)
}

fn format_iso_z_ms_signed(secs: i64, millis: u32) -> String {
    // `secs` may be negative for pre-1970 timestamps — split into a
    // floored day count and a non-negative seconds-in-day remainder so
    // the formatting math doesn't have to care about sign.
    let total_days = secs.div_euclid(86_400);
    let secs_in_day = secs.rem_euclid(86_400) as u32;
    let hour = secs_in_day / 3_600;
    let minute = (secs_in_day / 60) % 60;
    let second = secs_in_day % 60;
    let (year, month, day) = days_to_ymd(total_days);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    )
}

/// Civil-date → days-from-Unix-epoch (Howard Hinnant's algorithm,
/// proleptic Gregorian). Inverse of [`days_to_ymd`].
fn ymd_to_days(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let m = month as i64;
    let d = day as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + (d as u64) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + (doe as i64) - 719_468)
}

fn days_to_ymd(days_from_epoch: i64) -> (i64, u32, u32) {
    let z = days_from_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
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
// CompareExcludedBreakdown
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ExcludedBreakdown {
    total: u64,
    aggregate_only: u64,
    cost_only: u64,
    partial: u64,
    usage_only: u64,
}

fn compute_excluded(summary: &FidelitySummary, minimum: FidelityClass) -> ExcludedBreakdown {
    let mut out = ExcludedBreakdown::default();
    if matches!(minimum, FidelityClass::Partial) {
        return out;
    }
    let need = FIDELITY_ORDER
        .iter()
        .position(|c| *c == minimum.wire_str())
        .unwrap_or(0);
    for (i, key) in FIDELITY_ORDER.iter().enumerate() {
        if i >= need {
            continue;
        }
        let cls = parse_fidelity(key).unwrap();
        let n = summary.by_class.get(&cls).copied().unwrap_or(0);
        if n == 0 {
            continue;
        }
        out.total += n;
        match *key {
            "aggregate-only" => out.aggregate_only += n,
            "cost-only" => out.cost_only += n,
            "partial" => out.partial += n,
            "usage-only" => out.usage_only += n,
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// JSON envelope
// ---------------------------------------------------------------------------

fn build_json(
    table: &CompareTable,
    analyzed_turns: usize,
    minimum: FidelityClass,
    summary: &FidelitySummary,
) -> Value {
    let excluded = compute_excluded(summary, minimum);
    // Cells in (model × category) iteration order; matches the TS
    // `for m of models / for cat of categories` walk.
    let mut cells: Vec<Value> = Vec::with_capacity(table.models.len() * table.categories.len());
    for m in &table.models {
        for cat in &table.categories {
            let c = table
                .cells
                .get(m)
                .and_then(|by_cat| by_cat.get(cat))
                .cloned()
                .unwrap_or_else(empty_cell);
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
    for m in &table.models {
        let totals_for = table.totals.get(m).cloned().unwrap_or_default();
        totals.insert(
            m.clone(),
            json!({
                "turns": totals_for.turns,
                "totalCost": f64_to_json(totals_for.total_cost),
            }),
        );
    }

    json!({
        "analyzedTurns": analyzed_turns,
        "minSample": table.min_sample,
        "models": &table.models,
        "categories": &table.categories,
        "totals": Value::Object(totals),
        "cells": cells,
        "fidelity": {
            "minimum": minimum.wire_str(),
            "excluded": {
                "total": excluded.total,
                "aggregateOnly": excluded.aggregate_only,
                "costOnly": excluded.cost_only,
                "partial": excluded.partial,
                "usageOnly": excluded.usage_only,
            },
            "summary": fidelity_summary_to_value(summary),
        }
    })
}

/// Build the fidelity-summary JSON sub-object with the same key order
/// the TS path emits (literal `{ full, usage-only, aggregate-only,
/// cost-only, partial }` order, preserved via serde_json's
/// `preserve_order` feature).
fn fidelity_summary_to_value(s: &FidelitySummary) -> Value {
    let mut by_class = serde_json::Map::new();
    for key in &["full", "usage-only", "aggregate-only", "cost-only", "partial"] {
        let cls = parse_fidelity(key).unwrap();
        let n = s.by_class.get(&cls).copied().unwrap_or(0);
        by_class.insert((*key).to_string(), Value::from(n));
    }
    let mut by_granularity = serde_json::Map::new();
    for key in &["per-turn", "per-message", "per-session-aggregate", "cost-only"] {
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

fn empty_cell() -> CompareCell {
    CompareCell {
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

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

fn render_csv(table: &CompareTable) -> String {
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
    for m in &table.models {
        for cat in &table.categories {
            let c = table
                .cells
                .get(m)
                .and_then(|by_cat| by_cat.get(cat))
                .cloned()
                .unwrap_or_else(empty_cell);
            let row = vec![
                csv_cell(m),
                csv_cell(cat),
                c.turns.to_string(),
                c.edit_turns.to_string(),
                c.one_shot_turns.to_string(),
                c.priced_turns.to_string(),
                num_csv(c.total_cost, 6),
                c.cost_per_turn
                    .map(|v| num_csv(v, 6))
                    .unwrap_or_default(),
                c.one_shot_rate
                    .map(|v| num_csv(v, 4))
                    .unwrap_or_default(),
                c.cache_hit_rate
                    .map(|v| num_csv(v, 4))
                    .unwrap_or_default(),
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

fn cell_fields(c: &CompareCell) -> [String; 3] {
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

fn render_tty(
    table: &CompareTable,
    analyzed_turns: usize,
    minimum: FidelityClass,
    summary: &FidelitySummary,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(String::new());
    lines.push(format!("turns analyzed: {}", format_uint(analyzed_turns as u64)));

    let excluded = compute_excluded(summary, minimum);
    if excluded.total > 0 {
        lines.push(format_excluded_note(&excluded, minimum));
    }
    lines.push(String::new());

    if table.models.is_empty() || table.categories.is_empty() {
        lines.push(
            "no data to compare (need turns spanning ≥1 model and ≥1 activity).".to_string(),
        );
        lines.push(String::new());
        return lines.join("\n");
    }

    let sub_header = build_sub_header(&table.models);

    let owned_empty = empty_cell();
    let cell_for = |m: &str, cat: &str| -> CompareCell {
        table
            .cells
            .get(m)
            .and_then(|by| by.get(cat))
            .cloned()
            .unwrap_or_else(empty_cell)
    };
    // Suppress the unused-variable warning on `owned_empty`; it's only
    // referenced when we run a corner case where neither cells.get nor
    // by_cat.get is hit, which the table builder doesn't produce today.
    let _ = &owned_empty;

    let mut data_rows: Vec<Vec<String>> = Vec::new();
    for cat in &table.categories {
        let mut row: Vec<String> = vec![cat.clone()];
        for m in &table.models {
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
    for mi in 0..table.models.len() {
        let start = 1 + mi * 3;
        let group_width =
            widths[start] + SEP.len() + widths[start + 1] + SEP.len() + widths[start + 2];
        let name = display_model_name(&table.models[mi]);
        let name_w = display_width(name);
        if name_w > group_width {
            widths[start + 2] += name_w - group_width;
        }
    }

    // Group-name line.
    let mut group_line: Vec<String> = vec![pad_end("", widths[0])];
    for mi in 0..table.models.len() {
        let start = 1 + mi * 3;
        let group_width =
            widths[start] + SEP.len() + widths[start + 1] + SEP.len() + widths[start + 2];
        let name = display_model_name(&table.models[mi]);
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
    for cat in &table.categories {
        let any_has_data = table
            .models
            .iter()
            .any(|m| !cell_for(m, cat).no_data);
        if !any_has_data {
            continue;
        }
        for m in &table.models {
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
                    table.min_sample
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
    for m in &table.models {
        let tot = table.totals.get(m).cloned().unwrap_or_default();
        let total_cost = if tot.turns > 0 {
            format_usd(tot.total_cost)
        } else {
            DASH.to_string()
        };
        lines.push(format!(
            "{}: {} turns, {} total",
            display_model_name(m),
            format_uint(tot.turns),
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

fn format_excluded_note(excluded: &ExcludedBreakdown, minimum: FidelityClass) -> String {
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
    fn parse_provider_filter_trims_lowercases_and_dedupes() {
        let got = parse_provider_filter(Some(" Anthropic,OPENAI ,, anthropic"))
            .unwrap()
            .unwrap();
        assert!(got.contains("anthropic"));
        assert!(got.contains("openai"));
        assert_eq!(got.len(), 2, "duplicates should collapse: got {got:?}");
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
        assert!(matches!(parse_fidelity("full").unwrap(), FidelityClass::Full));
        assert!(matches!(
            parse_fidelity("usage-only").unwrap(),
            FidelityClass::UsageOnly
        ));
        assert!(parse_fidelity("nope").is_err());
    }

    #[test]
    fn display_model_name_strips_provider_prefix() {
        assert_eq!(display_model_name("anthropic/claude-sonnet-4-6"), "claude-sonnet-4-6");
        assert_eq!(display_model_name("claude-haiku-4-5"), "claude-haiku-4-5");
    }

    // -------------------------------------------------------------------
    // Codex P1 / P2: ISO normalization + relative-range millisecond
    // padding.  Both bugs surface as ledger lex-order skews: the ledger
    // stores rows with `...mmmZ` precision, so a `since` that doesn't
    // match that shape gets compared as the wrong instant.
    // -------------------------------------------------------------------

    #[test]
    fn normalize_iso_widens_no_fraction_to_three_zeros() {
        // P2 root cause: same-second ledger row `...12.500Z` would sort
        // *before* a `--since` cutoff of `...12Z`, dropping valid turns.
        // Normalizing widens to `.000Z` so the cutoff is the lower bound
        // for that second.
        assert_eq!(
            normalize_iso_to_utc_z("2026-05-06T00:00:00Z"),
            Some("2026-05-06T00:00:00.000Z".to_string()),
        );
    }

    #[test]
    fn normalize_iso_preserves_millisecond_precision() {
        assert_eq!(
            normalize_iso_to_utc_z("2026-05-06T00:00:00.500Z"),
            Some("2026-05-06T00:00:00.500Z".to_string()),
        );
        // Sub-millisecond digits are truncated to 3 (matches the ledger
        // shape; mirrors `Date.toISOString()` truncation closely enough
        // for `since`-cutoff lex ordering).
        assert_eq!(
            normalize_iso_to_utc_z("2026-05-06T00:00:00.500999Z"),
            Some("2026-05-06T00:00:00.500Z".to_string()),
        );
        // Shorter fraction is right-padded.
        assert_eq!(
            normalize_iso_to_utc_z("2026-05-06T00:00:00.5Z"),
            Some("2026-05-06T00:00:00.500Z".to_string()),
        );
    }

    #[test]
    fn normalize_iso_converts_negative_offset_to_utc() {
        // P1 root cause: `-07:00` is 7h *behind* UTC, so the same
        // wall-clock time corresponds to a UTC instant 7h *later*.
        // 2026-05-06T00:00:00-07:00 == 2026-05-06T07:00:00Z.
        assert_eq!(
            normalize_iso_to_utc_z("2026-05-06T00:00:00-07:00"),
            Some("2026-05-06T07:00:00.000Z".to_string()),
        );
    }

    #[test]
    fn normalize_iso_converts_positive_offset_to_utc() {
        // 2026-05-06T00:00:00+09:00 == 2026-05-05T15:00:00Z.
        assert_eq!(
            normalize_iso_to_utc_z("2026-05-06T00:00:00+09:00"),
            Some("2026-05-05T15:00:00.000Z".to_string()),
        );
    }

    #[test]
    fn normalize_iso_handles_lowercase_z() {
        assert_eq!(
            normalize_iso_to_utc_z("2026-05-06t00:00:00.500z"),
            Some("2026-05-06T00:00:00.500Z".to_string()),
        );
    }

    #[test]
    fn normalize_iso_accepts_date_only() {
        // Date-only input: no time component → midnight UTC.
        assert_eq!(
            normalize_iso_to_utc_z("2026-05-06"),
            Some("2026-05-06T00:00:00.000Z".to_string()),
        );
    }

    #[test]
    fn normalize_iso_rejects_garbage() {
        assert_eq!(normalize_iso_to_utc_z("not a date"), None);
        assert_eq!(normalize_iso_to_utc_z("2026/05/06"), None);
        assert_eq!(normalize_iso_to_utc_z("2026-13-01T00:00:00Z"), None); // bad month
        assert_eq!(normalize_iso_to_utc_z("2026-05-06T25:00:00Z"), None); // bad hour
        assert_eq!(normalize_iso_to_utc_z("2026-05-06T00:00:00+9"), None); // malformed offset
    }

    #[test]
    fn normalize_since_relative_emits_milliseconds() {
        // P2: relative range output must carry the `.000Z` fragment so
        // ledger rows with sub-second precision sort correctly against
        // the cutoff. We can't pin the absolute value (depends on `now`),
        // but we can assert the shape.
        let out = normalize_since(Some("7d")).unwrap().unwrap();
        assert!(out.ends_with(".000Z"), "expected .000Z suffix in {out}");
        assert_eq!(out.len(), 24, "expected 24-char canonical shape: {out}");
    }

    #[test]
    fn normalize_since_iso_pass_normalizes_offset() {
        let out = normalize_since(Some("2026-05-06T00:00:00-07:00"))
            .unwrap()
            .unwrap();
        assert_eq!(out, "2026-05-06T07:00:00.000Z");
    }

    #[test]
    fn normalize_since_relative_format_is_lex_compatible_with_ledger_rows() {
        // Sanity check: a canonical `.000Z` cutoff must lex *before* the
        // same-second ledger row carrying any non-zero millisecond
        // suffix. This is the property the bug was breaking.
        let cutoff = "2026-05-06T12:00:00.000Z";
        let row_a = "2026-05-06T12:00:00.500Z";
        let row_b = "2026-05-06T12:00:00.001Z";
        assert!(cutoff <= row_a);
        assert!(cutoff <= row_b);
    }

    #[test]
    fn ymd_round_trip() {
        for (y, m, d) in &[(1970, 1, 1), (2026, 5, 6), (2000, 2, 29), (1999, 12, 31)] {
            let days = ymd_to_days(*y, *m, *d).unwrap();
            let (ry, rm, rd) = days_to_ymd(days);
            assert_eq!((*y, *m, *d), (ry, rm, rd));
        }
    }
}
