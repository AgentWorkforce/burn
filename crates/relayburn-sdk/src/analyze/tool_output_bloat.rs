//! Oversized tool-output bloat detector — Rust port of
//! `packages/analyze/src/tool-output-bloat.ts`. See AgentWorkforce/burn#271.
//!
//! Two signal sources unified under one detector shape:
//!
//!  - Signal A (Claude-only static config): read `~/.claude/settings.json`
//!    and the project's `.claude/settings.json`. The setting itself is in
//!    characters, so the parsed value is converted to tokens via the same
//!    `bytes/4` heuristic Signal B uses before comparing against the
//!    token-unit threshold (default 15000 tokens ≈ 60000 chars).
//!
//!  - Signal B (cross-harness session-data evidence): for every session,
//!    find `tool_result` events whose payload exceeds a threshold. Aggregate
//!    by `(source, toolName)` so detectors flag tools that consistently
//!    produce oversized output across many sessions, not single one-offs.
//!
//! Both signals emit the same [`ToolOutputBloat`] shape so the CLI can
//! render a single severity-ranked list.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::reader::types::UserTurnBlockKind;
use crate::reader::{
    normalize_tool_name, SourceKind, ToolResultEventRecord, ToolResultEventSource, TurnRecord,
    UserTurnRecord,
};

use crate::analyze::cost::lookup_model_rate;
use crate::analyze::findings::{EstimatedSavings, WasteAction, WasteFinding, WasteSeverity};
use crate::analyze::pricing::PricingTable;

/// Env-var key the static-config check fires on. Claude's harness exposes
/// this inside `.claude/settings.json` under `env.BASH_MAX_OUTPUT_LENGTH`.
pub const BASH_MAX_OUTPUT_ENV_KEY: &str = "BASH_MAX_OUTPUT_LENGTH";

/// Default token threshold for both signals. 15k tokens of `tool_result`
/// content rides in cache for every subsequent turn until compaction.
pub const DEFAULT_BLOAT_TOKEN_THRESHOLD: u64 = 15_000;

/// Minimum number of oversized events before we surface a (source, tool)
/// bucket as a finding. A single oversized result is genuine waste, just
/// lower-severity.
const DEFAULT_MIN_OCCURRENCES: u64 = 1;

/// Minimum number of (sized) events before we trust a p95 to bound the
/// threshold. With fewer, the p95 collapses onto a single oversized event
/// and would self-exclude every flag, so we fall back to the static 15k
/// floor.
const P95_SAMPLE_FLOOR: usize = 20;

use super::findings::severity_from_usd;
use super::util::{bytes_from_tokens, fmt_usd, format_with_commas, tokens_from_bytes};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolOutputBloatKind {
    StaticConfig,
    ObservedBloat,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutputBloat {
    pub source: SourceKind,
    pub kind: ToolOutputBloatKind,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_limit: Option<u64>,
    pub evidenced_max_output: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidenced_p95_output: Option<u64>,
    pub occurrence_count: u64,
    pub cost: f64,
    pub evidence: Vec<String>,
}

// ---------------------------------------------------------------------------
// Signal A — static config check
// ---------------------------------------------------------------------------

/// Subset of Claude's `.claude/settings.json` we care about. Unknown keys
/// are preserved in `extra` so round-tripping doesn't drop user data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct LoadedClaudeSettings {
    pub path: PathBuf,
    pub settings: ClaudeSettings,
}

/// Resolve the user-level settings file (`~/.claude/settings.json`). Home
/// resolution honors `HOME`, then `USERPROFILE` — see [`crate::util::home_dir`].
pub fn user_claude_settings_path() -> PathBuf {
    crate::util::home_dir()
        .join(".claude")
        .join("settings.json")
}

/// Resolve the project-level settings file relative to `cwd`. Project
/// settings override user settings (matching Claude's actual precedence).
pub fn project_claude_settings_path<P: AsRef<Path>>(cwd: P) -> PathBuf {
    cwd.as_ref().join(".claude").join("settings.json")
}

