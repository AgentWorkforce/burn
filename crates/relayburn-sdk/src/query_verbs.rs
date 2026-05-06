//! Query verbs — `summary`, `session_cost`, `overhead`, `overhead_trim`,
//! `hotspots`. Rust port of the corresponding exports from
//! `packages/sdk/index.js`.
//!
//! Each verb appears as an `impl LedgerHandle` method (sync, returns
//! `anyhow::Result`) plus a free-function form that opens its own ledger
//! handle from `LedgerOpenOptions`. Free functions take `ledger_home:
//! Option<PathBuf>` so callers don't have to mutate process env to point
//! at a non-default ledger.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::analyze::{
    aggregate_by_bash, aggregate_by_bash_verb, aggregate_by_file, aggregate_by_subagent,
    attribute_hotspots, attribute_overhead, build_trim_recommendations, cost_for_turn,
    detect_patterns, detect_tool_call_patterns, detect_tool_output_bloat, find_overhead_files,
    findings_from_patterns, load_claude_settings, load_overhead_file, load_pricing,
    project_claude_settings_path, render_unified_diff_for_recommendation, summarize_fidelity,
    sum_costs, tool_call_pattern_to_finding, tool_output_bloat_to_finding,
    user_claude_settings_path, AttributeOverheadInput, AttributionMethod, BashAggregation,
    BashVerbAggregation, DetectPatternsOptions, DetectToolCallPatternsOptions,
    DetectToolOutputBloatOptions, FidelitySummary, FileAggregation,
    HotspotsOptions as AnalyzeHotspotsOptions, LoadedClaudeSettings, MarkdownSection,
    OverheadFile, OverheadFileKind, ParsedOverheadFile, PricingTable, SubagentAggregation,
    WasteFinding,
};
use crate::ledger::Query;
use crate::reader::{
    parse_bash_command, resolve_project, BashParse, SourceKind, TurnRecord, UserTurnRecord,
};

use crate::{Ledger, LedgerHandle, LedgerOpenOptions};

// ---------------------------------------------------------------------------
// since-string parsing
// ---------------------------------------------------------------------------

/// Accept either a CLI-style relative range (`24h`, `7d`, `4w`, `2m`) or an
/// ISO timestamp and return an ISO string the ledger query can compare. The
/// ledger filter does lexical compare on `turn.ts`, so passing a raw `7d`
/// would silently filter every turn out — same trap the TS sibling
/// (`packages/sdk/index.js`) protects against.
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
        let when = now.saturating_sub(secs_back);
        return Ok(Some(format_iso_z(when)));
    }

    // ISO-style: validate by checking the leading `YYYY-MM-DD` prefix. A
    // chrono-grade parser would be heavier than the gate needs — anything
    // beyond the date prefix the ledger compares lexically.
    if !looks_like_iso(raw) {
        anyhow::bail!(
            "invalid since: {raw} (expected ISO timestamp or relative range like 7d)"
        );
    }
    Ok(Some(raw.to_string()))
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

fn looks_like_iso(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10
        && b[0..4].iter().all(|c| c.is_ascii_digit())
        && b[4] == b'-'
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[7] == b'-'
        && b[8..10].iter().all(|c| c.is_ascii_digit())
}

