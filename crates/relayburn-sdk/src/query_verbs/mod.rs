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
    deltas_for_session, detect_ghost_surface, detect_patterns, detect_tool_call_patterns,
    detect_tool_output_bloat, find_overhead_files, findings_from_patterns,
    ghost_surface_to_finding, has_minimum_fidelity, load_claude_settings, load_overhead_file,
    load_pricing, project_claude_settings_path, provider_for,
    render_unified_diff_for_recommendation, sort_findings, sum_costs, summarize_fidelity,
    summarize_fidelity_from_iter, summarize_replacement_savings, tally_unpriced,
    tool_call_pattern_to_finding, tool_output_bloat_to_finding, user_claude_settings_path,
    AggregateByProviderOptions, AttributeOverheadInput, AttributionMethod, BashAggregation,
    BashVerbAggregation, BuildSubagentTreeOptions, CompareOptions as AnalyzeCompareOptions,
    CompareTable, ComputeQualityOptions, ContextDelta, ContextDeltaOpts, CostBreakdown,
    CoverageField, DetectPatternsOptions, DetectToolCallPatternsOptions,
    DetectToolOutputBloatOptions, FidelitySummary, FieldCoverage, FileAggregation,
    GhostSurfaceFindingOptions, HotspotsOptions as AnalyzeHotspotsOptions, LoadedClaudeSettings,
    MarkdownSection, McpServerAggregation, OverheadFile, OverheadFileKind, OwnerRail,
    ParsedOverheadFile, PricingTable, ProviderAggregateRow, ProviderFilter, QualityResult,
    ReplacementSavingsSummary, RowCoverage, SessionClaudeMdCost, SubagentAggregation,
    SubagentTreeNode, SubagentTypeStats, ToolSavingsAggregate, TurnSpanTree, UsageCostAggregateRow,
    WasteFinding,
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

/// Adapt the `(whole seconds, millis)` shape used by the since/bucket paths to
/// the shared [`crate::util::time::format_iso_ms`] formatter.
fn format_iso_z_ms(secs: i64, millis: u32) -> String {
    crate::util::time::format_iso_ms(secs * 1_000 + millis as i64)
}

/// Range-checking wrapper over [`crate::util::time::ymd_to_days`]: rejects
/// out-of-range month/day (since this parses untrusted `since` strings),
/// then defers to the shared Hinnant primitive.
fn ymd_to_days(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(crate::util::time::ymd_to_days(year, month, day))
}

// ---------------------------------------------------------------------------
// time-bucketing — shared by `--bucket` on summary / compare / hotspots / overhead
// ---------------------------------------------------------------------------

/// Parse a `--bucket` duration into seconds.
///
/// Note: unlike `--since` (where `m` means *month*), bucket sizes use the
/// natural chart-axis grammar — `s`=seconds, `m`=minutes, `h`=hours, `d`=days,
/// `w`=weeks. A month-sized bucket is meaningless for a burn time-series, and
/// minute buckets (`5m`) are the common case (e.g. a "last 5 minutes" chart).
pub fn parse_bucket(s: &str) -> Result<u64> {
    bucket_secs_from_str(s).ok_or_else(|| {
        anyhow::anyhow!("invalid bucket: {s} (expected a duration like 30s, 5m, 1h, 12h, 1d, 7d)")
    })
}

fn bucket_secs_from_str(s: &str) -> Option<u64> {
    // Split on the final char (not the final byte) so a trailing multi-byte
    // UTF-8 unit can't land us on an invalid boundary and panic.
    let mut chars = s.chars();
    let unit = chars.next_back()?;
    let num = chars.as_str();
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = num.parse().ok()?;
    if n == 0 {
        return None;
    }
    match unit {
        's' => Some(n),
        'm' => n.checked_mul(60),
        'h' => n.checked_mul(3_600),
        'd' => n.checked_mul(86_400),
        'w' => n.checked_mul(7 * 86_400),
        _ => None,
    }
}

