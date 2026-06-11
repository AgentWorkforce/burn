//! Query verbs — `summary`, `summary_report`, `session_cost`, `compare`,
//! `overhead`, `overhead_trim`, `hotspots`, and `search`. Rust port of the
//! corresponding exports from `packages/sdk/index.js`, plus additive richer
//! Rust surfaces where preserving the slim Node shape matters.
//!
//! Each verb appears as an `impl LedgerHandle` method (sync, returns
//! `anyhow::Result`) plus a free-function form that opens its own ledger
//! handle from `LedgerOpenOptions`. Free functions take `ledger_home:
//! Option<PathBuf>` so callers don't have to mutate process env to point
//! at a non-default ledger.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::analyze::{
    aggregate_by_bash, aggregate_by_bash_verb, aggregate_by_file, aggregate_by_mcp_server,
    aggregate_by_provider, aggregate_by_subagent, aggregate_subagent_type_stats,
    attribute_hotspots, attribute_overhead, build_compare_table, build_ghost_surface_inputs,
    build_subagent_tree, build_trim_recommendations, compute_quality, cost_for_turn,
    detect_ghost_surface, detect_patterns, detect_tool_call_patterns, detect_tool_output_bloat,
    find_overhead_files, findings_from_patterns, ghost_surface_to_finding, has_minimum_fidelity,
    load_claude_settings, load_overhead_file, load_pricing, project_claude_settings_path,
    provider_for, render_unified_diff_for_recommendation, sort_findings, sum_costs,
    summarize_fidelity, summarize_fidelity_from_iter, summarize_replacement_savings,
    tally_unpriced, tool_call_pattern_to_finding, tool_output_bloat_to_finding,
    user_claude_settings_path, AggregateByProviderOptions, AttributeOverheadInput,
    AttributionMethod, BashAggregation, BashVerbAggregation, BuildSubagentTreeOptions,
    CompareOptions as AnalyzeCompareOptions, CompareTable, ComputeQualityOptions, CostBreakdown,
    CoverageField, DetectPatternsOptions, DetectToolCallPatternsOptions,
    DetectToolOutputBloatOptions, FidelitySummary, FieldCoverage, FileAggregation,
    GhostSurfaceFindingOptions, HotspotsOptions as AnalyzeHotspotsOptions, LoadedClaudeSettings,
    MarkdownSection, McpServerAggregation, OverheadFile, OverheadFileKind, ParsedOverheadFile,
    PricingTable, ProviderAggregateRow, ProviderFilter, QualityResult, ReplacementSavingsSummary,
    RowCoverage, SessionClaudeMdCost, SubagentAggregation, SubagentTreeNode, SubagentTypeStats,
    ToolSavingsAggregate, UsageCostAggregateRow, WasteFinding,
};
use crate::ledger::{EnrichedTurn, Enrichment, Query};
use crate::reader::{
    parse_bash_command, resolve_project, BashParse, ContentRecord, Coverage, FidelityClass,
    RelationshipType, SessionRelationshipRecord, SourceKind, StopReason, TurnRecord, Usage,
    UsageGranularity, UserTurnBlockKind, UserTurnRecord,
};

use crate::{Ledger, LedgerHandle, LedgerOpenOptions};

// ---------------------------------------------------------------------------
// since-string parsing
// ---------------------------------------------------------------------------

/// Accept either a CLI-style relative range (`24h`, `7d`, `4w`, `2m`) or an
/// ISO timestamp and return a canonical UTC `YYYY-MM-DDTHH:MM:SS.mmmZ`
/// string the ledger query can lex-compare against stored `ts` values.
///
/// Why canonicalize:
///
/// - Ledger rows always carry sub-second precision (`...mmmZ`). The SQLite
///   query filter is `ts >= ?`, which is lex-compared. A cutoff like
///   `...12Z` would sort *after* `...12.500Z` (because `.` < `Z`), dropping
///   same-second rows. Emitting `.000Z` makes the cutoff the lower bound
///   for that second.
/// - An ISO offset like `2026-05-06T00:00:00-07:00` would otherwise sort
///   before any UTC ledger row regardless of the actual instant. Re-emitting
///   as UTC keeps lex order aligned with chronological order.
///
/// Garbage inputs error out; `None` / empty inputs return `Ok(None)`.
pub fn normalize_since(since: Option<&str>) -> Result<Option<String>> {
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
        let now = system_now_secs();
        let when = now.saturating_sub(secs_back) as i64;
        return Ok(Some(format_iso_z_ms(when, 0)));
    }

    if let Some(canonical) = normalize_iso_to_utc_z(raw) {
        return Ok(Some(canonical));
    }
    anyhow::bail!("invalid since: {raw} (expected ISO timestamp or relative range like 7d)");
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

fn system_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse an ISO 8601 / RFC 3339 timestamp and re-emit it as a fully
/// canonical UTC `YYYY-MM-DDTHH:MM:SS.mmmZ` string. Handles:
///
/// - `YYYY-MM-DD` (date-only — assumed midnight UTC).
/// - `YYYY-MM-DDTHH:MM:SS` (offset-less — assumed UTC).
/// - `YYYY-MM-DDTHH:MM:SS.fff` (fractional seconds, any width 1–9).
/// - `Z` suffix (case-insensitive) or `+HH:MM` / `-HH:MM` offsets.
///
/// Returns `None` for inputs that don't look ISO-shaped, so the caller can
/// surface a usage error. Sub-millisecond fractional digits are truncated,
/// matching JS `Date.toISOString()` rounding closely enough for ledger
/// `since` lex-ordering. Whole-second inputs widen to `.000Z`.
fn normalize_iso_to_utc_z(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
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

    let mut hour: u32 = 0;
    let mut minute: u32 = 0;
    let mut second: u32 = 0;
    let mut millis: u32 = 0;
    let mut offset_minutes: i32 = 0;

    if bytes.len() > 10 {
        if !(bytes[10] == b'T' || bytes[10] == b't' || bytes[10] == b' ') {
            return None;
        }
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
            let mut frac_str = String::from(std::str::from_utf8(&bytes[frac_start..idx]).ok()?);
            if frac_str.len() > 3 {
                frac_str.truncate(3);
            }
            while frac_str.len() < 3 {
                frac_str.push('0');
            }
            millis = frac_str.parse().ok()?;
        }

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

    let days = ymd_to_days(year, month, day)?;
    let local_secs: i64 =
        days * 86_400 + (hour as i64) * 3_600 + (minute as i64) * 60 + (second as i64);
    // `local = utc + offset` → `utc = local - offset` (offset in minutes).
    let utc_secs: i64 = local_secs - (offset_minutes as i64) * 60;
    Some(format_iso_z_ms(utc_secs, millis))
}