fn system_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format Unix-seconds as `YYYY-MM-DDTHH:MM:SSZ`. Proleptic Gregorian — same
/// flavor of date math `relayburn-ingest::pending_stamps` uses to avoid a
/// chrono dep.
fn format_iso_z(secs: u64) -> String {
    let total_days = (secs / 86_400) as i64;
    let secs_in_day = (secs % 86_400) as u32;
    let hour = secs_in_day / 3_600;
    let minute = (secs_in_day / 60) % 60;
    let second = secs_in_day % 60;
    let (year, month, day) = days_to_ymd(total_days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn days_to_ymd(days_from_epoch: i64) -> (i64, u32, u32) {
    // Howard Hinnant's date-library algorithm (proleptic Gregorian).
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

fn build_query(
    session: Option<&str>,
    project: Option<&str>,
    since: Option<&str>,
) -> Result<Query> {
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

fn open_with(ledger_home: Option<&Path>) -> Result<LedgerHandle> {
    let opts = match ledger_home {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    Ledger::open(opts)
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
pub struct Summary {
    pub total_tokens: u64,
    pub total_cost: f64,
    pub turn_count: u64,
    pub by_tool: Vec<SummaryToolRow>,
    pub by_model: Vec<SummaryModelRow>,
}

impl LedgerHandle {
    pub fn summary(&self, opts: SummaryOptions) -> Result<Summary> {
        let q = build_query(
            opts.session.as_deref(),
            opts.project.as_deref(),
            opts.since.as_deref(),
        )?;
        let turns = collect_turns(self, &q)?;
        let pricing = load_pricing(None);
        Ok(compute_summary(&turns, &pricing))
    }
}

pub fn summary(opts: SummaryOptions) -> Result<Summary> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.summary(SummaryOptions {
        ledger_home: None,
        ..opts
    })
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
    }
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
// overhead + overhead_trim — share `gather_overhead`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadOptions {
    pub project: Option<PathBuf>,
    pub since: Option<String>,
    pub kind: Option<OverheadFileKind>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadSection {
    pub heading: String,
    pub start_line: u64,
    pub end_line: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadSectionCost {
    pub file_path: String,
    pub section: OverheadSection,
    pub token_share: f64,
    pub cost_per_session: f64,
    pub total_cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadAttributionDetail {
    pub session_count: u64,
    pub per_session_avg: f64,
    pub per_session_p95: f64,
    pub total_cost: f64,
    pub section_costs: Vec<OverheadSectionCost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadFileSummary {
    pub kind: OverheadFileKind,
    pub path: String,
    pub applies_to: Vec<SourceKind>,
    pub total_lines: u64,
    pub bytes: u64,
    pub tokens: u64,
    pub sections: Vec<MarkdownSection>,
    pub grouping_level: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadPerFileEntry {
    pub path: String,
    pub kind: OverheadFileKind,
    pub applies_to: Vec<SourceKind>,
    pub attribution: OverheadAttributionDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadResult {
    pub project: String,
    pub files: Vec<OverheadFileSummary>,
    pub per_file: Vec<OverheadPerFileEntry>,
    pub grand_total: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimOptions {
    pub project: Option<PathBuf>,
    pub since: Option<String>,
    pub kind: Option<OverheadFileKind>,
    pub ledger_home: Option<PathBuf>,
    pub top: Option<u64>,
    pub include_diff: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimSection {
    pub heading: String,
    pub start_line: u64,
    pub end_line: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimProjectedSavings {
    pub per_session_usd: f64,
    pub across_window_usd: f64,
    pub tokens: u64,
    pub token_share: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimRecommendation {
    pub file: String,
    pub kind: OverheadFileKind,
    pub applies_to: Vec<SourceKind>,
    pub section: OverheadTrimSection,
    pub projected_savings: OverheadTrimProjectedSavings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimSummary {
    pub files_analyzed: u64,
    pub files_with_recommendations: u64,
    pub total_recommendations: u64,
    pub total_projected_savings_per_session: f64,
    pub total_projected_savings_across_window: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimResult {
    pub project: String,
    pub since: String,
    pub recommendations: Vec<OverheadTrimRecommendation>,
    pub summary: OverheadTrimSummary,
}

struct GatheredOverhead {
    project_path: PathBuf,
    files: Vec<ParsedOverheadFile>,
    attribution: Option<crate::analyze::OverheadAttribution>,
}

fn gather_overhead(
    handle: &LedgerHandle,
    project: Option<&Path>,
    since: Option<&str>,
    kind: Option<OverheadFileKind>,
) -> Result<GatheredOverhead> {
    let project_path: PathBuf = match project {
        Some(p) => fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()),
        None => std::env::current_dir()?,
    };

    let mut found: Vec<OverheadFile> = find_overhead_files(&project_path);
    if let Some(want) = kind {
        found.retain(|f| f.kind == want);
    }
    if found.is_empty() {
        return Ok(GatheredOverhead {
            project_path,
            files: Vec::new(),
            attribution: None,
        });
    }

    let mut parsed_files: Vec<ParsedOverheadFile> = Vec::with_capacity(found.len());
    for f in found {
        parsed_files.push(load_overhead_file(f)?);
    }

    let resolved = resolve_project(&project_path.to_string_lossy());
    let q = Query {
        project: Some(resolved.project_key.unwrap_or(resolved.project)),
        since: normalize_since(since)?,
        ..Default::default()
    };
    let turns = collect_turns(handle, &q)?;
    let pricing = load_pricing(None);
    let attribution = attribute_overhead(AttributeOverheadInput {
        files: &parsed_files,
        turns: &turns,
        pricing: &pricing,
    });
    Ok(GatheredOverhead {
        project_path,
        files: parsed_files,
        attribution: Some(attribution),
    })
}

impl LedgerHandle {
    pub fn overhead(&self, opts: OverheadOptions) -> Result<OverheadResult> {
        let data = gather_overhead(
            self,
            opts.project.as_deref(),
            opts.since.as_deref(),
            opts.kind,
        )?;
        let project_str = data.project_path.to_string_lossy().into_owned();
        let Some(attribution) = data.attribution else {
            return Ok(OverheadResult {
                project: project_str,
                files: Vec::new(),
                per_file: Vec::new(),
                grand_total: 0.0,
            });
        };
        let files = data
            .files
            .iter()
            .map(|pf| OverheadFileSummary {
                kind: pf.file.kind,
                path: pf.file.path.clone(),
                applies_to: pf.file.applies_to.clone(),
                total_lines: pf.parsed.total_lines,
                bytes: pf.parsed.bytes,
                tokens: pf.parsed.tokens,
                sections: pf.parsed.sections.clone(),
                grouping_level: pf.parsed.grouping_level,
            })
            .collect();
        let per_file = attribution
            .per_file
            .iter()
            .map(|p| OverheadPerFileEntry {
                path: p.file.path.clone(),
                kind: p.file.kind,
                applies_to: p.file.applies_to.clone(),
                attribution: OverheadAttributionDetail {
                    session_count: p.attribution.session_count,
                    per_session_avg: p.attribution.per_session_avg,
                    per_session_p95: p.attribution.per_session_p95,
                    total_cost: p.attribution.total_cost,
                    section_costs: p
                        .attribution
                        .section_costs
                        .iter()
                        .map(|sc| OverheadSectionCost {
                            file_path: sc.file_path.clone(),
                            section: OverheadSection {
                                heading: sc.section.heading.clone(),
                                start_line: sc.section.start_line,
                                end_line: sc.section.end_line,
                                tokens: sc.section.tokens,
                            },
                            token_share: sc.token_share,
                            cost_per_session: sc.cost_per_session,
                            total_cost: sc.total_cost,
                        })
                        .collect(),
                },
            })
            .collect();
        Ok(OverheadResult {
            project: project_str,
            files,
            per_file,
            grand_total: attribution.grand_total,
        })
    }

    pub fn overhead_trim(&self, opts: OverheadTrimOptions) -> Result<OverheadTrimResult> {
        let since_label = opts
            .since
            .clone()
            .unwrap_or_else(|| "all time".to_string());
        let data = gather_overhead(
            self,
            opts.project.as_deref(),
            opts.since.as_deref(),
            opts.kind,
        )?;
        let project_str = data.project_path.to_string_lossy().into_owned();
        let top_n = parse_top_n(opts.top);
        let include_diff = opts.include_diff.unwrap_or(true);

        let Some(attribution) = data.attribution else {
            return Ok(OverheadTrimResult {
                project: project_str,
                since: since_label,
                recommendations: Vec::new(),
                summary: OverheadTrimSummary {
                    files_analyzed: 0,
                    files_with_recommendations: 0,
                    total_recommendations: 0,
                    total_projected_savings_per_session: 0.0,
                    total_projected_savings_across_window: 0.0,
                },
            });
        };

        let mut recommendations: Vec<OverheadTrimRecommendation> = Vec::new();
        let mut files_with_recs: u64 = 0;
        let mut text_cache: HashMap<String, String> = HashMap::new();

        for fa in &attribution.per_file {
            let recs = build_trim_recommendations(&fa.attribution, top_n);
            if recs.is_empty() {
                continue;
            }
            files_with_recs += 1;
            let file_text: Option<String> = if include_diff {
                if let Some(t) = text_cache.get(&fa.file.path) {
                    Some(t.clone())
                } else {
                    let read = fs::read_to_string(&fa.file.path)?;
                    text_cache.insert(fa.file.path.clone(), read.clone());
                    Some(read)
                }
            } else {
                None
            };
            for rec in &recs {
                let diff = if include_diff {
                    Some(render_unified_diff_for_recommendation(
                        &fa.file.path,
                        file_text.as_deref().unwrap_or(""),
                        rec,
                        Some(&data.project_path),
                    ))
                } else {
                    None
                };
                recommendations.push(OverheadTrimRecommendation {
                    file: to_project_relative(&fa.file.path, &data.project_path),
                    kind: fa.file.kind,
                    applies_to: fa.file.applies_to.clone(),
                    section: OverheadTrimSection {
                        heading: rec.section.heading.clone(),
                        start_line: rec.section.start_line,
                        end_line: rec.section.end_line,
                        tokens: rec.section.tokens,
                    },
                    projected_savings: OverheadTrimProjectedSavings {
                        per_session_usd: rec.projected_savings_per_session,
                        across_window_usd: rec.projected_savings_across_window,
                        tokens: rec.section.tokens,
                        token_share: rec.token_share,
                    },
                    diff,
                });
            }
        }

        let total_per_session: f64 = recommendations
            .iter()
            .map(|r| r.projected_savings.per_session_usd)
            .sum();
        let total_across_window: f64 = recommendations
            .iter()
            .map(|r| r.projected_savings.across_window_usd)
            .sum();

        Ok(OverheadTrimResult {
            project: project_str,
            since: since_label,
            summary: OverheadTrimSummary {
                files_analyzed: data.files.len() as u64,
                files_with_recommendations: files_with_recs,
                total_recommendations: recommendations.len() as u64,
                total_projected_savings_per_session: total_per_session,
                total_projected_savings_across_window: total_across_window,
            },
            recommendations,
        })
    }
}

pub fn overhead(opts: OverheadOptions) -> Result<OverheadResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.overhead(OverheadOptions {
        ledger_home: None,
        ..opts
    })
}

pub fn overhead_trim(opts: OverheadTrimOptions) -> Result<OverheadTrimResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.overhead_trim(OverheadTrimOptions {
        ledger_home: None,
        ..opts
    })
}

fn parse_top_n(v: Option<u64>) -> usize {
    match v {
        Some(n) if n > 0 => n as usize,
        _ => 3,
    }
}

fn to_project_relative(file_path: &str, project_path: &Path) -> String {
    let p = Path::new(file_path);
    match p.strip_prefix(project_path) {
        Ok(r) if !r.as_os_str().is_empty() => {
            r.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")
        }
        _ => file_path.replace(std::path::MAIN_SEPARATOR, "/"),
    }
}

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
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub group_by: Option<HotspotsGroupBy>,
    pub patterns: Option<Vec<String>>,
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
    /// Aggregate fidelity summary for the matched-window turns. The analyze
    /// `FidelitySummary` doesn't derive `Serialize`, so this trip through
    /// `serde_json::Value` keeps the wire shape stable.
    pub summary: serde_json::Value,
    pub refused: bool,
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
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "refusalReason")]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "bash-verb")]
    BashVerb {
        rows: Vec<BashVerbAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "refusalReason")]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "file")]
    File {
        rows: Vec<FileAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "refusalReason")]
        refusal_reason: Option<String>,
    },
    #[serde(rename = "subagent")]
    Subagent {
        rows: Vec<SubagentAggregation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "refusalReason")]
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
        let q = build_query(
            opts.session.as_deref(),
            opts.project.as_deref(),
            opts.since.as_deref(),
        )?;
        let turns = collect_turns(self, &q)?;
        let pricing = load_pricing(None);

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
    for t in turns {
        if turn_passes_hotspots_coverage(t) {
            eligible.push(t.clone());
        } else {
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
        ));
    }

    let session_ids: HashSet<String> = eligible.iter().map(|t| t.session_id.clone()).collect();
    let side_q = Query {
        session_id: q.session_id.clone(),
        since: q.since.clone(),
        ..Default::default()
    };
    let user_turns_by_session = bucket_user_turns_by_session(handle, &side_q, Some(&session_ids))?;

    let result = attribute_hotspots(
        &eligible,
        &AnalyzeHotspotsOptions {
            pricing,
            content_by_session: None,
            user_turns_by_session: Some(&user_turns_by_session),
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
        HotspotsGroupBy::Attribution => {}
    }

    let files = aggregate_by_file(&result.attributions);
    let bash_verbs = aggregate_by_bash_verb(&result.attributions, parse_bash_verb);
    let bash = aggregate_by_bash(&result.attributions);
    let subagents = aggregate_by_subagent(&result.attributions);
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
            fidelity: HotspotsFidelityBlock {
                analyzed: eligible.len() as u64,
                excluded: excluded.len() as u64,
                summary: summary_value,
                refused: false,
            },
            refused: None,
            refusal_reason: None,
        },
    )))
}