/// Read and parse a `.claude/settings.json` from disk. Returns `None` when
/// the file is missing or malformed — both cases mean "no setting to
/// check", indistinguishable from "no waste". Misconfigured user JSON must
/// not crash `burn hotspots`.
pub fn load_claude_settings<P: AsRef<Path>>(file_path: P) -> Option<LoadedClaudeSettings> {
    let path = file_path.as_ref();
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if !parsed.is_object() {
        return None;
    }
    let settings: ClaudeSettings = serde_json::from_value(parsed).ok()?;
    Some(LoadedClaudeSettings {
        path: path.to_path_buf(),
        settings,
    })
}

#[derive(Debug, Clone, Default)]
pub struct DetectStaticConfigBloatOptions {
    pub threshold: Option<u64>,
    /// Pre-loaded settings files in precedence order (lowest → highest).
    /// Project settings should appear AFTER user settings so the merge picks
    /// them up as the override.
    pub settings: Vec<LoadedClaudeSettings>,
}

/// Inspect the merged Claude env block and emit a `ToolOutputBloat` when
/// `BASH_MAX_OUTPUT_LENGTH` is above the threshold. Emits ONE finding per
/// run even when both user and project files set the value — the offending
/// value is the merged result and the actionable file is the one that
/// "won" the merge (project beats user).
pub fn detect_static_config_bloat(opts: &DetectStaticConfigBloatOptions) -> Vec<ToolOutputBloat> {
    let threshold = opts.threshold.unwrap_or(DEFAULT_BLOAT_TOKEN_THRESHOLD);

    let mut merged_value: Option<&str> = None;
    let mut source_path: Option<&Path> = None;
    for loaded in &opts.settings {
        let Some(env) = loaded.settings.env.as_ref() else {
            continue;
        };
        let Some(v) = env.get(BASH_MAX_OUTPUT_ENV_KEY) else {
            continue;
        };
        if let Some(s) = v.as_str() {
            if !s.is_empty() {
                merged_value = Some(s);
                source_path = Some(loaded.path.as_path());
            }
        }
    }
    let (raw, path) = match (merged_value, source_path) {
        (Some(v), Some(p)) => (v, p),
        _ => return Vec::new(),
    };
    let Ok(numeric_chars) = parse_int_lenient(raw) else {
        return Vec::new();
    };
    let numeric_tokens = tokens_from_bytes(numeric_chars);
    if numeric_tokens <= threshold {
        return Vec::new();
    }
    vec![ToolOutputBloat {
        source: SourceKind::ClaudeCode,
        kind: ToolOutputBloatKind::StaticConfig,
        tool_name: "Bash".to_string(),
        configured_limit: Some(numeric_chars),
        evidenced_max_output: numeric_tokens,
        evidenced_p95_output: None,
        occurrence_count: 1,
        cost: 0.0,
        evidence: vec![path.to_string_lossy().into_owned()],
    }]
}

/// Mimic JS `parseInt(s, 10)` enough for our inputs: trim leading
/// whitespace, then consume the optional sign and any leading digits.
/// Returns `Err(())` when no digits are found.
fn parse_int_lenient(s: &str) -> Result<u64, ()> {
    let trimmed = s.trim_start();
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    let mut negative = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        negative = bytes[i] == b'-';
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return Err(());
    }
    // Negative values are not meaningful here; treat as parse failure
    // (`tokens_from_bytes` would clamp them to 0 anyway).
    if negative {
        return Err(());
    }
    trimmed[start..i].parse::<u64>().map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Signal B — observed bloat across sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DetectObservedBloatOptions<'a> {
    pub tool_result_events: &'a [ToolResultEventRecord],
    pub user_turns: &'a [UserTurnRecord],
    pub turns: &'a [TurnRecord],
    pub pricing: &'a PricingTable,
    pub threshold: Option<u64>,
    pub min_occurrences: Option<u64>,
}

#[derive(Default)]
struct ToolUseLookup {
    /// `(source, sessionId, toolUseId)` -> tool name (post `normalize_tool_name`).
    tool_name_by_use_id: HashMap<(SourceKind, String, String), String>,
    /// `(source, sessionId, toolUseId)` -> approxTokens from user-turn blocks.
    approx_tokens_by_use_id: HashMap<(SourceKind, String, String), u64>,
    /// `(source, sessionId, messageId)` -> model.
    model_by_message_id: HashMap<(SourceKind, String, String), String>,
}