/// Parse the canonical ledger `ts` (`YYYY-MM-DDTHH:MM:SS(.mmm)Z`) into whole
/// seconds since the Unix epoch. Sub-second precision is dropped (fine for
/// bucket assignment). Returns `None` for malformed input.
pub(crate) fn iso_z_to_epoch_secs(ts: &str) -> Option<i64> {
    if ts.len() < 19 {
        return None;
    }
    let year: i64 = ts.get(0..4)?.parse().ok()?;
    let month: u32 = ts.get(5..7)?.parse().ok()?;
    let day: u32 = ts.get(8..10)?.parse().ok()?;
    let hour: i64 = ts.get(11..13)?.parse().ok()?;
    let minute: i64 = ts.get(14..16)?.parse().ok()?;
    let second: i64 = ts.get(17..19)?.parse().ok()?;
    let days = ymd_to_days(year, month, day)?;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Contiguous time buckets `[edges[i], edges[i+1])` covering `[anchor, end)`,
/// each `bucket_secs` wide. There are `edges.len() - 1` buckets; the final
/// edge is the first multiple of `bucket_secs` at or after `end`.
pub(crate) struct Buckets {
    pub(crate) bucket_secs: u64,
    edges: Vec<i64>,
}

impl Buckets {
    pub(crate) fn new(anchor_secs: i64, end_secs: i64, bucket_secs: u64) -> Self {
        let step = bucket_secs.max(1) as i64;
        let end = end_secs.max(anchor_secs + step); // always at least one bucket
        let mut edges = Vec::new();
        let mut e = anchor_secs;
        while e < end {
            edges.push(e);
            e += step;
        }
        edges.push(e);
        Self { bucket_secs, edges }
    }

    pub(crate) fn len(&self) -> usize {
        self.edges.len().saturating_sub(1)
    }

    pub(crate) fn start_iso(&self, i: usize) -> String {
        format_iso_z_ms(self.edges[i], 0)
    }

    pub(crate) fn end_iso(&self, i: usize) -> String {
        format_iso_z_ms(self.edges[i + 1], 0)
    }

    /// Bucket index for a `ts` epoch, or `None` if it falls outside
    /// `[anchor, last_edge)`.
    pub(crate) fn index_for(&self, ts_epoch: i64) -> Option<usize> {
        let anchor = *self.edges.first()?;
        let last = *self.edges.last()?;
        if ts_epoch < anchor || ts_epoch >= last {
            return None;
        }
        let idx = ((ts_epoch - anchor) / self.bucket_secs.max(1) as i64) as usize;
        Some(idx.min(self.len().saturating_sub(1)))
    }
}

/// Pick the bucket anchor: the normalized `--since` epoch when present, else
/// the earliest turn `ts` seen in `turn_ts`.
pub(crate) fn bucket_anchor_secs(
    since_norm: Option<&str>,
    turn_ts: impl Iterator<Item = i64>,
) -> Option<i64> {
    if let Some(secs) = since_norm.and_then(iso_z_to_epoch_secs) {
        return Some(secs);
    }
    turn_ts.min()
}

/// Upper bound on bucket count, so a tiny `--bucket` over an ancient `--since`
/// can't allocate millions of windows.
pub(crate) const MAX_BUCKETS: i64 = 10_000;

/// Reject a window that would span more than [`MAX_BUCKETS`] buckets. We fail
/// fast rather than silently moving the anchor forward: truncating the lower
/// bound would drop already-queried turns and break the invariant that
/// per-bucket totals reconcile with the un-bucketed `--since` total.
pub(crate) fn ensure_bucket_span(anchor: i64, end: i64, bucket_secs: u64) -> Result<()> {
    let max_span = MAX_BUCKETS.saturating_mul(bucket_secs.max(1) as i64);
    if end.saturating_sub(anchor) > max_span {
        anyhow::bail!(
            "--bucket would create more than {MAX_BUCKETS} buckets; use a wider --bucket or a narrower --since"
        );
    }
    Ok(())
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

mod summary;
pub use summary::*;

mod sessions;
pub use sessions::*;

mod overhead;
pub use overhead::*;

mod hotspots;
pub use hotspots::*;

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