fn refused_for_group(
    group: HotspotsGroupBy,
    refusal: String,
    excluded_total: u64,
    summary_value: serde_json::Value,
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
        HotspotsGroupBy::Attribution => HotspotsResult::Attribution(Box::new(
            HotspotsAttributionResult {
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
                fidelity: HotspotsFidelityBlock {
                    analyzed: 0,
                    excluded: excluded_total,
                    summary: summary_value,
                    refused: true,
                },
                refused: Some(true),
                refusal_reason: Some(refusal),
            },
        )),
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

    let side_q = Query {
        session_id: q.session_id.clone(),
        since: q.since.clone(),
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

    // ghost-surface omitted: its TS sibling drives an async pipeline of
    // filesystem mining + synthetic-prompt deduction that goes well beyond
    // the ledger surface. Defer to a follow-up SDK PR.

    if wanted_set.contains("tool-call-pattern") {
        let patterns = detect_tool_call_patterns(turns, &DetectToolCallPatternsOptions { pricing });
        for p in patterns {
            findings.push(tool_call_pattern_to_finding(&p));
        }
    }

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

// ---------------------------------------------------------------------------
// state_status — derived-state report for `burn state status`
// ---------------------------------------------------------------------------

/// Per-table row counts in `burn.sqlite`. First-seen order of fields matches
/// the human-render layout the CLI emits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnDbRowCounts {
    pub turns: u64,
    pub user_turns: u64,
    pub compactions: u64,
    pub relationships: u64,
    pub tool_result_events: u64,
    pub sessions: u64,
    pub stamps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BurnDbStatus {
    pub path: String,
    pub exists: bool,
    pub rows: BurnDbRowCounts,
    pub total_rows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentDbStatus {
    pub path: String,
    pub exists: bool,
    pub rows: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStateStatus {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_built_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rebuild_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateConfigSummary {
    pub store: String,
    /// Numeric retention window in days, or `null` when retention is `forever`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<f64>,
    /// `true` iff retention is configured as `forever`.
    pub retention_forever: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateStatus {
    pub home: String,
    pub burn: BurnDbStatus,
    pub content: ContentDbStatus,
    pub archive: ArchiveStateStatus,
    pub config: StateConfigSummary,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateStatusOptions {
    pub ledger_home: Option<PathBuf>,
}

impl LedgerHandle {
    /// Compose a [`StateStatus`] report describing the on-disk layout of
    /// the open ledger: file paths/sizes for the two SQLite databases,
    /// per-table row counts in `burn.sqlite`, the row count in
    /// `content.sqlite`, the `archive_state` schema/last-built/last-rebuild
    /// fields, and the resolved [`crate::BurnConfig`].
    pub fn state_status(&self) -> Result<StateStatus> {
        let burn_path = self.inner.burn_path().to_path_buf();
        let content_path = self.inner.content_path().to_path_buf();

        // We deliberately don't report file sizes here. WAL checkpointing
        // grows the SQLite files in non-deterministic increments after
        // the first write transaction, so a size readout would drift
        // across runs even on a logically-empty ledger. Callers that
        // need disk-usage info should `du` the files directly.
        let burn_exists = fs::metadata(&burn_path).is_ok();
        let content_exists = fs::metadata(&content_path).is_ok();

        let rows = BurnDbRowCounts {
            turns: self.inner.count_table("turns")? as u64,
            user_turns: self.inner.count_table("user_turns")? as u64,
            compactions: self.inner.count_table("compactions")? as u64,
            relationships: self.inner.count_table("relationships")? as u64,
            tool_result_events: self.inner.count_table("tool_result_events")? as u64,
            sessions: self.inner.count_table("sessions")? as u64,
            stamps: self.inner.count_table("stamps")? as u64,
        };
        let total_rows = rows.turns
            + rows.user_turns
            + rows.compactions
            + rows.relationships
            + rows.tool_result_events
            + rows.sessions
            + rows.stamps;

        let archive = read_archive_state(&self.inner)?;
        let config = resolve_config_summary();

        // Render paths through the home directory if both share a common
        // ancestor. The CLI normalizer rewrites the absolute fixture path
        // to ${RELAYBURN_HOME}; keep them as plain strings here so the
        // structured output is faithful and the presenter does any
        // home-relative rewriting.
        let home = burn_path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        Ok(StateStatus {
            home,
            burn: BurnDbStatus {
                path: burn_path.to_string_lossy().into_owned(),
                exists: burn_exists,
                rows,
                total_rows,
            },
            content: ContentDbStatus {
                path: content_path.to_string_lossy().into_owned(),
                exists: content_exists,
                rows: self.inner.count_content()? as u64,
            },
            archive,
            config,
        })
    }
}

/// Free-function form of [`LedgerHandle::state_status`] — opens a ledger
/// from `opts.ledger_home` (or the env-var default) and returns the status.
pub fn state_status(opts: StateStatusOptions) -> Result<StateStatus> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.state_status()
}

fn read_archive_state(ledger: &crate::RawLedger) -> Result<ArchiveStateStatus> {
    // The archive_state row is created by `Ledger::open` (DDL inserts id=1
    // ON CONFLICT DO NOTHING), so this query is reliable. Reach through
    // the public `count_table` surface for schema_version by querying via
    // a small helper; rusqlite is exposed via the raw `Ledger` so we use
    // its connection directly through a query method.
    let json: String = ledger.read_archive_state_json()?;
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    struct Raw {
        schema_version: u32,
        #[serde(default)]
        last_built_at: Option<String>,
        #[serde(default)]
        last_rebuild_at: Option<String>,
    }
    let raw: Raw = serde_json::from_str(&json).map_err(|e| anyhow::anyhow!(e))?;
    Ok(ArchiveStateStatus {
        schema_version: raw.schema_version,
        last_built_at: raw.last_built_at,
        last_rebuild_at: raw.last_rebuild_at,
    })
}

fn resolve_config_summary() -> StateConfigSummary {
    let cfg = crate::ledger::load_config().unwrap_or_default();
    let store = match cfg.content.store {
        crate::reader::ContentStoreMode::Full => "full",
        crate::reader::ContentStoreMode::HashOnly => "hash-only",
        crate::reader::ContentStoreMode::Off => "off",
    }
    .to_string();
    match cfg.content.retention_days {
        crate::ledger::Retention::Forever => StateConfigSummary {
            store,
            retention_days: None,
            retention_forever: true,
        },
        crate::ledger::Retention::Days(d) => StateConfigSummary {
            store,
            retention_days: Some(d),
            retention_forever: false,
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{ToolCall, Usage};
    use tempfile::TempDir;

    fn fixture_handle() -> (TempDir, LedgerHandle) {
        let dir = tempfile::tempdir().unwrap();
        let opts = LedgerOpenOptions::with_home(dir.path());
        let mut handle = Ledger::open(opts).expect("open ledger");

        let turn1 = TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "sess-a".into(),
            session_path: None,
            message_id: "m-1".into(),
            turn_index: 0,
            ts: "2026-04-23T00:00:00.000Z".into(),
            model: "claude-sonnet-4-6".into(),
            project: Some("/tmp/proj".into()),
            project_key: None,
            usage: Usage {
                input: 1000,
                output: 500,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: vec![ToolCall {
                id: "tu-1".into(),
                name: "Read".into(),
                target: Some("/tmp/proj/foo.rs".into()),
                args_hash: "h1".into(),
                is_error: None,
                edit_pre_hash: None,
                edit_post_hash: None,
                skill_name: None,
                replaced_tools: None,
                collapsed_calls: None,
            }],
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        };
        let turn2 = TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "sess-a".into(),
            session_path: None,
            message_id: "m-2".into(),
            turn_index: 1,
            ts: "2026-04-23T00:01:00.000Z".into(),
            model: "claude-sonnet-4-6".into(),
            project: Some("/tmp/proj".into()),
            project_key: None,
            usage: Usage {
                input: 800,
                output: 400,
                reasoning: 0,
                cache_read: 200,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: vec![ToolCall {
                id: "tu-2".into(),
                name: "Read".into(),
                target: Some("/tmp/proj/foo.rs".into()),
                args_hash: "h1".into(),
                is_error: None,
                edit_pre_hash: None,
                edit_post_hash: None,
                skill_name: None,
                replaced_tools: None,
                collapsed_calls: None,
            }],
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        };
        handle
            .raw_mut()
            .append_turns(&[turn1, turn2])
            .expect("append turns");
        (dir, handle)
    }

    #[test]
    fn normalize_since_accepts_relative_ranges() {
        let v = normalize_since(Some("7d")).unwrap().unwrap();
        assert_eq!(v.len(), 20);
        assert!(v.ends_with('Z'));
    }

    #[test]
    fn normalize_since_passes_iso_through() {
        let iso = "2026-04-01T00:00:00Z";
        assert_eq!(normalize_since(Some(iso)).unwrap().as_deref(), Some(iso));
    }

    #[test]
    fn normalize_since_rejects_garbage() {
        assert!(normalize_since(Some("zzz")).is_err());
    }

    #[test]
    fn normalize_since_returns_none_for_empty() {
        assert!(normalize_since(None).unwrap().is_none());
        assert!(normalize_since(Some("")).unwrap().is_none());
    }

    #[test]
    fn summary_aggregates_two_turns() {
        let (_dir, handle) = fixture_handle();
        let s = handle.summary(SummaryOptions::default()).unwrap();
        assert_eq!(s.turn_count, 2);
        assert_eq!(s.total_tokens, 1000 + 500 + 800 + 400 + 200);
        assert_eq!(s.by_model.len(), 1);
        assert_eq!(s.by_model[0].model, "claude-sonnet-4-6");
        assert_eq!(s.by_tool.len(), 1);
        assert_eq!(s.by_tool[0].tool, "Read");
        assert_eq!(s.by_tool[0].count, 2);
        assert!(s.total_cost > 0.0);
    }

    #[test]
    fn summary_session_filter_narrows_to_session() {
        let (_dir, handle) = fixture_handle();
        let s = handle
            .summary(SummaryOptions {
                session: Some("nope".into()),
                ..SummaryOptions::default()
            })
            .unwrap();
        assert_eq!(s.turn_count, 0);
        assert_eq!(s.total_tokens, 0);
    }

    #[test]
    fn session_cost_returns_note_when_session_missing() {
        let (_dir, handle) = fixture_handle();
        let r = handle.session_cost(SessionCostOptions::default()).unwrap();
        assert!(r.session_id.is_none());
        assert_eq!(r.note.as_deref(), Some("no session id provided"));
        assert_eq!(r.turn_count, 0);
    }

    #[test]
    fn session_cost_aggregates_turns_for_known_session() {
        let (_dir, handle) = fixture_handle();
        let r = handle
            .session_cost(SessionCostOptions {
                session: Some("sess-a".into()),
                ..SessionCostOptions::default()
            })
            .unwrap();
        assert_eq!(r.session_id.as_deref(), Some("sess-a"));
        assert_eq!(r.turn_count, 2);
        assert_eq!(r.models, vec!["claude-sonnet-4-6".to_string()]);
        assert!(r.total_usd > 0.0);
        assert!(r.note.is_none());
    }

    #[test]
    fn session_cost_known_session_with_no_turns_emits_note() {
        let (_dir, handle) = fixture_handle();
        let r = handle
            .session_cost(SessionCostOptions {
                session: Some("ghost".into()),
                ..SessionCostOptions::default()
            })
            .unwrap();
        assert_eq!(r.session_id.as_deref(), Some("ghost"));
        assert_eq!(r.turn_count, 0);
        assert_eq!(
            r.note.as_deref(),
            Some("no turns recorded for this session yet")
        );
    }

    #[test]
    fn overhead_returns_empty_when_no_files_present() {
        let (_dir, handle) = fixture_handle();
        let project = tempfile::tempdir().unwrap();
        let r = handle
            .overhead(OverheadOptions {
                project: Some(project.path().to_path_buf()),
                ..OverheadOptions::default()
            })
            .unwrap();
        assert!(r.files.is_empty());
        assert!(r.per_file.is_empty());
        assert_eq!(r.grand_total, 0.0);
    }

    #[test]
    fn overhead_attributes_when_claude_md_present() {
        let (_dir, handle) = fixture_handle();
        let project = tempfile::tempdir().unwrap();
        let body = format!("## Section\n{}", "x".repeat(800));
        std::fs::write(project.path().join("CLAUDE.md"), &body).unwrap();
        let r = handle
            .overhead(OverheadOptions {
                project: Some(project.path().to_path_buf()),
                ..OverheadOptions::default()
            })
            .unwrap();
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.per_file.len(), 1);
        assert_eq!(r.files[0].kind, OverheadFileKind::ClaudeMd);
    }

    #[test]
    fn overhead_trim_emits_summary_when_claude_md_present() {
        let (_dir, handle) = fixture_handle();
        let project = tempfile::tempdir().unwrap();
        let body = format!(
            "## Big\n{}\n\n## Small\n{}\n",
            "y".repeat(8000),
            "z".repeat(200)
        );
        std::fs::write(project.path().join("CLAUDE.md"), &body).unwrap();
        let r = handle
            .overhead_trim(OverheadTrimOptions {
                project: Some(project.path().to_path_buf()),
                top: Some(1),
                ..OverheadTrimOptions::default()
            })
            .unwrap();
        // The fixture's turns have cache_read=0/200 — well below this
        // CLAUDE.md's token count — so attribution sees no rides and total
        // cost is 0. `build_trim_recommendations` still emits a top-N row
        // per non-preamble section, with projected savings = 0; that's the
        // contract. With `top=1` and two H2 sections in the file, we get
        // a single recommendation.
        assert_eq!(r.summary.files_analyzed, 1);
        assert_eq!(r.recommendations.len(), 1);
        assert_eq!(r.recommendations[0].projected_savings.per_session_usd, 0.0);
        assert!(r.recommendations[0].diff.is_some());
        assert_eq!(r.since, "all time");
    }

    #[test]
    fn hotspots_returns_attribution_shape_by_default() {
        let (_dir, handle) = fixture_handle();
        let r = handle.hotspots(HotspotsOptions::default()).unwrap();
        match r {
            HotspotsResult::Attribution(a) => {
                // Our turns lack `fidelity` (None), so the coverage gate
                // passes — both turns are eligible.
                assert_eq!(a.turns_analyzed, 2);
                assert!(a.grand_total >= 0.0);
                assert_eq!(a.fidelity.analyzed, 2);
                assert_eq!(a.fidelity.excluded, 0);
            }
            other => panic!("expected attribution, got {other:?}"),
        }
    }

    #[test]
    fn hotspots_group_by_file_returns_file_kind() {
        let (_dir, handle) = fixture_handle();
        let r = handle
            .hotspots(HotspotsOptions {
                group_by: Some(HotspotsGroupBy::File),
                ..HotspotsOptions::default()
            })
            .unwrap();
        match r {
            HotspotsResult::File { rows, refused, .. } => {
                assert!(refused.is_none());
                // Two `Read` calls on /tmp/proj/foo.rs collapse into 1 row.
                assert!(rows.len() <= 1);
            }
            other => panic!("expected file, got {other:?}"),
        }
    }

    #[test]
    fn hotspots_with_patterns_returns_findings_kind() {
        let (_dir, handle) = fixture_handle();
        let r = handle
            .hotspots(HotspotsOptions {
                patterns: Some(vec!["retry-loop".into()]),
                ..HotspotsOptions::default()
            })
            .unwrap();
        match r {
            HotspotsResult::Findings { findings, summary } => {
                // No retries in fixture, so findings is empty — but the
                // kind:findings shape and summary block should still ship.
                assert!(findings.is_empty());
                assert!(summary.is_object());
            }
            other => panic!("expected findings, got {other:?}"),
        }
    }

    #[test]
    fn free_function_summary_round_trips_through_ledger_home() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut handle = Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
            let t = TurnRecord {
                v: 1,
                source: SourceKind::ClaudeCode,
                session_id: "x".into(),
                session_path: None,
                message_id: "m".into(),
                turn_index: 0,
                ts: "2026-04-23T00:00:00.000Z".into(),
                model: "claude-sonnet-4-6".into(),
                project: None,
                project_key: None,
                usage: Usage {
                    input: 100,
                    output: 50,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                tool_calls: Vec::new(),
                files_touched: None,
                subagent: None,
                stop_reason: None,
                activity: None,
                retries: None,
                has_edits: None,
                fidelity: None,
            };
            handle.raw_mut().append_turns(&[t]).unwrap();
        }
        let s = summary(SummaryOptions {
            ledger_home: Some(dir.path().to_path_buf()),
            ..SummaryOptions::default()
        })
        .unwrap();
        assert_eq!(s.turn_count, 1);
        assert_eq!(s.total_tokens, 150);
    }

    #[test]
    fn state_status_reports_zero_rows_on_fresh_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let handle = Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
        let s = handle.state_status().unwrap();
        assert!(s.burn.exists);
        assert!(s.content.exists);
        assert_eq!(s.burn.rows.turns, 0);
        assert_eq!(s.burn.rows.user_turns, 0);
        assert_eq!(s.burn.rows.compactions, 0);
        assert_eq!(s.burn.rows.relationships, 0);
        assert_eq!(s.burn.rows.tool_result_events, 0);
        assert_eq!(s.burn.rows.sessions, 0);
        assert_eq!(s.burn.rows.stamps, 0);
        assert_eq!(s.burn.total_rows, 0);
        assert_eq!(s.content.rows, 0);
        assert_eq!(s.archive.schema_version, 1);
        assert!(s.archive.last_built_at.is_none());
        assert!(s.archive.last_rebuild_at.is_none());
    }

    #[test]
    fn state_status_counts_appended_turns_and_user_turns() {
        let (_dir, handle) = fixture_handle();
        let s = handle.state_status().unwrap();
        assert_eq!(s.burn.rows.turns, 2);
        assert_eq!(s.burn.total_rows, 2);
    }

    #[test]
    fn state_status_free_function_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _ = Ledger::open(LedgerOpenOptions::with_home(dir.path())).unwrap();
        }
        let s = state_status(StateStatusOptions {
            ledger_home: Some(dir.path().to_path_buf()),
        })
        .unwrap();
        assert!(s.burn.exists);
        assert_eq!(s.burn.total_rows, 0);
    }
}