fn build_lookup(user_turns: &[UserTurnRecord], turns: &[TurnRecord]) -> ToolUseLookup {
    let mut lookup = ToolUseLookup::default();
    for ut in user_turns {
        for block in &ut.blocks {
            if block.kind != UserTurnBlockKind::ToolResult {
                continue;
            }
            let Some(tu) = block.tool_use_id.as_deref() else {
                continue;
            };
            if block.approx_tokens == 0 {
                continue;
            }
            lookup.approx_tokens_by_use_id.insert(
                (ut.source, ut.session_id.clone(), tu.to_string()),
                block.approx_tokens,
            );
        }
    }
    for t in turns {
        lookup.model_by_message_id.insert(
            (t.source, t.session_id.clone(), t.message_id.clone()),
            t.model.clone(),
        );
        for call in &t.tool_calls {
            if call.id.is_empty() {
                continue;
            }
            lookup.tool_name_by_use_id.insert(
                (t.source, t.session_id.clone(), call.id.clone()),
                call.name.clone(),
            );
        }
    }
    lookup
}

/// p95 of a slice of values using the nearest-rank definition. Empty → 0.
fn percentile(values: &[u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted: Vec<u64> = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len() as f64;
    let raw = (p / 100.0 * n).ceil() as i64 - 1;
    let idx = raw.clamp(0, sorted.len() as i64 - 1) as usize;
    sorted[idx]
}

fn price_carry_cost(tokens: u64, model: &str, pricing: &PricingTable) -> f64 {
    let Some(rate) = lookup_model_rate(model, pricing) else {
        return 0.0;
    };
    (tokens as f64 / 1_000_000.0) * rate.input
}

/// `tool_result` (Claude/Anthropic) and `function_call_output` (Codex) are
/// the canonical "carrier" events for a tool call. Other event types
/// (subagent_notification, queue_event, progress_event) can share the
/// same `toolUseId` but carry their own payloads.
fn is_carrier_event(e: &ToolResultEventRecord) -> bool {
    matches!(
        e.event_source,
        ToolResultEventSource::ToolResult | ToolResultEventSource::FunctionCallOutput
    )
}

fn size_event_tokens(e: &ToolResultEventRecord, lookup: &ToolUseLookup) -> Option<u64> {
    if is_carrier_event(e) {
        let key = (e.source, e.session_id.clone(), e.tool_use_id.clone());
        if let Some(&enriched) = lookup.approx_tokens_by_use_id.get(&key) {
            return Some(enriched);
        }
    }
    match e.content_length {
        Some(cl) if cl > 0 => Some(tokens_from_bytes(cl)),
        _ => None,
    }
}

fn first_model_for_session<'a>(
    turns: &'a [TurnRecord],
    source: SourceKind,
    session_id: &str,
) -> Option<&'a str> {
    for t in turns {
        if t.source == source && t.session_id == session_id {
            return Some(&t.model);
        }
    }
    None
}