fn format_iso_z_ms(secs: i64, millis: u32) -> String {
    let total_days = secs.div_euclid(86_400);
    let secs_in_day = secs.rem_euclid(86_400) as u32;
    let hour = secs_in_day / 3_600;
    let minute = (secs_in_day / 60) % 60;
    let second = secs_in_day % 60;
    let (year, month, day) = days_to_ymd(total_days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Civil-date → days-from-Unix-epoch (Howard Hinnant's algorithm, proleptic
/// Gregorian). Inverse of [`days_to_ymd`].
fn ymd_to_days(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let m = month as i64;
    let d = day as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + (d as u64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
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
// Shared helpers — query construction + hotspots coverage gate
// ---------------------------------------------------------------------------

fn build_query(session: Option<&str>, project: Option<&str>, since: Option<&str>) -> Result<Query> {
    let mut q = Query::default();
    if let Some(s) = session {
        q.session_id = Some(s.to_string());
    }
    if let Some(p) = project {
        q.project = Some(p.to_string());
    }
    if let Some(since_norm) = normalize_since(since)? {
        q.since = Some(since_norm);
    }
    Ok(q)
}

/// Mirrors the TS `HOTSPOTS_ATTRIBUTION_REQUIRED` + `turnPassesCoverage`
/// pair. Records without `fidelity` (older ledger writers) pass.
fn turn_passes_hotspots_coverage(turn: &TurnRecord) -> bool {
    let Some(f) = turn.fidelity.as_ref() else {
        return true;
    };
    f.coverage.has_tool_calls && f.coverage.has_tool_result_events
}

fn collect_turns(handle: &LedgerHandle, q: &Query) -> Result<Vec<TurnRecord>> {
    let enriched = handle.inner.query_turns(q)?;
    Ok(enriched.into_iter().map(|e| e.turn).collect())
}

fn bucket_user_turns_by_session(
    handle: &LedgerHandle,
    side_q: &Query,
    keep: Option<&HashSet<String>>,
) -> Result<HashMap<String, Vec<UserTurnRecord>>> {
    let mut out: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
    let user_turns = handle.inner.query_user_turns(side_q)?;
    for ut in user_turns {
        if let Some(set) = keep {
            if !set.contains(&ut.session_id) {
                continue;
            }
        }
        out.entry(ut.session_id.clone()).or_default().push(ut);
    }
    Ok(out)
}

/// Bucket `tool_result_events` rows by `session_id`, optionally filtered
/// to a `keep` set. Mirrors [`bucket_user_turns_by_session`]; powers the
/// `output_bytes` plumbing for hotspots (#436) so the SDK can hand the
/// analyze layer a per-session lookup without re-walking the ledger.
fn bucket_tool_result_events_by_session(
    handle: &LedgerHandle,
    side_q: &Query,
    keep: Option<&HashSet<String>>,
) -> Result<HashMap<String, Vec<crate::reader::ToolResultEventRecord>>> {
    let mut out: HashMap<String, Vec<crate::reader::ToolResultEventRecord>> = HashMap::new();
    let events = handle.inner.query_tool_result_events(side_q)?;
    for ev in events {
        if let Some(set) = keep {
            if !set.contains(&ev.session_id) {
                continue;
            }
        }
        out.entry(ev.session_id.clone()).or_default().push(ev);
    }
    Ok(out)
}

fn open_with(ledger_home: Option<&Path>) -> Result<LedgerHandle> {
    let opts = match ledger_home {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    Ledger::open(opts)
}

fn normalize_provider_filter(provider: Option<Vec<String>>) -> Option<ProviderFilter> {
    let filter: ProviderFilter = provider
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    (!filter.is_empty()).then_some(filter)
}

// ---------------------------------------------------------------------------
// summary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Enrichment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by_tag: Option<String>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryToolRow {
    pub tool: String,
    pub tokens: u64,
    pub cost: f64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryModelRow {
    pub model: String,
    pub tokens: u64,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryTagRow {
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub tokens: u64,
    pub cost: f64,
    pub turn_count: u64,
}

/// Per-outcome turn counts, surfaced by `burn summary` for the one-line
/// outcome breakdown (`142 end_turn, 3 max_tokens, 1 refusal, 0 pause`).
///
/// Counts mirror the [`StopReason`] enum variants plus a `none` slot for
/// turns whose row carried no `stop_reason` field at all — that's Codex
/// today (no field in the rollout schema) and any pre-3.0 ledger row that
/// was ingested before the reader started populating the enum.
///
/// `Silent` is reserved for "row exists, carries a stop_reason that we
/// don't recognize" — distinct from `none` so we can spot a future harness
/// regression rather than silently lumping it with Codex.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopReasonCounts {
    pub end_turn: u64,
    pub max_tokens: u64,
    pub pause_turn: u64,
    pub stop_sequence: u64,
    pub tool_use: u64,
    pub refusal: u64,
    pub silent: u64,
    /// Turns whose record carried no `stop_reason` field — e.g. Codex
    /// rollouts (the harness doesn't report one) or pre-3.0 ledger rows
    /// from before the reader started parsing the field.
    pub none: u64,
}

impl StopReasonCounts {
    /// Accumulate one turn's outcome into the bucket counts. `None` lands
    /// in [`Self::none`]; unrecognized variants would already be normalized
    /// to [`StopReason::Silent`] upstream by the lenient deserializer.
    pub fn bump(&mut self, reason: Option<StopReason>) {
        match reason {
            None => self.none += 1,
            Some(StopReason::EndTurn) => self.end_turn += 1,
            Some(StopReason::MaxTokens) => self.max_tokens += 1,
            Some(StopReason::PauseTurn) => self.pause_turn += 1,
            Some(StopReason::StopSequence) => self.stop_sequence += 1,
            Some(StopReason::ToolUse) => self.tool_use += 1,
            Some(StopReason::Refusal) => self.refusal += 1,
            Some(StopReason::Silent) => self.silent += 1,
        }
    }

    /// Fold every turn's `stop_reason` into a fresh counts struct.
    pub fn from_turns(turns: &[TurnRecord]) -> Self {
        let mut out = Self::default();
        for t in turns {
            out.bump(t.stop_reason);
        }
        out
    }

    /// True iff every counter is zero — useful for "skip the outcome line
    /// entirely" presentation logic in summary.
    pub fn is_empty(&self) -> bool {
        self.end_turn
            | self.max_tokens
            | self.pause_turn
            | self.stop_sequence
            | self.tool_use
            | self.refusal
            | self.silent
            | self.none
            == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub total_tokens: u64,
    pub total_cost: f64,
    pub turn_count: u64,
    pub by_tool: Vec<SummaryToolRow>,
    pub by_model: Vec<SummaryModelRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_tag: Option<Vec<SummaryTagRow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_savings: Option<ReplacementSavingsSummary>,
    /// Per-outcome breakdown — `end_turn` / `max_tokens` / `refusal` / etc.
    /// Counts roll up the trailing `stop_reason` of every assistant turn
    /// in the filtered slice. See #437.
    pub stop_reasons: StopReasonCounts,
    /// Count of turns whose model had no entry in the pricing snapshot.
    /// Their cost is reported as $0. Zero when all models are priced.
    #[serde(default)]
    pub unpriced_turns: u64,
    /// Distinct model names (first-seen order) that had no pricing entry.
    /// Empty when all models are priced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<String>,
}

impl LedgerHandle {
    pub fn summary(&self, opts: SummaryOptions) -> Result<Summary> {
        let mut q = build_query(
            opts.session.as_deref(),
            opts.project.as_deref(),
            opts.since.as_deref(),
        )?;
        if let Some(tags) = opts.tags.clone() {
            validate_tags(&tags)?;
            if !tags.is_empty() {
                q.enrichment = Some(tags);
            }
        }
        let group_by_tag = opts.group_by_tag.clone();
        if let Some(tag) = group_by_tag.as_deref() {
            validate_tag_key(tag, "groupByTag")?;
        }
        let enriched = self.inner.query_turns(&q)?;
        let turns: Vec<TurnRecord> = enriched.iter().map(|e| e.turn.clone()).collect();
        let pricing = load_pricing(None);
        let mut summary = compute_summary(&turns, &pricing);
        if let Some(tag) = group_by_tag {
            summary.by_tag = Some(compute_summary_by_tag(&enriched, &tag, &pricing));
        }
        Ok(summary)
    }
}

pub fn summary(opts: SummaryOptions) -> Result<Summary> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.summary(SummaryOptions {
        ledger_home: None,
        ..opts
    })
}

fn validate_tags(tags: &Enrichment) -> Result<()> {
    for key in tags.keys() {
        validate_tag_key(key, "tag")?;
    }
    Ok(())
}

fn validate_tag_key(key: &str, label: &str) -> Result<()> {
    if key.is_empty() {
        anyhow::bail!("{label} key must be non-empty");
    }
    Ok(())
}

fn compute_summary(turns: &[TurnRecord], pricing: &PricingTable) -> Summary {
    // First-seen iteration order matches TS `Map` semantics.
    let mut by_tool_order: Vec<String> = Vec::new();
    let mut by_tool: HashMap<String, SummaryToolRow> = HashMap::new();
    let mut by_model_order: Vec<String> = Vec::new();
    let mut by_model: HashMap<String, SummaryModelRow> = HashMap::new();
    let mut total_tokens: u64 = 0;
    let mut total_cost: f64 = 0.0;

    for t in turns {
        let cost = cost_for_turn(t, pricing).map(|c| c.total).unwrap_or(0.0);
        let tokens = t.usage.input
            + t.usage.output
            + t.usage.reasoning
            + t.usage.cache_read
            + t.usage.cache_create_5m
            + t.usage.cache_create_1h;
        total_tokens += tokens;
        total_cost += cost;

        let model_row = by_model.entry(t.model.clone()).or_insert_with(|| {
            by_model_order.push(t.model.clone());
            SummaryModelRow {
                model: t.model.clone(),
                tokens: 0,
                cost: 0.0,
            }
        });
        model_row.tokens += tokens;
        model_row.cost += cost;

        for call in &t.tool_calls {
            let tool_row = by_tool.entry(call.name.clone()).or_insert_with(|| {
                by_tool_order.push(call.name.clone());
                SummaryToolRow {
                    tool: call.name.clone(),
                    tokens: 0,
                    cost: 0.0,
                    count: 0,
                }
            });
            tool_row.tokens += tokens;
            tool_row.cost += cost;
            tool_row.count += 1;
        }
    }

    let savings = summarize_replacement_savings(turns, None);
    let replacement_savings = if savings.calls > 0 {
        Some(savings)
    } else {
        None
    };

    // Use the same pricing table that was used for cost accumulation so the
    // count precisely matches which turns contributed $0 to `total_cost`.
    let (unpriced_turns, unpriced_models) = tally_unpriced(turns, pricing);

    Summary {
        total_tokens,
        total_cost,
        turn_count: turns.len() as u64,
        by_tool: by_tool_order
            .into_iter()
            .map(|k| by_tool.remove(&k).unwrap())
            .collect(),
        by_model: by_model_order
            .into_iter()
            .map(|k| by_model.remove(&k).unwrap())
            .collect(),
        by_tag: None,
        replacement_savings,
        stop_reasons: StopReasonCounts::from_turns(turns),
        unpriced_turns,
        unpriced_models,
    }
}

fn compute_summary_by_tag(
    enriched: &[EnrichedTurn],
    tag: &str,
    pricing: &PricingTable,
) -> Vec<SummaryTagRow> {
    let mut order: Vec<Option<String>> = Vec::new();
    let mut rows: HashMap<Option<String>, SummaryTagRow> = HashMap::new();

    for e in enriched {
        let value = e.enrichment.get(tag).cloned();
        let tokens = total_tokens_for_turn(&e.turn);
        let cost = cost_for_turn(&e.turn, pricing)
            .map(|c| c.total)
            .unwrap_or(0.0);
        let row = rows.entry(value.clone()).or_insert_with(|| {
            order.push(value.clone());
            SummaryTagRow {
                tag: tag.to_string(),
                value,
                tokens: 0,
                cost: 0.0,
                turn_count: 0,
            }
        });
        row.tokens += tokens;
        row.cost += cost;
        row.turn_count += 1;
    }

    let mut out: Vec<SummaryTagRow> = order
        .into_iter()
        .map(|k| rows.remove(&k).unwrap())
        .collect();
    out.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn total_tokens_for_turn(t: &TurnRecord) -> u64 {
    t.usage.input
        + t.usage.output
        + t.usage.reasoning
        + t.usage.cache_read
        + t.usage.cache_create_5m
        + t.usage.cache_create_1h
}

// ---------------------------------------------------------------------------
// richer summary report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryReportOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Enrichment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by_tag: Option<String>,
    pub agent: Option<String>,
    /// Provider labels to keep. Values are trimmed and matched
    /// case-insensitively against the SDK's effective provider resolver.
    #[serde(default)]
    pub providers: Option<Vec<String>>,
    #[serde(default)]
    pub mode: SummaryReportMode,
    #[serde(default)]
    pub include_quality: bool,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum SummaryReportMode {
    Grouped {
        #[serde(default)]
        by_provider: bool,
    },
    ByTool,
    BySubagentType,
    ByRelationship {
        #[serde(default)]
        subagent: bool,
    },
    SubagentTree {
        #[serde(default)]
        session_id: Option<String>,
    },
}

impl Default for SummaryReportMode {
    fn default() -> Self {
        Self::Grouped { by_provider: false }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SummaryGroupBy {
    Model,
    Provider,
    Tag,
}

impl SummaryGroupBy {
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Tag => "tag",
        }
    }

    pub fn json_key(self) -> &'static str {
        match self {
            Self::Model => "byModel",
            Self::Provider => "byProvider",
            Self::Tag => "byTag",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum SummaryReport {
    Grouped(SummaryGroupedReport),
    ByTool(SummaryByToolReport),
    BySubagentType(SummarySubagentTypeReport),
    Relationship(SummaryRelationshipReport),
    SubagentTree(SummarySubagentTreeReport),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryGroupedReport {
    pub group_by: SummaryGroupBy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tag_values: Vec<Option<String>>,
    pub turn_count: u64,
    pub rows: Vec<UsageCostAggregateRow>,
    pub total_cost: CostBreakdown,
    pub fidelity: FidelitySummary,
    /// Stable TS-compatible JSON shape for per-cell coverage. Kept in the SDK
    /// so presenters don't rebuild order-sensitive HashMap projections.
    pub per_cell_fidelity: serde_json::Value,
    pub replacement_savings: ReplacementSavingsSummary,
    /// Per-outcome turn counts (issue #437). Always populated; presenters
    /// decide whether to render the line based on `is_empty()`.
    pub stop_reasons: StopReasonCounts,
    /// Paired / orphan subagent transcript counts (issue #435). Populated
    /// by a lazy walk over the Claude `~/.claude/projects/` tree at
    /// summary time — when no sidecars exist anywhere reachable the
    /// `read_dir` short-circuits and the field stays at
    /// `SubagentCounts::default()`. Presenters render the
    /// `subagents: X paired, Y orphan` line only when
    /// `!subagents.is_empty()`.
    #[serde(
        default,
        skip_serializing_if = "crate::reader::SubagentCounts::is_empty"
    )]
    pub subagents: crate::reader::SubagentCounts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<QualityResult>,
    /// Count of turns whose model had no entry in the pricing snapshot.
    /// Their cost is reported as $0. Zero when all models are priced.
    #[serde(default)]
    pub unpriced_turns: u64,
    /// Distinct model names (first-seen order) that had no pricing entry.
    /// Empty when all models are priced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SummaryToolAttributionMethod {
    Unattributed,
    Sized,
    EvenSplit,
}

impl SummaryToolAttributionMethod {
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Unattributed => "unattributed",
            Self::Sized => "sized",
            Self::EvenSplit => "even-split",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryToolAttributionRow {
    pub tool: String,
    pub calls: u64,
    pub attributed_cost: f64,
    pub attribution_method: SummaryToolAttributionMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub savings: Option<ToolSavingsAggregate>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryByToolReport {
    pub turn_count: u64,
    pub rows: Vec<SummaryToolAttributionRow>,
    pub unattributed_cost: f64,
    pub fidelity: FidelitySummary,
    pub replacement_savings: ReplacementSavingsSummary,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummarySubagentTypeReport {
    pub stats: Vec<SubagentTypeStats>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRelationshipReport {
    pub relationships: Vec<SummaryRelationshipStats>,
    pub subagent_types: Vec<SummaryRelationshipSubagentStats>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRelationshipStats {
    pub relationship_type: RelationshipType,
    pub count: u64,
    pub session_count: u64,
    pub turn_count: u64,
    pub total_cost: f64,
    pub median_cost: f64,
    pub p95_cost: f64,
    pub mean_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRelationshipSubagentStats {
    pub subagent_type: String,
    pub invocations: u64,
    pub turns: u64,
    pub total_cost: f64,
    pub median_cost: f64,
    pub p95_cost: f64,
    pub mean_cost: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummarySubagentTreeReport {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<SubagentTreeNode>,
}

impl LedgerHandle {
    pub fn summary_report(&self, opts: SummaryReportOptions) -> Result<SummaryReport> {
        let q = build_summary_report_query(&opts)?;
        let provider_filter = normalize_summary_provider_filter(opts.providers.as_deref());
        let pricing = load_pricing(None);
        let agent_session_ids = match opts.agent.as_deref() {
            Some(agent_id) => Some(resolve_summary_agent_session_tree(&self.inner, agent_id)?),
            None => None,
        };

        if let SummaryReportMode::SubagentTree { session_id } = &opts.mode {
            let session_id = session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| q.session_id.clone())
                .ok_or_else(|| anyhow::anyhow!("subagent tree summary requires a session id"))?;
            let relationships =
                collect_summary_subagent_tree_relationships(&self.inner, &session_id, &q)?;
            let enriched =
                load_summary_subagent_tree_turns(&self.inner, &session_id, &relationships, &q)?;
            let enriched = filter_summary_enriched_turns(
                enriched,
                opts.agent.as_deref(),
                agent_session_ids.as_ref(),
                provider_filter.as_ref(),
            );
            let turns = summary_turns_from_enriched(&enriched);
            let tree_opts =
                BuildSubagentTreeOptions::new(&pricing).with_relationships(&relationships);
            let trees = build_subagent_tree(&turns, &tree_opts);
            let root = trees
                .get(&session_id)
                .cloned()
                .or_else(|| find_summary_tree_node(trees.values(), &session_id));
            return Ok(SummaryReport::SubagentTree(SummarySubagentTreeReport {
                session_id,
                root,
            }));
        }

        let enriched = self.inner.query_turns(&q)?;
        let enriched = filter_summary_enriched_turns(
            enriched,
            opts.agent.as_deref(),
            agent_session_ids.as_ref(),
            provider_filter.as_ref(),
        );
        let turns = summary_turns_from_enriched(&enriched);

        match opts.mode {
            SummaryReportMode::Grouped { by_provider } => {
                let (group_by, tag_key, tag_values, rows) = if let Some(tag_key) =
                    opts.group_by_tag.as_deref()
                {
                    let (rows, values) = summary_aggregate_by_tag(&enriched, tag_key, &pricing);
                    (SummaryGroupBy::Tag, Some(tag_key.to_string()), values, rows)
                } else if by_provider {
                    (
                        SummaryGroupBy::Provider,
                        None,
                        Vec::new(),
                        aggregate_by_provider(&turns, AggregateByProviderOptions::new(&pricing))
                            .into_iter()
                            .map(summary_provider_to_aggregate_row)
                            .collect(),
                    )
                } else {
                    (
                        SummaryGroupBy::Model,
                        None,
                        Vec::new(),
                        summary_aggregate_by_model(&turns, &pricing),
                    )
                };
                let total_cost = sum_costs(rows.iter().map(|r| &r.cost));
                let fidelity = summarize_fidelity(&turns);
                let per_cell_fidelity = summary_per_cell_fidelity_to_value(&rows, group_by);
                let replacement_savings = summarize_replacement_savings(&turns, None);
                let quality = if opts.include_quality {
                    Some(compute_summary_quality_for_turns(&self.inner, &turns)?)
                } else {
                    None
                };
                let stop_reasons = StopReasonCounts::from_turns(&turns);
                // Lazy walk over `~/.claude/projects/` (or the configured
                // override) for the `subagents: X paired, Y orphan`
                // summary line (issue #435). The walk short-circuits when
                // the projects root is missing or every session lacks a
                // `subagents/` subdir — i.e. zero cost on the vast
                // majority of summaries that don't hit a session with
                // sidecar transcripts.
                //
                // When the summary itself is scoped (any of `--session`,
                // `--project`, `--since`, `--workflow`, `--tags`,
                // `--agent`, `--providers`) we restrict the sidecar
                // walk to the same session-id set the rest of the
                // summary covers; otherwise the line could report
                // paired/orphan counts from sessions the user excluded.
                // Un-filtered runs keep the original global walk
                // behavior.
                let session_filter = summary_subagent_session_filter(&opts, &turns);
                let subagents = compute_summary_subagent_counts(session_filter.as_ref());
                let (unpriced_turns, unpriced_models) = tally_unpriced(&turns, &pricing);
                Ok(SummaryReport::Grouped(SummaryGroupedReport {
                    group_by,
                    tag_key,
                    tag_values,
                    turn_count: turns.len() as u64,
                    rows,
                    total_cost,
                    fidelity,
                    per_cell_fidelity,
                    replacement_savings,
                    stop_reasons,
                    subagents,
                    quality,
                    unpriced_turns,
                    unpriced_models,
                }))
            }
            SummaryReportMode::ByTool => {
                let attribution_turns =
                    load_summary_by_tool_attribution_turns(&self.inner, &enriched, &q)?;
                let report = compute_summary_by_tool_report(
                    &self.inner,
                    &turns,
                    &attribution_turns,
                    &pricing,
                )?;
                Ok(SummaryReport::ByTool(report))
            }
            SummaryReportMode::BySubagentType => {
                let stats =
                    aggregate_subagent_type_stats(&turns, &BuildSubagentTreeOptions::new(&pricing));
                Ok(SummaryReport::BySubagentType(SummarySubagentTypeReport {
                    stats,
                }))
            }
            SummaryReportMode::ByRelationship { subagent } => {
                let relationships = self
                    .inner
                    .query_relationships(&summary_relationship_query_for_turn_slice(&q))?;
                let matches =
                    match_summary_relationships_to_turns(&relationships, &turns, &pricing);
                let stats = aggregate_summary_relationship_stats(&matches);
                if subagent {
                    let subagent_types = aggregate_summary_relationship_subagent_stats(&matches);
                    let relationships = stats
                        .into_iter()
                        .filter(|s| s.relationship_type == RelationshipType::Subagent)
                        .collect();
                    Ok(SummaryReport::Relationship(SummaryRelationshipReport {
                        relationships,
                        subagent_types,
                    }))
                } else {
                    Ok(SummaryReport::Relationship(SummaryRelationshipReport {
                        relationships: stats,
                        subagent_types: Vec::new(),
                    }))
                }
            }
            SummaryReportMode::SubagentTree { .. } => unreachable!(),
        }
    }
}

pub fn summary_report(opts: SummaryReportOptions) -> Result<SummaryReport> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.summary_report(SummaryReportOptions {
        ledger_home: None,
        ..opts
    })
}

pub fn summary_fidelity_summary_to_value(s: &FidelitySummary) -> serde_json::Value {
    let mut by_class = serde_json::Map::new();
    for class in [
        FidelityClass::Full,
        FidelityClass::UsageOnly,
        FidelityClass::AggregateOnly,
        FidelityClass::CostOnly,
        FidelityClass::Partial,
    ] {
        by_class.insert(
            class.wire_str().to_string(),
            serde_json::json!(*s.by_class.get(&class).unwrap_or(&0)),
        );
    }

    let mut by_granularity = serde_json::Map::new();
    for g in [
        UsageGranularity::PerTurn,
        UsageGranularity::PerMessage,
        UsageGranularity::PerSessionAggregate,
        UsageGranularity::CostOnly,
    ] {
        by_granularity.insert(
            g.wire_str().to_string(),
            serde_json::json!(*s.by_granularity.get(&g).unwrap_or(&0)),
        );
    }

    let mut missing = serde_json::Map::new();
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
            serde_json::json!(*s.missing_coverage.get(field).unwrap_or(&0)),
        );
    }

    let mut out = serde_json::Map::new();
    out.insert("total".into(), serde_json::json!(s.total));
    out.insert("byClass".into(), serde_json::Value::Object(by_class));
    out.insert(
        "byGranularity".into(),
        serde_json::Value::Object(by_granularity),
    );
    out.insert("missingCoverage".into(), serde_json::Value::Object(missing));
    out.insert("unknown".into(), serde_json::json!(s.unknown));
    serde_json::Value::Object(out)
}

pub fn summary_per_cell_fidelity_to_value(
    rows: &[UsageCostAggregateRow],
    group_by: SummaryGroupBy,
) -> serde_json::Value {
    let cells: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let fields = [
                ("input", &r.coverage.input),
                ("output", &r.coverage.output),
                ("reasoning", &r.coverage.reasoning),
                ("cacheRead", &r.coverage.cache_read),
                ("cacheCreate", &r.coverage.cache_create),
            ];
            let mut fields_map = serde_json::Map::new();
            let mut partial = false;
            for (name, c) in fields {
                if summary_cell_is_partial(c) || (c.known == 0 && c.missing > 0) {
                    partial = true;
                }
                fields_map.insert(
                    name.to_string(),
                    serde_json::json!({
                        "known": c.known,
                        "missing": c.missing,
                    }),
                );
            }
            serde_json::json!({
                "label": r.label,
                "partial": partial,
                "fields": serde_json::Value::Object(fields_map),
            })
        })
        .collect();
    serde_json::json!({
        "groupBy": group_by.wire_str(),
        "cells": cells,
    })
}

pub fn summary_replacement_savings_to_value(
    savings: &ReplacementSavingsSummary,
) -> serde_json::Value {
    let mut by_tool: Vec<serde_json::Value> = savings
        .by_tool
        .iter()
        .map(|(name, agg)| {
            serde_json::json!({
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
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let bv = b
            .get("estimatedTokensSaved")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        bv.cmp(&av).then_with(|| {
            let at = a
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let bt = b
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            at.cmp(bt)
        })
    });
    serde_json::json!({
        "calls": savings.calls,
        "collapsedCalls": savings.collapsed_calls,
        "estimatedTokensSaved": savings.estimated_tokens_saved,
        "byTool": by_tool,
    })
}

fn build_summary_report_query(opts: &SummaryReportOptions) -> Result<Query> {
    let mut q = build_query(
        opts.session.as_deref(),
        opts.project.as_deref(),
        opts.since.as_deref(),
    )?;
    if let Some(tag) = opts.group_by_tag.as_deref() {
        validate_tag_key(tag, "groupByTag")?;
    }
    let mut enrichment = BTreeMap::new();
    if let Some(workflow) = &opts.workflow {
        enrichment.insert("workflowId".to_string(), workflow.clone());
    }
    if let Some(tags) = opts.tags.as_ref() {
        validate_tags(tags)?;
        for (key, value) in tags {
            if let Some(existing) = enrichment.get(key) {
                if existing != value {
                    anyhow::bail!(
                        "conflicting filters for tag \"{key}\" ({existing:?} vs {value:?})"
                    );
                }
            }
            enrichment.insert(key.clone(), value.clone());
        }
    }
    if !enrichment.is_empty() {
        q.enrichment = Some(enrichment);
    }
    Ok(q)
}

fn normalize_summary_provider_filter(providers: Option<&[String]>) -> Option<ProviderFilter> {
    let providers: ProviderFilter = providers
        .unwrap_or(&[])
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if providers.is_empty() {
        None
    } else {
        Some(providers)
    }
}

fn filter_summary_enriched_turns(
    turns: Vec<EnrichedTurn>,
    agent_id: Option<&str>,
    agent_session_ids: Option<&HashSet<String>>,
    provider_filter: Option<&ProviderFilter>,
) -> Vec<EnrichedTurn> {
    turns
        .into_iter()
        .filter(|t| summary_agent_passes(t, agent_id, agent_session_ids))
        .filter(|t| summary_provider_passes(&t.turn, provider_filter))
        .collect()
}

fn summary_agent_passes(
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

fn summary_provider_passes(t: &TurnRecord, provider_filter: Option<&ProviderFilter>) -> bool {
    let Some(filter) = provider_filter else {
        return true;
    };
    let provider = provider_for(t).provider.to_ascii_lowercase();
    filter.contains(&provider)
}

fn summary_turns_from_enriched(enriched: &[EnrichedTurn]) -> Vec<TurnRecord> {
    enriched.iter().map(|e| e.turn.clone()).collect()
}

fn load_summary_by_tool_attribution_turns(
    ledger: &crate::ledger::Ledger,
    selected: &[EnrichedTurn],
    q: &Query,
) -> Result<Vec<TurnRecord>> {
    let session_ids: Vec<String> = selected
        .iter()
        .map(|e| e.turn.session_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let turns = ledger.query_turns_in_sessions(
        &Query {
            source: q.source,
            ..Default::default()
        },
        &session_ids,
    )?;
    let mut by_key: IndexMap<String, EnrichedTurn> = IndexMap::new();
    for t in turns {
        let key = format!(
            "{}|{}|{}",
            t.turn.source.wire_str(),
            t.turn.session_id,
            t.turn.message_id,
        );
        by_key.insert(key, t);
    }
    Ok(by_key.into_values().map(|e| e.turn).collect())
}

fn resolve_summary_agent_session_tree(
    ledger: &crate::ledger::Ledger,
    agent_id: &str,
) -> Result<HashSet<String>> {
    Ok(collect_summary_agent_session_tree(
        &ledger.query_relationships(&Query::default())?,
        agent_id,
    ))
}

fn collect_summary_agent_session_tree(
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

/// Resolve the Claude projects root and run [`count_subagents_under`]
/// against it for the `subagents: X paired, Y orphan` summary line.
///
/// We honor `BURN_CLAUDE_PROJECTS_DIR` so tests (and integration
/// fixtures) can point at a sandbox without scanning the developer's
/// `~/.claude`. The env var also lets the CLI summary remain
/// reproducible against a fixture-only test suite. When unset we fall
/// back to `$HOME/.claude/projects`; if that doesn't exist the
/// underlying walk returns `(0, 0)` and the summary line is skipped.
///
/// `session_filter` matches the rest of the summary's filter set:
/// `None` means "no filter — count every session reachable from the
/// projects root" (the un-filtered `burn summary` path); `Some(set)`
/// means "only count sidecars whose session id is in `set`" so a
/// `burn summary --session A` / `--project B` / `--since 24h` run gets
/// a subagent count scoped to the same sessions the rest of the
/// numbers cover.
fn compute_summary_subagent_counts(
    session_filter: Option<&HashSet<String>>,
) -> crate::reader::SubagentCounts {
    use crate::reader::count_subagents_under;
    let root = if let Some(p) = std::env::var_os("BURN_CLAUDE_PROJECTS_DIR") {
        std::path::PathBuf::from(p)
    } else {
        // `HOME` is unset on stock Windows shells (`USERPROFILE` carries
        // the user home there). Fall back to it before degenerating to
        // `.` so a Claude Code install on Windows still resolves to
        // `%USERPROFILE%\.claude\projects` without the caller having
        // to set `BURN_CLAUDE_PROJECTS_DIR` explicitly.
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        home.join(".claude").join("projects")
    };
    count_subagents_under(&root, session_filter)
}

/// Build the session-id filter set the subagent counter should descend
/// into. Returns `None` when `opts` carries no scoping filters, which
/// preserves the original "scan every reachable session" behavior for
/// the bare `burn summary` invocation. Returns `Some(set)` when any
/// filter (`session`, `project`, `since`, `workflow`, `tags`, `agent`,
/// `providers`) is active — `set` is the session ids that survived
/// every filter, derived from the already-filtered `turns` slice.
///
/// Plumbing the filter via the filtered turn set (instead of e.g.
/// duplicating the SQL filters inside the walker) ensures the count
/// can never diverge from the rest of the summary numbers: anything
/// that drops a session from the row aggregates also drops it from the
/// subagent count.
fn summary_subagent_session_filter(
    opts: &SummaryReportOptions,
    turns: &[TurnRecord],
) -> Option<HashSet<String>> {
    let has_filter = opts.session.is_some()
        || opts.project.is_some()
        || opts.since.is_some()
        || opts.workflow.is_some()
        || opts.agent.is_some()
        || opts.tags.as_ref().map(|t| !t.is_empty()).unwrap_or(false)
        || opts
            .providers
            .as_ref()
            .map(|p| !p.is_empty())
            .unwrap_or(false);
    if !has_filter {
        return None;
    }
    Some(turns.iter().map(|t| t.session_id.clone()).collect())
}

fn compute_summary_quality_for_turns(
    ledger: &crate::ledger::Ledger,
    turns: &[TurnRecord],
) -> Result<QualityResult> {
    let content_by_session = load_summary_content_for_quality(ledger, turns)?;
    Ok(compute_quality(
        turns,
        &ComputeQualityOptions {
            content_by_session: Some(&content_by_session),
            now_ms: None,
        },
    ))
}

fn load_summary_content_for_quality(
    ledger: &crate::ledger::Ledger,
    turns: &[TurnRecord],
) -> Result<HashMap<String, Vec<ContentRecord>>> {
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

fn summary_aggregate_by_tag(
    enriched: &[EnrichedTurn],
    tag_key: &str,
    pricing: &PricingTable,
) -> (Vec<UsageCostAggregateRow>, Vec<Option<String>>) {
    let mut by_value: HashMap<Option<String>, UsageCostAggregateRow> = HashMap::new();
    let mut order: Vec<Option<String>> = Vec::new();
    for enriched in enriched {
        let value = enriched.enrichment.get(tag_key).cloned();
        let label = value.clone().unwrap_or_else(|| "(untagged)".to_string());
        let row = by_value.entry(value.clone()).or_insert_with(|| {
            order.push(value.clone());
            summary_empty_row(&label)
        });
        row.turns += 1;
        row.usage.input += enriched.turn.usage.input;
        row.usage.output += enriched.turn.usage.output;
        row.usage.reasoning += enriched.turn.usage.reasoning;
        row.usage.cache_read += enriched.turn.usage.cache_read;
        row.usage.cache_create_5m += enriched.turn.usage.cache_create_5m;
        row.usage.cache_create_1h += enriched.turn.usage.cache_create_1h;
        summary_accumulate_coverage(
            &mut row.coverage,
            enriched.turn.fidelity.as_ref().map(|f| &f.coverage),
        );
        if let Some(c) = cost_for_turn(&enriched.turn, pricing) {
            row.cost.total += c.total;
            row.cost.input += c.input;
            row.cost.output += c.output;
            row.cost.reasoning += c.reasoning;
            row.cost.cache_read += c.cache_read;
            row.cost.cache_create += c.cache_create;
        }
    }

    let mut pairs: Vec<(Option<String>, UsageCostAggregateRow)> = order
        .into_iter()
        .map(|value| {
            let row = by_value.remove(&value).unwrap();
            (value, row)
        })
        .collect();
    pairs.sort_by(|a, b| {
        b.1.cost
            .total
            .partial_cmp(&a.1.cost.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let (values, rows): (Vec<Option<String>>, Vec<UsageCostAggregateRow>) =
        pairs.into_iter().unzip();
    (rows, values)
}

fn summary_aggregate_by_model(
    turns: &[TurnRecord],
    pricing: &PricingTable,
) -> Vec<UsageCostAggregateRow> {
    let mut by_model: IndexMap<String, UsageCostAggregateRow> = IndexMap::new();
    for t in turns {
        let key = if t.model.is_empty() {
            "unknown".to_string()
        } else {
            t.model.clone()
        };
        let row = by_model
            .entry(key.clone())
            .or_insert_with(|| summary_empty_row(&key));
        row.turns += 1;
        row.usage.input += t.usage.input;
        row.usage.output += t.usage.output;
        row.usage.reasoning += t.usage.reasoning;
        row.usage.cache_read += t.usage.cache_read;
        row.usage.cache_create_5m += t.usage.cache_create_5m;
        row.usage.cache_create_1h += t.usage.cache_create_1h;
        summary_accumulate_coverage(&mut row.coverage, t.fidelity.as_ref().map(|f| &f.coverage));
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

fn summary_provider_to_aggregate_row(p: ProviderAggregateRow) -> UsageCostAggregateRow {
    UsageCostAggregateRow {
        label: p.label,
        turns: p.turns,
        usage: p.usage,
        cost: p.cost,
        coverage: p.coverage,
    }
}

fn summary_empty_row(label: &str) -> UsageCostAggregateRow {
    UsageCostAggregateRow {
        label: label.to_string(),
        turns: 0,
        usage: Usage::default(),
        cost: CostBreakdown {
            model: label.to_string().into(),
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

fn summary_accumulate_coverage(target: &mut RowCoverage, coverage: Option<&Coverage>) {
    for f in [
        CoverageField::Input,
        CoverageField::Output,
        CoverageField::Reasoning,
        CoverageField::CacheRead,
        CoverageField::CacheCreate,
    ] {
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

fn summary_cell_is_partial(c: &FieldCoverage) -> bool {
    c.known > 0 && c.missing > 0
}

#[derive(Debug, Default, Clone)]
struct SummaryToolAgg {
    calls: u64,
    cost: f64,
    sized_cost: f64,
    even_split_cost: f64,
}

#[derive(Debug, Default)]
struct SummaryUserTurnSizeBucket {
    tool_bytes_by_id: HashMap<String, u64>,
    total_bytes: u64,
}

fn compute_summary_by_tool_report(
    ledger: &crate::ledger::Ledger,
    turns: &[TurnRecord],
    attribution_turns: &[TurnRecord],
    pricing: &PricingTable,
) -> Result<SummaryByToolReport> {
    let user_turns_by_session = load_summary_user_turns_for_by_tool(ledger, attribution_turns)?;
    let selected_turns = selected_summary_turn_keys(turns);
    let (by_tool, unattributed_cost) = attribute_summary_cost_to_tools(
        attribution_turns,
        pricing,
        &user_turns_by_session,
        Some(&selected_turns),
    );
    let fidelity = summarize_fidelity(turns);
    let replacement_savings = summarize_replacement_savings(turns, None);
    let mut sorted: Vec<(String, SummaryToolAgg)> = by_tool.into_iter().collect();
    sorted.sort_by(|a, b| {
        b.1.cost
            .partial_cmp(&a.1.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let rows = sorted
        .into_iter()
        .map(|(tool, agg)| SummaryToolAttributionRow {
            savings: replacement_savings.by_tool.get(&tool).cloned(),
            tool,
            calls: agg.calls,
            attributed_cost: agg.cost,
            attribution_method: summary_tool_attribution_method(&agg),
        })
        .collect();
    Ok(SummaryByToolReport {
        turn_count: turns.len() as u64,
        rows,
        unattributed_cost,
        fidelity,
        replacement_savings,
    })
}

fn load_summary_user_turns_for_by_tool(
    ledger: &crate::ledger::Ledger,
    turns: &[TurnRecord],
) -> Result<HashMap<String, Vec<UserTurnRecord>>> {
    let session_ids: BTreeSet<String> = turns.iter().map(|t| t.session_id.clone()).collect();
    let mut out = HashMap::new();
    for session_id in session_ids {
        let rows = ledger.query_user_turns(&Query {
            session_id: Some(session_id.clone()),
            ..Default::default()
        })?;
        if !rows.is_empty() {
            out.insert(session_id, rows);
        }
    }
    Ok(out)
}

fn selected_summary_turn_keys(turns: &[TurnRecord]) -> HashSet<String> {
    turns.iter().map(summary_turn_identity_key).collect()
}

fn attribute_summary_cost_to_tools(
    turns: &[TurnRecord],
    pricing: &PricingTable,
    user_turns_by_session: &HashMap<String, Vec<UserTurnRecord>>,
    selected_turns: Option<&HashSet<String>>,
) -> (IndexMap<String, SummaryToolAgg>, f64) {
    let mut by_tool: IndexMap<String, SummaryToolAgg> = IndexMap::new();
    let mut unattributed = 0.0;
    let mut by_session: IndexMap<String, Vec<&TurnRecord>> = IndexMap::new();
    for t in turns {
        by_session.entry(t.session_id.clone()).or_default().push(t);
    }

    for (session_id, mut list) in by_session {
        list.sort_by_key(|t| t.turn_index);
        let user_turn_size_index = index_summary_user_turn_block_sizes(
            user_turns_by_session
                .get(&session_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        );
        for i in 0..list.len() {
            let turn = list[i];
            if !summary_turn_is_selected(turn, selected_turns) {
                continue;
            }
            let Some(c) = cost_for_turn(turn, pricing) else {
                continue;
            };
            let ingest_cost = c.input + c.cache_read + c.cache_create;

            if i == 0 {
                unattributed += ingest_cost;
                continue;
            }
            let prior = list[i - 1];
            if prior.tool_calls.is_empty() {
                unattributed += ingest_cost;
                continue;
            }

            let key = summary_bridge_key(&prior.message_id, &turn.message_id);
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
                    by_tool.entry(tc.name.clone()).or_default().calls += 1;
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
                    agg.calls += 1;
                    agg.cost += share;
                    agg.even_split_cost += share;
                }
            }
        }
    }

    (by_tool, unattributed)
}

fn summary_turn_is_selected(turn: &TurnRecord, selected_turns: Option<&HashSet<String>>) -> bool {
    selected_turns
        .map(|keys| keys.contains(&summary_turn_identity_key(turn)))
        .unwrap_or(true)
}

fn summary_turn_identity_key(turn: &TurnRecord) -> String {
    format!(
        "{}\0{}\0{}",
        turn.source.wire_str(),
        turn.session_id,
        turn.message_id
    )
}

fn index_summary_user_turn_block_sizes(
    user_turns: &[UserTurnRecord],
) -> HashMap<String, SummaryUserTurnSizeBucket> {
    let mut out: HashMap<String, SummaryUserTurnSizeBucket> = HashMap::new();
    for user_turn in user_turns {
        let (Some(preceding), Some(following)) = (
            user_turn.preceding_message_id.as_ref(),
            user_turn.following_message_id.as_ref(),
        ) else {
            continue;
        };
        let bucket = out
            .entry(summary_bridge_key(preceding, following))
            .or_default();
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

fn summary_bridge_key(preceding_message_id: &str, following_message_id: &str) -> String {
    format!("{preceding_message_id}\0{following_message_id}")
}

fn summary_tool_attribution_method(agg: &SummaryToolAgg) -> SummaryToolAttributionMethod {
    if agg.sized_cost == 0.0 && agg.even_split_cost == 0.0 {
        SummaryToolAttributionMethod::Unattributed
    } else if agg.sized_cost >= agg.even_split_cost {
        SummaryToolAttributionMethod::Sized
    } else {
        SummaryToolAttributionMethod::EvenSplit
    }
}

const SUMMARY_RELATIONSHIP_ORDER: [RelationshipType; 4] = [
    RelationshipType::Root,
    RelationshipType::Continuation,
    RelationshipType::Fork,
    RelationshipType::Subagent,
];

#[derive(Debug, Clone)]
struct SummaryRelationshipMatch {
    relationship_type: RelationshipType,
    session_id: String,
    subagent_type: Option<String>,
    turn_count: u64,
    cost: f64,
}

struct SummaryRelationshipTurnIndex<'a> {
    all_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    main_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    sidechain_by_session: HashMap<String, Vec<&'a TurnRecord>>,
    subagent_by_session_agent: HashMap<String, Vec<&'a TurnRecord>>,
}

fn summary_relationship_query_for_turn_slice(q: &Query) -> Query {
    Query {
        session_id: q.session_id.clone(),
        source: q.source,
        ..Default::default()
    }
}

fn match_summary_relationships_to_turns(
    relationships: &[SessionRelationshipRecord],
    turns: &[TurnRecord],
    pricing: &PricingTable,
) -> Vec<SummaryRelationshipMatch> {
    let index = build_summary_relationship_turn_index(turns);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for r in relationships {
        let key = summary_relationship_instance_key(r);
        if !seen.insert(key) {
            continue;
        }
        let matched_turns = summary_turns_for_relationship(r, &index);
        if matched_turns.is_empty() {
            continue;
        }
        let cost = matched_turns
            .iter()
            .map(|t| cost_for_turn(t, pricing).map(|c| c.total).unwrap_or(0.0))
            .sum();
        out.push(SummaryRelationshipMatch {
            relationship_type: r.relationship_type,
            session_id: r.session_id.clone(),
            subagent_type: summary_relationship_subagent_type(r, &matched_turns),
            turn_count: matched_turns.len() as u64,
            cost,
        });
    }
    out
}

fn build_summary_relationship_turn_index(turns: &[TurnRecord]) -> SummaryRelationshipTurnIndex<'_> {
    let mut index = SummaryRelationshipTurnIndex {
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
        if summary_is_main_thread_turn(turn) {
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
                    .entry(summary_session_agent_key(&turn.session_id, agent_id))
                    .or_default()
                    .push(turn);
            }
        }
    }
    index
}

fn summary_turns_for_relationship<'a>(
    r: &SessionRelationshipRecord,
    index: &'a SummaryRelationshipTurnIndex<'a>,
) -> Vec<&'a TurnRecord> {
    match r.relationship_type {
        RelationshipType::Root => index
            .main_by_session
            .get(&r.session_id)
            .cloned()
            .unwrap_or_default(),
        RelationshipType::Subagent => {
            if let Some(agent_id) = r.agent_id.as_ref().filter(|s| !s.is_empty()) {
                let key = summary_session_agent_key(&r.session_id, agent_id);
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

fn aggregate_summary_relationship_stats(
    matches: &[SummaryRelationshipMatch],
) -> Vec<SummaryRelationshipStats> {
    #[derive(Default)]
    struct RelationshipSessionRollup {
        relationship_count: u64,
        turn_count: u64,
        cost: f64,
    }

    let mut by_type: HashMap<RelationshipType, HashMap<String, RelationshipSessionRollup>> =
        HashMap::new();
    for m in matches {
        let by_session = by_type.entry(m.relationship_type).or_default();
        let current = by_session.entry(m.session_id.clone()).or_default();
        current.relationship_count += 1;
        current.turn_count += m.turn_count;
        current.cost += m.cost;
    }

    let mut out = Vec::new();
    for relationship_type in SUMMARY_RELATIONSHIP_ORDER {
        let Some(by_session) = by_type.get(&relationship_type) else {
            continue;
        };
        if by_session.is_empty() {
            continue;
        }
        let mut costs: Vec<f64> = by_session.values().map(|rollup| rollup.cost).collect();
        costs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let total_cost: f64 = costs.iter().sum();
        let session_count = by_session.len() as u64;
        out.push(SummaryRelationshipStats {
            relationship_type,
            count: by_session
                .values()
                .map(|rollup| rollup.relationship_count)
                .sum(),
            session_count,
            turn_count: by_session.values().map(|rollup| rollup.turn_count).sum(),
            total_cost,
            median_cost: summary_percentile(&costs, 0.5),
            p95_cost: summary_percentile(&costs, 0.95),
            mean_cost: if session_count > 0 {
                total_cost / session_count as f64
            } else {
                0.0
            },
        });
    }
    out
}

fn aggregate_summary_relationship_subagent_stats(
    matches: &[SummaryRelationshipMatch],
) -> Vec<SummaryRelationshipSubagentStats> {
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
        out.push(SummaryRelationshipSubagentStats {
            subagent_type,
            invocations,
            turns: agg.turns,
            total_cost: agg.total,
            median_cost: summary_percentile(&agg.costs, 0.5),
            p95_cost: summary_percentile(&agg.costs, 0.95),
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

fn summary_relationship_subagent_type(
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

fn summary_relationship_instance_key(r: &SessionRelationshipRecord) -> String {
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

fn summary_session_agent_key(session_id: &str, agent_id: &str) -> String {
    format!("{session_id}\0{agent_id}")
}

fn summary_is_main_thread_turn(turn: &TurnRecord) -> bool {
    match &turn.subagent {
        None => true,
        Some(sub) => !sub.is_sidechain || sub.agent_id.as_deref() == Some(&turn.session_id),
    }
}

fn summary_percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank =
        ((p * sorted.len() as f64).ceil() as i64 - 1).clamp(0, sorted.len() as i64 - 1) as usize;
    sorted[rank]
}

fn collect_summary_subagent_tree_relationships(
    ledger: &crate::ledger::Ledger,
    session_id: &str,
    q: &Query,
) -> Result<Vec<SessionRelationshipRecord>> {
    let relationships = ledger.query_relationships(&Query {
        source: q.source,
        ..Default::default()
    })?;
    Ok(collect_summary_connected_relationships(
        &relationships,
        session_id,
    ))
}

fn collect_summary_connected_relationships(
    relationships: &[SessionRelationshipRecord],
    session_id: &str,
) -> Vec<SessionRelationshipRecord> {
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, r) in relationships.iter().enumerate() {
        for id in summary_relationship_connected_ids(r) {
            if !id.is_empty() {
                by_id.entry(id).or_default().push(idx);
            }
        }
    }

    let mut out: IndexMap<String, SessionRelationshipRecord> = IndexMap::new();
    let mut seen_ids = HashSet::new();
    let mut queue = VecDeque::from([session_id.to_string()]);
    while let Some(id) = queue.pop_front() {
        if !seen_ids.insert(id.clone()) {
            continue;
        }
        let Some(rows) = by_id.get(&id) else {
            continue;
        };
        for idx in rows {
            let r = &relationships[*idx];
            for next in summary_relationship_connected_ids(r) {
                if !next.is_empty() && !seen_ids.contains(&next) {
                    queue.push_back(next);
                }
            }
            out.insert(summary_relationship_instance_key(r), r.clone());
        }
    }
    out.into_values().collect()
}

fn summary_relationship_connected_ids(r: &SessionRelationshipRecord) -> Vec<String> {
    let mut ids = vec![r.session_id.clone()];
    if let Some(related) = &r.related_session_id {
        ids.push(related.clone());
    }
    if let Some(agent) = &r.agent_id {
        ids.push(agent.clone());
    }
    ids
}

fn load_summary_subagent_tree_turns(
    ledger: &crate::ledger::Ledger,
    session_id: &str,
    relationships: &[SessionRelationshipRecord],
    q: &Query,
) -> Result<Vec<EnrichedTurn>> {
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

fn find_summary_tree_node<'a>(
    trees: impl IntoIterator<Item = &'a SubagentTreeNode>,
    node_id: &str,
) -> Option<SubagentTreeNode> {
    for root in trees {
        if let Some(found) = find_summary_node(root, node_id) {
            return Some(found.clone());
        }
    }
    None
}

fn find_summary_node<'a>(
    node: &'a SubagentTreeNode,
    node_id: &str,
) -> Option<&'a SubagentTreeNode> {
    if node.node_id == node_id {
        return Some(node);
    }
    for child in &node.children {
        if let Some(found) = find_summary_node(child, node_id) {
            return Some(found);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// session_cost
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCostOptions {
    pub session: Option<String>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCostResult {
    pub session_id: Option<String>,
    #[serde(rename = "totalUSD")]
    pub total_usd: f64,
    pub total_tokens: u64,
    pub turn_count: u64,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl LedgerHandle {
    pub fn session_cost(&self, opts: SessionCostOptions) -> Result<SessionCostResult> {
        let Some(session_id) = opts.session.clone() else {
            return Ok(SessionCostResult {
                session_id: None,
                total_usd: 0.0,
                total_tokens: 0,
                turn_count: 0,
                models: Vec::new(),
                note: Some("no session id provided".to_string()),
            });
        };
        let q = Query::for_session(&session_id);
        let turns = collect_turns(self, &q)?;
        if turns.is_empty() {
            return Ok(SessionCostResult {
                session_id: Some(session_id),
                total_usd: 0.0,
                total_tokens: 0,
                turn_count: 0,
                models: Vec::new(),
                note: Some("no turns recorded for this session yet".to_string()),
            });
        }
        let pricing = load_pricing(None);
        let mut models = std::collections::BTreeSet::new();
        let mut total_tokens: u64 = 0;
        let mut costs = Vec::with_capacity(turns.len());
        for t in &turns {
            models.insert(t.model.clone());
            let u = &t.usage;
            total_tokens += u.input
                + u.output
                + u.reasoning
                + u.cache_read
                + u.cache_create_5m
                + u.cache_create_1h;
            if let Some(c) = cost_for_turn(t, &pricing) {
                costs.push(c);
            }
        }
        let total = sum_costs(costs.iter());
        let total_usd = (total.total * 1_000_000.0).round() / 1_000_000.0;
        Ok(SessionCostResult {
            session_id: Some(session_id),
            total_usd,
            total_tokens,
            turn_count: turns.len() as u64,
            models: models.into_iter().collect(),
            note: None,
        })
    }
}

pub fn session_cost(opts: SessionCostOptions) -> Result<SessionCostResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.session_cost(SessionCostOptions {
        ledger_home: None,
        ..opts
    })
}

// ---------------------------------------------------------------------------
// inferences — per-API-call rollup (#434)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferencesOptions {
    /// Restrict to a single session. Required for the typical "show me
    /// the API-call timeline of session X" use case; cross-session
    /// fan-outs should call without it.
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub ledger_home: Option<PathBuf>,
}

impl LedgerHandle {
    /// Read per-API-call inferences (issue #434). One row per
    /// `(source, session_id, request_id)` triple — the unit a downstream
    /// "how many API calls" surface should consume rather than counting
    /// raw assistant turns (a multi-block Claude inference produces one
    /// `TurnRecord` already, but the inference key is the durable
    /// per-API-call identity even when the harness changes how it
    /// chunks rows).
    pub fn inferences(&self, opts: InferencesOptions) -> Result<Vec<crate::reader::Inference>> {
        let q = build_query(
            opts.session.as_deref(),
            opts.project.as_deref(),
            opts.since.as_deref(),
        )?;
        Ok(self.inner.query_inferences(&q)?)
    }
}

pub fn inferences(opts: InferencesOptions) -> Result<Vec<crate::reader::Inference>> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.inferences(InferencesOptions {
        ledger_home: None,
        ..opts
    })
}

// ---------------------------------------------------------------------------
// sessions_list
// ---------------------------------------------------------------------------

/// Default row cap when `SessionsListOptions::limit` is `None`. Picked to
/// match the "find a session to review" scroll budget — a tighter cap than
/// the typical agent's recent-session count, with `--limit` for callers
/// that want more.
pub const SESSIONS_LIST_DEFAULT_LIMIT: u64 = 20;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionsListOptions {
    /// Slice the ledger to events at or after this point. Same parser as
    /// every other verb's `since` (relative `24h`/`7d`/`4w`/`2m` or ISO).
    pub since: Option<String>,
    /// Restrict to a single project (matches `project` or `projectKey`).
    pub project: Option<String>,
    /// Case-insensitive substring filter against `session_id` and the
    /// resolved project label. Kept simple — FTS5 is not consulted here.
    pub grep: Option<String>,
    /// Row cap. Defaults to [`SESSIONS_LIST_DEFAULT_LIMIT`] when `None`.
    pub limit: Option<u64>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionListEntry {
    /// Full session id. Renderers should preserve this exactly.
    pub session_id: String,
    /// Project label (`project` if present, falling back to `projectKey`).
    /// `None` when neither field was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// ISO timestamp of the earliest turn within the filter window.
    pub started_at: String,
    /// ISO timestamp of the latest turn within the filter window.
    pub last_seen: String,
    pub turn_count: u64,
    #[serde(rename = "totalCostUSD")]
    pub total_cost_usd: f64,
    /// Distinct models observed in the session, sorted lexicographically.
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionsListResult {
    /// Sessions ordered by `last_seen` descending — most-recent first.
    pub sessions: Vec<SessionListEntry>,
    /// Effective row cap used for the response (the `limit` flag, defaulted).
    pub limit: u64,
    /// `true` when the underlying turn scan was truncated by `limit`. Lets
    /// callers tell "no more sessions" apart from "more exist; widen the
    /// cap to see them".
    pub truncated: bool,
}

impl LedgerHandle {
    /// Enumerate sessions in the ledger most-recent first. Derived from the
    /// `turns` table rather than `sessions` because the latter may be empty
    /// in older ledgers (the canonical source of truth is the per-turn rows
    /// every other read verb already trusts).
    pub fn sessions_list(&self, opts: SessionsListOptions) -> Result<SessionsListResult> {
        let limit = opts.limit.unwrap_or(SESSIONS_LIST_DEFAULT_LIMIT);
        let q = build_query(None, opts.project.as_deref(), opts.since.as_deref())?;
        let turns = collect_turns(self, &q)?;

        let pricing = load_pricing(None);
        // Aggregate per-session in a single pass over the turn stream.
        let mut acc: BTreeMap<String, SessionAccumulator> = BTreeMap::new();
        for turn in &turns {
            let entry = acc.entry(turn.session_id.clone()).or_default();
            entry.add_turn(turn, &pricing);
        }

        let needle = opts.grep.as_ref().map(|s| s.to_lowercase());
        let mut entries: Vec<SessionListEntry> = acc
            .into_iter()
            .map(|(session_id, acc)| acc.into_entry(session_id))
            .filter(|entry| match needle.as_deref() {
                None => true,
                Some(needle) => {
                    let project_match = entry
                        .project
                        .as_deref()
                        .map(|p| p.to_lowercase().contains(needle))
                        .unwrap_or(false);
                    project_match || entry.session_id.to_lowercase().contains(needle)
                }
            })
            .collect();

        // Most-recent first; tie-break on session_id for stable ordering when
        // two sessions share a last_seen ts (mostly tests, but worth pinning).
        entries.sort_by(|a, b| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });

        let truncated = entries.len() as u64 > limit;
        entries.truncate(limit as usize);

        Ok(SessionsListResult {
            sessions: entries,
            limit,
            truncated,
        })
    }
}

pub fn sessions_list(opts: SessionsListOptions) -> Result<SessionsListResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.sessions_list(SessionsListOptions {
        ledger_home: None,
        ..opts
    })
}

#[derive(Default)]
struct SessionAccumulator {
    started_at: Option<String>,
    last_seen: Option<String>,
    turn_count: u64,
    cost_total: f64,
    project: Option<String>,
    models: BTreeSet<String>,
}

impl SessionAccumulator {
    fn add_turn(&mut self, turn: &TurnRecord, pricing: &PricingTable) {
        self.turn_count += 1;
        match self.started_at.as_ref() {
            Some(cur) if cur.as_str() <= turn.ts.as_str() => {}
            _ => self.started_at = Some(turn.ts.clone()),
        }
        match self.last_seen.as_ref() {
            Some(cur) if cur.as_str() >= turn.ts.as_str() => {}
            _ => self.last_seen = Some(turn.ts.clone()),
        }
        if self.project.is_none() {
            // Mirror the resolution `Query.project` filters on so the rendered
            // column matches the value users would pass to `--project`.
            self.project = turn.project.clone().or_else(|| turn.project_key.clone());
        }
        self.models.insert(turn.model.clone());
        if let Some(c) = cost_for_turn(turn, pricing) {
            self.cost_total += c.total;
        }
    }

    fn into_entry(self, session_id: String) -> SessionListEntry {
        SessionListEntry {
            session_id,
            project: self.project,
            started_at: self.started_at.unwrap_or_default(),
            last_seen: self.last_seen.unwrap_or_default(),
            turn_count: self.turn_count,
            // Round to 6 decimals — same precision contract `session_cost`
            // uses, so the two surfaces are byte-comparable.
            total_cost_usd: (self.cost_total * 1_000_000.0).round() / 1_000_000.0,
            models: self.models.into_iter().collect(),
        }
    }
}

mod overhead;
pub use overhead::*;


// ---------------------------------------------------------------------------
// hotspots — discriminated union
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HotspotsGroupBy {
    Attribution,
    Bash,
    BashVerb,
    File,
    Subagent,
    Findings,
}

const DEFAULT_HOTSPOTS_FINDING_KINDS: &[&str] = &[
    "retry-loop",
    "failure-run",
    "cancellation-run",
    "compaction-loss",
    "edit-revert",
    "edit-heavy",
    "skill-recall-dup",
    "skill-pruning-protection",
    "system-prompt-tax",
    "ghost-surface",
    "tool-output-bloat",
    "tool-call-pattern",
];

fn default_hotspots_finding_kinds() -> Vec<String> {
    DEFAULT_HOTSPOTS_FINDING_KINDS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub group_by: Option<HotspotsGroupBy>,
    pub patterns: Option<Vec<String>>,
    /// Restrict to turns whose `enrichment.workflowId` matches.
    pub workflow: Option<String>,
    /// Restrict to turns whose derived provider is in the given set
    /// (case-insensitive). `None` / empty = no provider filter.
    pub provider: Option<Vec<String>>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsSessionTotal {
    pub session_id: String,
    pub grand_cost: f64,
    pub attributed_cost: f64,
    pub unattributed_cost: f64,
    pub attribution_method: AttributionMethod,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsFidelityBlock {
    pub analyzed: u64,
    pub excluded: u64,
    /// Aggregate fidelity summary for the matched-window turns. Stored as a
    /// `serde_json::Value` because older hotspot result shapes already exposed
    /// this JSON block directly.
    pub summary: serde_json::Value,
    pub refused: bool,
    /// Per-source coverage-gap breakdown. Computed in the same pass as the
    /// eligible/excluded split so CLI/MCP renderers don't need to re-walk the
    /// ledger to recover *which* sources contributed excluded turns. Not
    /// serialized — the JSON contract owns the aggregate counts above; this
    /// is an in-process renderer aid.
    #[serde(skip)]
    pub excluded_by_source: HotspotsExcludedBreakdown,
}

/// Per-source breakdown of turns that failed the hotspots coverage gate.
/// Sources are keyed by their wire string (e.g. `claude`, `codex`,
/// `opencode`) so the renderer can produce stable ordering without a second
/// ledger walk. See `HotspotsFidelityBlock::excluded_by_source`.
#[derive(Debug, Clone, Default)]
pub struct HotspotsExcludedBreakdown {
    pub sources: BTreeMap<String, HotspotsExcludedSourceRow>,
}

#[derive(Debug, Clone, Default)]
pub struct HotspotsExcludedSourceRow {
    pub count: u64,
    /// Distinct missing-coverage labels (e.g. `tool-call records`,
    /// `tool-result events`).
    pub missing: BTreeSet<String>,
    /// Distinct granularity buckets observed on excluded turns from this
    /// source.
    pub granularities: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum HotspotsResult {
    #[serde(rename = "attribution")]
    Attribution(Box<HotspotsAttributionResult>),
    #[serde(rename = "bash")]
    Bash {
        rows: Vec<BashAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "refusalReason"
        )]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "bash-verb")]
    BashVerb {
        rows: Vec<BashVerbAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "refusalReason"
        )]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "file")]
    File {
        rows: Vec<FileAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "refusalReason"
        )]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "subagent")]
    Subagent {
        rows: Vec<SubagentAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "refusalReason"
        )]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "findings")]
    Findings {
        findings: Vec<WasteFinding>,
        summary: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsAttributionResult {
    pub turns_analyzed: u64,
    pub grand_total: f64,
    pub attributed_total: f64,
    pub unattributed_total: f64,
    pub attribution_degraded: bool,
    pub sessions: Vec<HotspotsSessionTotal>,
    pub files: Vec<FileAggregation>,
    pub bash_verbs: Vec<BashVerbAggregation>,
    pub bash: Vec<BashAggregation>,
    pub subagents: Vec<SubagentAggregation>,
    pub mcp_servers: Vec<McpServerAggregation>,
    pub fidelity: HotspotsFidelityBlock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_reason: Option<String>,
}

impl LedgerHandle {
    pub fn hotspots(&self, opts: HotspotsOptions) -> Result<HotspotsResult> {
        let using_patterns = opts
            .patterns
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let mut q = build_query(
            opts.session.as_deref(),
            opts.project.as_deref(),
            opts.since.as_deref(),
        )?;
        if let Some(workflow) = opts.workflow.as_ref() {
            let mut enrichment = q.enrichment.unwrap_or_default();
            enrichment.insert("workflowId".to_string(), workflow.clone());
            q.enrichment = Some(enrichment);
        }
        let mut turns = collect_turns(self, &q)?;
        if let Some(filter) = normalize_provider_filter(opts.provider.clone()) {
            turns.retain(|t| {
                let provider = crate::analyze::provider_for(t).provider;
                filter.contains(&provider.to_ascii_lowercase())
            });
        }
        let pricing = load_pricing(None);

        if matches!(opts.group_by, Some(HotspotsGroupBy::Findings)) {
            let patterns = match opts.patterns {
                Some(patterns) if !patterns.is_empty() => patterns,
                _ => default_hotspots_finding_kinds(),
            };
            return run_hotspots_findings(self, &turns, &pricing, patterns, &q);
        }
        if using_patterns {
            return run_hotspots_findings(
                self,
                &turns,
                &pricing,
                opts.patterns.unwrap_or_default(),
                &q,
            );
        }
        run_hotspots_attribution(self, &turns, &pricing, opts.group_by, &q)
    }
}

pub fn hotspots(opts: HotspotsOptions) -> Result<HotspotsResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.hotspots(HotspotsOptions {
        ledger_home: None,
        ..opts
    })
}

fn run_hotspots_attribution(
    handle: &LedgerHandle,
    turns: &[TurnRecord],
    pricing: &PricingTable,
    group_by: Option<HotspotsGroupBy>,
    q: &Query,
) -> Result<HotspotsResult> {
    let mut eligible: Vec<TurnRecord> = Vec::new();
    let mut excluded: Vec<TurnRecord> = Vec::new();
    let mut excluded_by_source = HotspotsExcludedBreakdown::default();
    for t in turns {
        if turn_passes_hotspots_coverage(t) {
            eligible.push(t.clone());
        } else {
            record_excluded_source(&mut excluded_by_source, t);
            excluded.push(t.clone());
        }
    }
    let fidelity_summary = summarize_fidelity(turns);
    let summary_value = fidelity_summary_to_value(&fidelity_summary);

    if !turns.is_empty() && eligible.is_empty() {
        let refusal = format!(
            "{}/{} turns lack tool-call/tool-result coverage required for hotspots attribution",
            turns.len(),
            turns.len()
        );
        let group = group_by.unwrap_or(HotspotsGroupBy::Attribution);
        return Ok(refused_for_group(
            group,
            refusal,
            turns.len() as u64,
            summary_value,
            excluded_by_source,
        ));
    }

    let session_ids: HashSet<String> = eligible.iter().map(|t| t.session_id.clone()).collect();
    // Propagate `enrichment` (e.g. workflowId folds) into side queries so a
    // partial-session workflow stamp doesn't pull unrelated user-turns /
    // tool-result events into the per-session buckets and skew attribution
    // outside the requested slice.
    let side_q = Query {
        session_id: q.session_id.clone(),
        since: q.since.clone(),
        enrichment: q.enrichment.clone(),
        ..Default::default()
    };
    let user_turns_by_session = bucket_user_turns_by_session(handle, &side_q, Some(&session_ids))?;
    // Bytes plumbing (#436): hand attribute_hotspots a per-session lookup
    // so it can stamp `output_bytes` / `output_truncated` onto each
    // attribution row from the matching `ToolResultEventRecord`.
    let tool_result_events_by_session =
        bucket_tool_result_events_by_session(handle, &side_q, Some(&session_ids))?;

    let result = attribute_hotspots(
        &eligible,
        &AnalyzeHotspotsOptions {
            pricing,
            content_by_session: None,
            user_turns_by_session: Some(&user_turns_by_session),
            tool_result_events_by_session: Some(&tool_result_events_by_session),
        },
    );

    let group = group_by.unwrap_or(HotspotsGroupBy::Attribution);
    match group {
        HotspotsGroupBy::Bash => {
            return Ok(HotspotsResult::Bash {
                rows: aggregate_by_bash(&result.attributions),
                refused: None,
                refusal_reason: None,
            });
        }
        HotspotsGroupBy::BashVerb => {
            return Ok(HotspotsResult::BashVerb {
                rows: aggregate_by_bash_verb(&result.attributions, parse_bash_verb),
                refused: None,
                refusal_reason: None,
            });
        }
        HotspotsGroupBy::File => {
            return Ok(HotspotsResult::File {
                rows: aggregate_by_file(&result.attributions),
                refused: None,
                refusal_reason: None,
            });
        }
        HotspotsGroupBy::Subagent => {
            return Ok(HotspotsResult::Subagent {
                rows: aggregate_by_subagent(&result.attributions),
                refused: None,
                refusal_reason: None,
            });
        }
        HotspotsGroupBy::Findings => unreachable!("findings is handled before attribution"),
        HotspotsGroupBy::Attribution => {}
    }

    let files = aggregate_by_file(&result.attributions);
    let bash_verbs = aggregate_by_bash_verb(&result.attributions, parse_bash_verb);
    let bash = aggregate_by_bash(&result.attributions);
    let subagents = aggregate_by_subagent(&result.attributions);
    let mcp_servers = aggregate_by_mcp_server(&result.attributions);
    let even_split: usize = result
        .session_totals
        .iter()
        .filter(|s| matches!(s.attribution_method, AttributionMethod::EvenSplit))
        .count();
    let degraded = !result.session_totals.is_empty()
        && (even_split as f64 / result.session_totals.len() as f64) >= 0.5;

    let sessions = result
        .session_totals
        .into_iter()
        .map(|s| HotspotsSessionTotal {
            session_id: s.session_id,
            grand_cost: s.grand_cost,
            attributed_cost: s.attributed_cost,
            unattributed_cost: s.unattributed_cost,
            attribution_method: s.attribution_method,
        })
        .collect();

    Ok(HotspotsResult::Attribution(Box::new(
        HotspotsAttributionResult {
            turns_analyzed: eligible.len() as u64,
            grand_total: result.grand_total,
            attributed_total: result.attributed_total,
            unattributed_total: result.unattributed_total,
            attribution_degraded: degraded,
            sessions,
            files,
            bash_verbs,
            bash,
            subagents,
            mcp_servers,
            fidelity: HotspotsFidelityBlock {
                analyzed: eligible.len() as u64,
                excluded: excluded.len() as u64,
                summary: summary_value,
                refused: false,
                excluded_by_source,
            },
            refused: None,
            refusal_reason: None,
        },
    )))
}

/// Folds the coverage gap on `t` into the per-source breakdown. Mirrors
/// the CLI-side `describeExcluded` from `packages/cli/src/commands/hotspots.ts`
/// so callers can render the inline source clause without a second ledger
/// walk. Turns without `fidelity` are treated as best-effort full upstream
/// (`turn_passes_hotspots_coverage`) and never reach this function.
fn record_excluded_source(out: &mut HotspotsExcludedBreakdown, t: &TurnRecord) {
    let entry = out
        .sources
        .entry(t.source.wire_str().to_string())
        .or_default();
    entry.count += 1;
    if let Some(f) = t.fidelity.as_ref() {
        if !f.coverage.has_tool_calls {
            entry.missing.insert("tool-call records".to_string());
        }
        if !f.coverage.has_tool_result_events {
            entry.missing.insert("tool-result events".to_string());
        }
        entry
            .granularities
            .insert(f.granularity.wire_str().to_string());
    }
}

fn refused_for_group(
    group: HotspotsGroupBy,
    refusal: String,
    excluded_total: u64,
    summary_value: serde_json::Value,
    excluded_by_source: HotspotsExcludedBreakdown,
) -> HotspotsResult {
    match group {
        HotspotsGroupBy::Bash => HotspotsResult::Bash {
            rows: Vec::new(),
            refused: Some(true),
            refusal_reason: Some(refusal),
        },
        HotspotsGroupBy::BashVerb => HotspotsResult::BashVerb {
            rows: Vec::new(),
            refused: Some(true),
            refusal_reason: Some(refusal),
        },
        HotspotsGroupBy::File => HotspotsResult::File {
            rows: Vec::new(),
            refused: Some(true),
            refusal_reason: Some(refusal),
        },
        HotspotsGroupBy::Subagent => HotspotsResult::Subagent {
            rows: Vec::new(),
            refused: Some(true),
            refusal_reason: Some(refusal),
        },
        HotspotsGroupBy::Findings => HotspotsResult::Findings {
            findings: Vec::new(),
            summary: summary_value,
        },
        HotspotsGroupBy::Attribution => {
            HotspotsResult::Attribution(Box::new(HotspotsAttributionResult {
                turns_analyzed: 0,
                grand_total: 0.0,
                attributed_total: 0.0,
                unattributed_total: 0.0,
                attribution_degraded: false,
                sessions: Vec::new(),
                files: Vec::new(),
                bash_verbs: Vec::new(),
                bash: Vec::new(),
                subagents: Vec::new(),
                mcp_servers: Vec::new(),
                fidelity: HotspotsFidelityBlock {
                    analyzed: 0,
                    excluded: excluded_total,
                    summary: summary_value,
                    refused: true,
                    excluded_by_source,
                },
                refused: Some(true),
                refusal_reason: Some(refusal),
            }))
        }
    }
}

fn parse_bash_verb(command: &str) -> Option<BashParse> {
    parse_bash_command(command)
}

fn run_hotspots_findings(
    handle: &LedgerHandle,
    turns: &[TurnRecord],
    pricing: &PricingTable,
    wanted: Vec<String>,
    q: &Query,
) -> Result<HotspotsResult> {
    let wanted_set: HashSet<String> = wanted.into_iter().collect();
    let mut findings: Vec<WasteFinding> = Vec::new();

    // Propagate `enrichment` (e.g. workflowId folds) into side queries so a
    // partial-session workflow stamp doesn't pull unrelated user-turns /
    // tool-result events into the per-session buckets and skew attribution
    // outside the requested slice.
    let side_q = Query {
        session_id: q.session_id.clone(),
        since: q.since.clone(),
        enrichment: q.enrichment.clone(),
        ..Default::default()
    };

    let user_turns_all: Vec<UserTurnRecord> = handle.inner.query_user_turns(&side_q)?;
    let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
    for ut in &user_turns_all {
        user_turns_by_session
            .entry(ut.session_id.clone())
            .or_default()
            .push(ut.clone());
    }

    let detected = detect_patterns(
        turns,
        &DetectPatternsOptions {
            pricing,
            compactions: None,
            user_turns_by_session: Some(&user_turns_by_session),
            content_by_session: None,
            tool_result_events: None,
        },
    );
    for f in findings_from_patterns(&detected) {
        if wanted_set.contains(&f.kind) {
            findings.push(f);
        }
    }

    if wanted_set.contains("tool-output-bloat") {
        let mut settings: Vec<LoadedClaudeSettings> = Vec::new();
        if let Some(s) = load_claude_settings(user_claude_settings_path()) {
            settings.push(s);
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        if let Some(s) = load_claude_settings(project_claude_settings_path(&cwd)) {
            settings.push(s);
        }
        let tool_result_events = handle.inner.query_tool_result_events(&side_q)?;
        let bloats = detect_tool_output_bloat(&DetectToolOutputBloatOptions {
            settings: &settings,
            tool_result_events: &tool_result_events,
            user_turns: &user_turns_all,
            turns,
            pricing,
            threshold: None,
            min_occurrences: None,
        });
        for b in bloats {
            findings.push(tool_output_bloat_to_finding(&b));
        }
    }

    if wanted_set.contains("ghost-surface") {
        let inputs = build_ghost_surface_inputs(turns, pricing, None);
        let ghosts = detect_ghost_surface(&inputs);
        let options = GhostSurfaceFindingOptions::default();
        for g in ghosts {
            findings.push(ghost_surface_to_finding(&g, &options));
        }
    }

    if wanted_set.contains("tool-call-pattern") {
        let patterns = detect_tool_call_patterns(turns, &DetectToolCallPatternsOptions { pricing });
        for p in patterns {
            findings.push(tool_call_pattern_to_finding(&p));
        }
    }

    // `findings_from_patterns` already sorts the slice it returns, but the
    // tool-output-bloat / ghost-surface / tool-call-pattern batches above
    // are appended afterwards. Re-sort once so the global slice is
    // severity-descending → usdPerSession-descending end-to-end (TS parity).
    sort_findings(&mut findings);

    Ok(HotspotsResult::Findings {
        findings,
        summary: fidelity_summary_to_value(&summarize_fidelity(turns)),
    })
}

fn fidelity_summary_to_value(s: &FidelitySummary) -> serde_json::Value {
    // Mirror the TS shape: { total, byClass, byGranularity, missingCoverage,
    // unknown }. The analyze type doesn't derive Serialize so build it here.
    let by_class: serde_json::Map<String, serde_json::Value> = s
        .by_class
        .iter()
        .map(|(k, v)| {
            let key = serde_json::to_value(k)
                .ok()
                .and_then(|x| x.as_str().map(str::to_string))
                .unwrap_or_default();
            (key, serde_json::Value::from(*v))
        })
        .collect();
    let by_granularity: serde_json::Map<String, serde_json::Value> = s
        .by_granularity
        .iter()
        .map(|(k, v)| {
            let key = serde_json::to_value(k)
                .ok()
                .and_then(|x| x.as_str().map(str::to_string))
                .unwrap_or_default();
            (key, serde_json::Value::from(*v))
        })
        .collect();
    let missing: serde_json::Map<String, serde_json::Value> = s
        .missing_coverage
        .iter()
        .map(|(k, v)| ((*k).to_string(), serde_json::Value::from(*v)))
        .collect();
    serde_json::json!({
        "total": s.total,
        "byClass": serde_json::Value::Object(by_class),
        "byGranularity": serde_json::Value::Object(by_granularity),
        "missingCoverage": serde_json::Value::Object(missing),
        "unknown": s.unknown,
    })
}

mod compare;
pub use compare::*;


mod state;
pub use state::*;


mod flow;
pub use flow::*;


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