pub fn detect_observed_bloat(opts: &DetectObservedBloatOptions<'_>) -> Vec<ToolOutputBloat> {
    let events = opts.tool_result_events;
    if events.is_empty() {
        return Vec::new();
    }
    let lookup = build_lookup(opts.user_turns, opts.turns);
    let min_occurrences = opts.min_occurrences.unwrap_or(DEFAULT_MIN_OCCURRENCES);

    let mut all_tokens: Vec<u64> = Vec::new();
    for e in events {
        if let Some(t) = size_event_tokens(e, &lookup) {
            if t > 0 {
                all_tokens.push(t);
            }
        }
    }
    let p95 = if all_tokens.len() >= P95_SAMPLE_FLOOR {
        percentile(&all_tokens, 95.0)
    } else {
        0
    };
    let threshold = opts
        .threshold
        .unwrap_or_else(|| DEFAULT_BLOAT_TOKEN_THRESHOLD.max(p95));

    struct Bucket {
        tokens: Vec<u64>,
        sessions: HashSet<String>,
        cost: f64,
    }
    let mut buckets: HashMap<(SourceKind, String), Bucket> = HashMap::new();
    for e in events {
        let Some(tokens) = size_event_tokens(e, &lookup) else {
            continue;
        };
        if tokens == 0 || tokens <= threshold {
            continue;
        }
        let use_key = (e.source, e.session_id.clone(), e.tool_use_id.clone());
        let raw_name = lookup.tool_name_by_use_id.get(&use_key);
        let tool_name = match raw_name {
            Some(name) => normalize_tool_name(name).to_string(),
            None => "<unknown>".to_string(),
        };
        let bucket_key = (e.source, tool_name);
        let bucket = buckets.entry(bucket_key).or_insert_with(|| Bucket {
            tokens: Vec::new(),
            sessions: HashSet::new(),
            cost: 0.0,
        });
        bucket.tokens.push(tokens);
        bucket.sessions.insert(e.session_id.clone());
        let model = e
            .message_id
            .as_deref()
            .and_then(|mid| {
                lookup
                    .model_by_message_id
                    .get(&(e.source, e.session_id.clone(), mid.to_string()))
                    .map(String::as_str)
            })
            .or_else(|| first_model_for_session(opts.turns, e.source, &e.session_id));
        if let Some(model) = model {
            bucket.cost += price_carry_cost(tokens, model, opts.pricing);
        }
    }

    let mut out: Vec<ToolOutputBloat> = Vec::new();
    for ((source, tool_name), bucket) in buckets {
        if (bucket.tokens.len() as u64) < min_occurrences {
            continue;
        }
        let max = *bucket.tokens.iter().max().unwrap_or(&0);
        let p95tokens = percentile(&bucket.tokens, 95.0);
        let mut evidence: Vec<String> = bucket.sessions.into_iter().collect();
        evidence.sort();
        out.push(ToolOutputBloat {
            source,
            kind: ToolOutputBloatKind::ObservedBloat,
            tool_name,
            configured_limit: None,
            evidenced_max_output: max,
            evidenced_p95_output: Some(p95tokens),
            occurrence_count: bucket.tokens.len() as u64,
            cost: bucket.cost,
            evidence,
        });
    }
    // Sort by cost desc so the worst offender lands first.
    out.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

// ---------------------------------------------------------------------------
// Top-level orchestration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DetectToolOutputBloatOptions<'a> {
    pub settings: &'a [LoadedClaudeSettings],
    pub tool_result_events: &'a [ToolResultEventRecord],
    pub user_turns: &'a [UserTurnRecord],
    pub turns: &'a [TurnRecord],
    pub pricing: &'a PricingTable,
    pub threshold: Option<u64>,
    pub min_occurrences: Option<u64>,
}

pub(crate) fn detect_tool_output_bloat(
    opts: &DetectToolOutputBloatOptions<'_>,
) -> Vec<ToolOutputBloat> {
    let mut out: Vec<ToolOutputBloat> = Vec::new();
    if !opts.settings.is_empty() {
        out.extend(detect_static_config_bloat(
            &DetectStaticConfigBloatOptions {
                threshold: opts.threshold,
                settings: opts.settings.to_vec(),
            },
        ));
    }
    if !opts.tool_result_events.is_empty() {
        out.extend(detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: opts.tool_result_events,
            user_turns: opts.user_turns,
            turns: opts.turns,
            pricing: opts.pricing,
            threshold: opts.threshold,
            min_occurrences: opts.min_occurrences,
        }));
    }
    out
}

// ---------------------------------------------------------------------------
// WasteFinding adapter
// ---------------------------------------------------------------------------

/// Adapt a [`ToolOutputBloat`] into the unified [`WasteFinding`] envelope so
/// the CLI's `--findings` table can render it next to retry-loops, failure
/// runs, etc. Static-config emits a paste suggestion targeting
/// `settings.json`; observed-bloat emits an instruction-file paste
/// suggesting `head` / `tail` / `grep` filtering before reading.
pub fn tool_output_bloat_to_finding(bloat: &ToolOutputBloat) -> WasteFinding {
    let session_id = bloat.evidence.first().cloned().unwrap_or_default();

    if bloat.kind == ToolOutputBloatKind::StaticConfig {
        let safe_chars = bytes_from_tokens(DEFAULT_BLOAT_TOKEN_THRESHOLD);
        let action = WasteAction::Paste {
            label: "Reduce in settings.json".to_string(),
            text: format!("\"{BASH_MAX_OUTPUT_ENV_KEY}\": \"{safe_chars}\""),
        };
        let configured_chars_fmt = match bloat.configured_limit {
            Some(c) => format_with_commas(c),
            None => "?".to_string(),
        };
        let configured_tokens = bloat.evidenced_max_output;
        let configured_tokens_fmt = format_with_commas(configured_tokens);
        let threshold_fmt = format_with_commas(DEFAULT_BLOAT_TOKEN_THRESHOLD);
        let safe_chars_fmt = format_with_commas(safe_chars);
        return WasteFinding {
            kind: "tool-output-bloat".to_string(),
            severity: WasteSeverity::Warn,
            session_id: session_id.clone(),
            title: format!(
                "{key} configured at {chars} chars (≈ {tokens} tokens, above {threshold})",
                key = BASH_MAX_OUTPUT_ENV_KEY,
                chars = configured_chars_fmt,
                tokens = configured_tokens_fmt,
                threshold = threshold_fmt,
            ),
            detail: format!(
                "Claude is configured to allow Bash tool output up to {chars} chars \
(≈ {tokens} tokens) per call. Above {threshold} tokens ({safe} chars) the tool_result rides as \
cached input on every subsequent turn until compaction, dominating the call's actual usefulness. \
Source file: {source}.",
                chars = configured_chars_fmt,
                tokens = configured_tokens_fmt,
                threshold = threshold_fmt,
                safe = safe_chars_fmt,
                source = session_id,
            ),
            estimated_savings: EstimatedSavings {
                tokens_per_session: Some(configured_tokens),
                ..Default::default()
            },
            actions: vec![action],
            event_source: None,
        };
    }

    // Observed bloat (Signal B).
    let usd_estimate = bloat.cost;
    let severity = severity_from_usd(usd_estimate);
    let threshold_fmt = format_with_commas(DEFAULT_BLOAT_TOKEN_THRESHOLD);
    let advice = format!(
        "Avoid dumping full {tool} output into context. Filter first with head / tail / grep \
(or page through with sed -n) so only the relevant slice rides in cache on subsequent turns. \
Tool results > {threshold} tokens persist as cached input on every subsequent turn until compaction.",
        tool = bloat.tool_name,
        threshold = threshold_fmt,
    );
    let action = WasteAction::Paste {
        label: "Add to CLAUDE.md / AGENTS.md".to_string(),
        text: format!(
            "When running {tool}, never dump full output into context. Filter first with \
`head -n 200`, `tail -n 200`, `grep <pattern>`, or paginate with `sed -n '1,200p'`. \
Each unfiltered tool_result above {threshold} tokens rides in cache on every subsequent turn until compaction.",
            tool = bloat.tool_name,
            threshold = threshold_fmt,
        ),
    };
    let p95_phrase = match bloat.evidenced_p95_output {
        Some(p) => format!("P95: {} tokens. ", format_with_commas(p)),
        None => String::new(),
    };
    let source_str = serde_json::to_value(bloat.source)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    WasteFinding {
        kind: "tool-output-bloat".to_string(),
        severity,
        session_id,
        title: format!(
            "Oversized {source} {tool} output: {count}× (max {max} tok)",
            source = source_str,
            tool = bloat.tool_name,
            count = bloat.occurrence_count,
            max = format_with_commas(bloat.evidenced_max_output),
        ),
        detail: format!(
            "{count} {source} {tool} tool_result event(s) exceeded the {threshold}-token threshold \
across {sessions} session(s). Largest payload: {max} tokens. {p95}\
Estimated next-turn carry cost {usd}. {advice}",
            count = bloat.occurrence_count,
            source = source_str,
            tool = bloat.tool_name,
            threshold = threshold_fmt,
            sessions = bloat.evidence.len(),
            max = format_with_commas(bloat.evidenced_max_output),
            p95 = p95_phrase,
            usd = fmt_usd(usd_estimate),
            advice = advice,
        ),
        estimated_savings: EstimatedSavings {
            tokens_per_session: Some(bloat.evidenced_max_output),
            usd_per_session: Some(usd_estimate),
            ..Default::default()
        },
        actions: vec![action],
        event_source: None,
    }
}

#[cfg(test)]
#[path = "tool_output_bloat_tests.rs"]
mod tests;
