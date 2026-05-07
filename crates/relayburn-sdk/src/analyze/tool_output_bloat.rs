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

/// Inverse of `bytes_to_tokens` (kept in lockstep). Used by Signal A to
/// surface a character-unit safe ceiling for `BASH_MAX_OUTPUT_LENGTH`, and
/// by the finding adapter so the paste fix is in the unit `settings.json`
/// speaks.
const BYTES_PER_TOKEN: u64 = 4;

/// Minimum number of oversized events before we surface a (source, tool)
/// bucket as a finding. A single oversized result is genuine waste, just
/// lower-severity.
const DEFAULT_MIN_OCCURRENCES: u64 = 1;

/// Minimum number of (sized) events before we trust a p95 to bound the
/// threshold. With fewer, the p95 collapses onto a single oversized event
/// and would self-exclude every flag, so we fall back to the static 15k
/// floor.
const P95_SAMPLE_FLOOR: usize = 20;

/// `bytes / 4` heuristic, rounded up. Matches `bytesToApproxTokens` in
/// `relayburn-reader::user_turn`.
fn bytes_to_tokens(bytes: u64) -> u64 {
    if bytes == 0 {
        return 0;
    }
    bytes.div_ceil(BYTES_PER_TOKEN)
}

const SEVERITY_HIGH_USD: f64 = 0.5;
const SEVERITY_WARN_USD: f64 = 0.05;

fn severity_from_usd(usd: f64) -> WasteSeverity {
    if usd >= SEVERITY_HIGH_USD {
        WasteSeverity::High
    } else if usd >= SEVERITY_WARN_USD {
        WasteSeverity::Warn
    } else {
        WasteSeverity::Info
    }
}

fn fmt_usd(n: f64) -> String {
    format!("${:.4}", n)
}

use super::util::format_with_commas;

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

/// Resolve the user-level settings file (`$HOME/.claude/settings.json`).
/// Honors `HOME` (POSIX) so tests can inject an isolated home dir; falls
/// back to `USERPROFILE` for parity with Node's `os.homedir()`.
pub fn user_claude_settings_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join(".claude").join("settings.json")
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
    let numeric_tokens = bytes_to_tokens(numeric_chars);
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
    // (`bytes_to_tokens` would clamp them to 0 anyway).
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
        Some(cl) if cl > 0 => Some(bytes_to_tokens(cl)),
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

impl<'a> DetectToolOutputBloatOptions<'a> {
    pub fn new(pricing: &'a PricingTable) -> Self {
        Self {
            settings: &[],
            tool_result_events: &[],
            user_turns: &[],
            turns: &[],
            pricing,
            threshold: None,
            min_occurrences: None,
        }
    }
}

pub fn detect_tool_output_bloat(opts: &DetectToolOutputBloatOptions<'_>) -> Vec<ToolOutputBloat> {
    let mut out: Vec<ToolOutputBloat> = Vec::new();
    if !opts.settings.is_empty() {
        out.extend(detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: opts.threshold,
            settings: opts.settings.to_vec(),
        }));
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
        let safe_chars = DEFAULT_BLOAT_TOKEN_THRESHOLD * BYTES_PER_TOKEN;
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
mod tests {
    use super::*;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::reader::{
        ToolCall, ToolResultEventSource, ToolResultStatus, Usage, UserTurnBlock,
    };
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn loaded(path: &str, env: serde_json::Value) -> LoadedClaudeSettings {
        let settings: ClaudeSettings = serde_json::from_value(json!({ "env": env })).unwrap();
        LoadedClaudeSettings {
            path: PathBuf::from(path),
            settings,
        }
    }

    fn loaded_no_env(path: &str) -> LoadedClaudeSettings {
        LoadedClaudeSettings {
            path: PathBuf::from(path),
            settings: ClaudeSettings::default(),
        }
    }

    fn evt(
        session_id: &str,
        tool_use_id: &str,
        event_index: u64,
        message_id: Option<&str>,
    ) -> ToolResultEventRecord {
        ToolResultEventRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.to_string(),
            message_id: message_id.map(String::from),
            tool_use_id: tool_use_id.to_string(),
            call_index: None,
            event_index,
            ts: None,
            status: ToolResultStatus::Completed,
            event_source: ToolResultEventSource::ToolResult,
            content_length: None,
            content_hash: None,
            is_error: None,
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn evt_with(
        source: SourceKind,
        session_id: &str,
        tool_use_id: &str,
        event_index: u64,
        message_id: Option<&str>,
        event_source: ToolResultEventSource,
        content_length: Option<u64>,
        call_index: Option<u64>,
    ) -> ToolResultEventRecord {
        ToolResultEventRecord {
            v: 1,
            source,
            session_id: session_id.to_string(),
            message_id: message_id.map(String::from),
            tool_use_id: tool_use_id.to_string(),
            call_index,
            event_index,
            ts: None,
            status: ToolResultStatus::Completed,
            event_source,
            content_length,
            content_hash: None,
            is_error: None,
            usage: None,
            usage_attribution: None,
            subagent_session_id: None,
            agent_id: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn user_turn_with(
        source: SourceKind,
        session_id: &str,
        user_uuid: &str,
        preceding: &str,
        following: &str,
        tool_use_id: &str,
        byte_len: u64,
        approx_tokens: u64,
    ) -> UserTurnRecord {
        UserTurnRecord {
            v: 1,
            source,
            session_id: session_id.to_string(),
            user_uuid: user_uuid.to_string(),
            ts: "2026-04-20T00:00:00.500Z".to_string(),
            preceding_message_id: Some(preceding.to_string()),
            following_message_id: Some(following.to_string()),
            blocks: vec![UserTurnBlock {
                kind: UserTurnBlockKind::ToolResult,
                tool_use_id: Some(tool_use_id.to_string()),
                byte_len,
                approx_tokens,
                is_error: None,
            }],
        }
    }

    fn turn_with(
        source: SourceKind,
        session_id: &str,
        message_id: &str,
        turn_index: u64,
        tool_calls: Vec<ToolCall>,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session_id.to_string(),
            session_path: None,
            message_id: message_id.to_string(),
            turn_index,
            ts: "2026-04-20T00:00:00.000Z".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 10,
                output: 5,
                reasoning: 0,
                cache_read: 100,
                cache_create_5m: 50,
                cache_create_1h: 0,
            },
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

    fn tc(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            target: None,
            args_hash: "hash".to_string(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    // -------------------------------------------------------------------
    // Signal A — static-config check
    // -------------------------------------------------------------------

    #[test]
    fn signal_a_flags_oversized_bash_max_output_length() {
        let settings = vec![loaded(
            "/home/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "80000" }),
        )];
        let out = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        });
        assert_eq!(out.len(), 1);
        let f = &out[0];
        assert_eq!(f.kind, ToolOutputBloatKind::StaticConfig);
        assert_eq!(f.source, SourceKind::ClaudeCode);
        assert_eq!(f.tool_name, "Bash");
        assert_eq!(f.configured_limit, Some(80_000));
        assert_eq!(f.evidenced_max_output, 20_000);
        assert_eq!(f.occurrence_count, 1);
        assert_eq!(f.cost, 0.0);
        assert_eq!(f.evidence, vec!["/home/u/.claude/settings.json".to_string()]);
    }

    #[test]
    fn signal_a_does_not_flag_at_threshold() {
        let settings = vec![loaded(
            "/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "60000" }),
        )];
        assert!(detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        })
        .is_empty());
    }

    #[test]
    fn signal_a_unit_conversion_under_threshold() {
        let settings = vec![loaded(
            "/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "50000" }),
        )];
        assert!(detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        })
        .is_empty());
    }

    #[test]
    fn signal_a_no_env_block() {
        let settings = vec![loaded_no_env("/u/.claude/settings.json")];
        assert!(detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        })
        .is_empty());
    }

    #[test]
    fn signal_a_project_overrides_user() {
        let settings = vec![
            loaded(
                "/u/.claude/settings.json",
                json!({ BASH_MAX_OUTPUT_ENV_KEY: "80000" }),
            ),
            loaded(
                "/cwd/.claude/settings.json",
                json!({ BASH_MAX_OUTPUT_ENV_KEY: "60000" }),
            ),
        ];
        assert!(detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        })
        .is_empty());
    }

    #[test]
    fn signal_a_project_path_reported_when_project_overrides_to_oversized() {
        let settings = vec![
            loaded(
                "/u/.claude/settings.json",
                json!({ BASH_MAX_OUTPUT_ENV_KEY: "15000" }),
            ),
            loaded(
                "/cwd/.claude/settings.json",
                json!({ BASH_MAX_OUTPUT_ENV_KEY: "99999" }),
            ),
        ];
        let out = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].evidence, vec!["/cwd/.claude/settings.json".to_string()]);
        assert_eq!(out[0].configured_limit, Some(99_999));
    }

    #[test]
    fn signal_a_honors_custom_threshold() {
        let settings = vec![loaded(
            "/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "5000" }),
        )];
        let tight = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: Some(1_000),
            settings: settings.clone(),
        });
        assert_eq!(tight.len(), 1);
        let loose = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: Some(10_000),
            settings,
        });
        assert!(loose.is_empty());
    }

    // -------------------------------------------------------------------
    // Filesystem loader
    // -------------------------------------------------------------------

    #[test]
    fn load_settings_returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        assert!(load_claude_settings(dir.path().join("nope.json")).is_none());
    }

    #[test]
    fn load_settings_returns_none_for_malformed_json() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("bad.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(load_claude_settings(&p).is_none());
    }

    #[test]
    fn load_settings_reads_env_block() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            json!({ "env": { BASH_MAX_OUTPUT_ENV_KEY: "80000" } }).to_string(),
        )
        .unwrap();
        let loaded = load_claude_settings(&p).expect("loads");
        assert_eq!(loaded.path, p);
        let env = loaded.settings.env.as_ref().expect("env present");
        assert_eq!(
            env.get(BASH_MAX_OUTPUT_ENV_KEY).and_then(|v| v.as_str()),
            Some("80000"),
        );
    }

    #[test]
    fn load_and_detect_end_to_end() {
        let dir = tempdir().unwrap();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let p = claude_dir.join("settings.json");
        std::fs::write(
            &p,
            json!({ "env": { BASH_MAX_OUTPUT_ENV_KEY: "80000" } }).to_string(),
        )
        .unwrap();
        let loaded = load_claude_settings(&p).expect("loads");
        let out = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings: vec![loaded],
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].configured_limit, Some(80_000));
    }

    // -------------------------------------------------------------------
    // Signal B — observed bloat across sessions
    // -------------------------------------------------------------------

    #[test]
    fn signal_b_flags_bash_above_threshold() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            80_000,
            20_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        let b = &out[0];
        assert_eq!(b.kind, ToolOutputBloatKind::ObservedBloat);
        assert_eq!(b.source, SourceKind::ClaudeCode);
        assert_eq!(b.tool_name, "Bash");
        assert_eq!(b.occurrence_count, 1);
        assert_eq!(b.evidenced_max_output, 20_000);
        assert_eq!(b.evidence, vec!["s1".to_string()]);
        assert!(b.cost > 0.0, "cost should be priced via the model rate");
    }

    #[test]
    fn signal_b_does_not_flag_below_threshold() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            40_000,
            10_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert!(out.is_empty());
    }

    #[test]
    fn signal_b_aggregates_into_single_bucket() {
        let pricing = load_builtin_pricing();
        let events = vec![
            evt("s1", "tu_a", 0, Some("m1")),
            evt("s2", "tu_b", 0, Some("m2")),
            evt("s3", "tu_c", 0, Some("m3")),
        ];
        let user_turns = vec![
            user_turn_with(SourceKind::ClaudeCode, "s1", "u1", "m1", "m2", "tu_a", 80_000, 20_000),
            user_turn_with(SourceKind::ClaudeCode, "s2", "u2", "m2", "m3", "tu_b", 100_000, 25_000),
            user_turn_with(SourceKind::ClaudeCode, "s3", "u3", "m3", "m4", "tu_c", 120_000, 30_000),
        ];
        let turns = vec![
            turn_with(SourceKind::ClaudeCode, "s1", "m1", 0, vec![tc("tu_a", "Bash")]),
            turn_with(SourceKind::ClaudeCode, "s2", "m2", 0, vec![tc("tu_b", "Bash")]),
            turn_with(SourceKind::ClaudeCode, "s3", "m3", 0, vec![tc("tu_c", "Bash")]),
        ];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        let b = &out[0];
        assert_eq!(b.occurrence_count, 3);
        assert_eq!(b.evidenced_max_output, 30_000);
        assert_eq!(b.evidence.len(), 3);
    }

    #[test]
    fn signal_b_emits_one_bucket_per_source_tool_pair() {
        let pricing = load_builtin_pricing();
        let events = vec![
            evt_with(
                SourceKind::ClaudeCode,
                "s1",
                "tu_a",
                0,
                Some("m1"),
                ToolResultEventSource::ToolResult,
                None,
                None,
            ),
            evt_with(
                SourceKind::Codex,
                "s2",
                "call_b",
                0,
                Some("m2"),
                ToolResultEventSource::ToolResult,
                None,
                None,
            ),
            evt_with(
                SourceKind::Opencode,
                "s3",
                "opc_c",
                0,
                Some("m3"),
                ToolResultEventSource::ToolResult,
                None,
                None,
            ),
        ];
        let user_turns = vec![
            user_turn_with(SourceKind::ClaudeCode, "s1", "u1", "m1", "m2", "tu_a", 80_000, 20_000),
            user_turn_with(SourceKind::Codex, "s2", "u2", "m2", "m3", "call_b", 90_000, 22_500),
            user_turn_with(SourceKind::Opencode, "s3", "u3", "m3", "m4", "opc_c", 85_000, 21_250),
        ];
        let turns = vec![
            turn_with(SourceKind::ClaudeCode, "s1", "m1", 0, vec![tc("tu_a", "Bash")]),
            turn_with(SourceKind::Codex, "s2", "m2", 0, vec![tc("call_b", "shell")]),
            turn_with(SourceKind::Opencode, "s3", "m3", 0, vec![tc("opc_c", "bash")]),
        ];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 3);
        let mut sources: Vec<SourceKind> = out.iter().map(|b| b.source).collect();
        sources.sort_by_key(|s| match s {
            SourceKind::ClaudeCode => 0,
            SourceKind::Codex => 1,
            SourceKind::Opencode => 2,
            _ => 3,
        });
        assert_eq!(
            sources,
            vec![SourceKind::ClaudeCode, SourceKind::Codex, SourceKind::Opencode]
        );
        for b in &out {
            assert_eq!(b.tool_name, "Bash");
        }
    }

    #[test]
    fn signal_b_skips_events_without_user_turn_blocks() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &[],
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert!(out.is_empty());
    }

    #[test]
    fn signal_b_honors_custom_threshold() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            4_000,
            1_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let def = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert!(def.is_empty());
        let tight = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: Some(500),
            min_occurrences: None,
        });
        assert_eq!(tight.len(), 1);
        assert_eq!(tight[0].evidenced_max_output, 1_000);
    }

    #[test]
    fn signal_b_falls_back_to_unknown_tool_name() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "orphan", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "orphan",
            80_000,
            20_000,
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &[],
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tool_name, "<unknown>");
        assert_eq!(out[0].cost, 0.0);
    }

    #[test]
    fn signal_b_does_not_double_count_carrier_plus_subagent_notification() {
        let pricing = load_builtin_pricing();
        let events = vec![
            evt_with(
                SourceKind::ClaudeCode,
                "s1",
                "tu_a",
                0,
                Some("m1"),
                ToolResultEventSource::ToolResult,
                None,
                Some(0),
            ),
            evt_with(
                SourceKind::ClaudeCode,
                "s1",
                "tu_a",
                1,
                Some("m1"),
                ToolResultEventSource::SubagentNotification,
                Some(200),
                Some(1),
            ),
        ];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            80_000,
            20_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].occurrence_count, 1);
        assert_eq!(out[0].evidenced_max_output, 20_000);
    }

    // -------------------------------------------------------------------
    // Top-level orchestration
    // -------------------------------------------------------------------

    #[test]
    fn orchestration_runs_both_signals() {
        let pricing = load_builtin_pricing();
        let settings = vec![loaded(
            "/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "80000" }),
        )];
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            80_000,
            20_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_tool_output_bloat(&DetectToolOutputBloatOptions {
            settings: &settings,
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 2);
        let mut kinds: Vec<ToolOutputBloatKind> = out.iter().map(|b| b.kind).collect();
        kinds.sort_by_key(|k| match k {
            ToolOutputBloatKind::ObservedBloat => 0,
            ToolOutputBloatKind::StaticConfig => 1,
        });
        assert_eq!(
            kinds,
            vec![
                ToolOutputBloatKind::ObservedBloat,
                ToolOutputBloatKind::StaticConfig,
            ]
        );
    }

    #[test]
    fn orchestration_signal_a_only() {
        let pricing = load_builtin_pricing();
        let settings = vec![loaded(
            "/u/.claude/settings.json",
            json!({ BASH_MAX_OUTPUT_ENV_KEY: "80000" }),
        )];
        let out = detect_tool_output_bloat(&DetectToolOutputBloatOptions {
            settings: &settings,
            tool_result_events: &[],
            user_turns: &[],
            turns: &[],
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, ToolOutputBloatKind::StaticConfig);
    }

    #[test]
    fn orchestration_signal_b_only() {
        let pricing = load_builtin_pricing();
        let events = vec![evt("s1", "tu_a", 0, Some("m1"))];
        let user_turns = vec![user_turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "u1",
            "m1",
            "m2",
            "tu_a",
            80_000,
            20_000,
        )];
        let turns = vec![turn_with(
            SourceKind::ClaudeCode,
            "s1",
            "m1",
            0,
            vec![tc("tu_a", "Bash")],
        )];
        let out = detect_tool_output_bloat(&DetectToolOutputBloatOptions {
            settings: &[],
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, ToolOutputBloatKind::ObservedBloat);
    }

    // -------------------------------------------------------------------
    // WasteFinding adapter
    // -------------------------------------------------------------------

    #[test]
    fn finding_adapter_signal_a_paste_targets_settings_json() {
        let f = tool_output_bloat_to_finding(&ToolOutputBloat {
            source: SourceKind::ClaudeCode,
            kind: ToolOutputBloatKind::StaticConfig,
            tool_name: "Bash".to_string(),
            configured_limit: Some(80_000),
            evidenced_max_output: 20_000,
            evidenced_p95_output: None,
            occurrence_count: 1,
            cost: 0.0,
            evidence: vec!["/u/.claude/settings.json".to_string()],
        });
        assert_eq!(f.kind, "tool-output-bloat");
        assert_eq!(f.actions.len(), 1);
        match &f.actions[0] {
            WasteAction::Paste { label, text } => {
                assert!(label.contains("settings.json"), "label: {label}");
                assert!(text.contains(BASH_MAX_OUTPUT_ENV_KEY), "text: {text}");
                assert!(text.contains("\"60000\""), "text should target 60000 chars: {text}");
            }
            other => panic!("expected Paste action, got {other:?}"),
        }
        assert_eq!(f.estimated_savings.tokens_per_session, Some(20_000));
    }

    #[test]
    fn finding_adapter_signal_b_emits_instruction_paste() {
        let f = tool_output_bloat_to_finding(&ToolOutputBloat {
            source: SourceKind::Codex,
            kind: ToolOutputBloatKind::ObservedBloat,
            tool_name: "shell".to_string(),
            configured_limit: None,
            evidenced_max_output: 25_000,
            evidenced_p95_output: Some(24_000),
            occurrence_count: 4,
            cost: 0.07,
            evidence: vec!["s1".to_string(), "s2".to_string()],
        });
        assert_eq!(f.kind, "tool-output-bloat");
        assert_eq!(f.severity, WasteSeverity::Warn);
        assert!(f.title.contains("codex shell"), "title: {}", f.title);
        assert!(f.title.contains("4×"), "title: {}", f.title);
        assert!(f.detail.contains("head"), "detail: {}", f.detail);
        assert!(f.detail.contains("tail"), "detail: {}", f.detail);
        assert!(f.detail.contains("grep"), "detail: {}", f.detail);
        assert!(matches!(f.actions[0], WasteAction::Paste { .. }));
    }

    // -------------------------------------------------------------------
    // Fixture-driven integration coverage
    // -------------------------------------------------------------------

    fn workspace_fixture(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join(rel)
    }

    #[test]
    fn fixture_settings_json_oversized_bash_output_length() {
        let path = workspace_fixture("claude/settings/oversized-bash-output-length.json");
        let loaded = load_claude_settings(&path).expect("fixture loads");
        let result = detect_static_config_bloat(&DetectStaticConfigBloatOptions {
            threshold: None,
            settings: vec![loaded],
        });
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].configured_limit, Some(80_000));
        assert_eq!(result[0].evidence, vec![path.to_string_lossy().into_owned()]);
    }

    #[test]
    fn fixture_claude_oversized_bash_output_enriched_path() {
        use crate::reader::{parse_claude_session, ClaudeParseOptions};
        let pricing = load_builtin_pricing();
        let path = workspace_fixture("claude/oversized-bash-output.jsonl");
        let parsed = parse_claude_session(&path, &ClaudeParseOptions::default()).expect("parses");
        // cl100k tokenizes repeated single-char content far below the
        // bytes/4 heuristic; we don't have cl100k wired here so the
        // detector falls back to bytes/4 either way. Use a low threshold
        // so the assertion still trips on the synthetic content.
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &parsed.tool_result_events,
            user_turns: &parsed.user_turns,
            turns: &parsed.turns,
            pricing: &pricing,
            threshold: Some(5_000),
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, SourceKind::ClaudeCode);
        assert_eq!(out[0].tool_name, "Bash");
        assert!(out[0].evidenced_max_output > 5_000);
    }

    #[test]
    fn fixture_claude_oversized_bash_output_content_length_fallback() {
        use crate::reader::{parse_claude_session, ClaudeParseOptions};
        let pricing = load_builtin_pricing();
        let path = workspace_fixture("claude/oversized-bash-output.jsonl");
        let parsed = parse_claude_session(&path, &ClaudeParseOptions::default()).expect("parses");
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &parsed.tool_result_events,
            user_turns: &[],
            turns: &parsed.turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, SourceKind::ClaudeCode);
        assert_eq!(out[0].tool_name, "Bash");
        assert!(out[0].evidenced_max_output >= DEFAULT_BLOAT_TOKEN_THRESHOLD);
    }

    #[test]
    fn fixture_codex_oversized_shell_output() {
        use crate::reader::{parse_codex_session, ParseCodexOptions};
        let pricing = load_builtin_pricing();
        let path = workspace_fixture("codex/oversized-shell-output.jsonl");
        let parsed = parse_codex_session(&path, &ParseCodexOptions::default()).expect("parses");
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &parsed.tool_result_events,
            user_turns: &parsed.user_turns,
            turns: &parsed.turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, SourceKind::Codex);
        // Codex `shell` normalizes to canonical `Bash`.
        assert_eq!(out[0].tool_name, "Bash");
        assert!(out[0].evidenced_max_output >= DEFAULT_BLOAT_TOKEN_THRESHOLD);
    }

    #[test]
    fn fixture_opencode_synthesized_bash() {
        let pricing = load_builtin_pricing();
        let events = vec![evt_with(
            SourceKind::Opencode,
            "ses_bloat",
            "opc_bash_1",
            0,
            Some("msg_bloat"),
            ToolResultEventSource::ToolResult,
            None,
            None,
        )];
        let user_turns = vec![user_turn_with(
            SourceKind::Opencode,
            "ses_bloat",
            "u_bloat",
            "msg_bloat",
            "msg_bloat_next",
            "opc_bash_1",
            80_000,
            20_000,
        )];
        let turns = vec![turn_with(
            SourceKind::Opencode,
            "ses_bloat",
            "msg_bloat",
            0,
            vec![tc("opc_bash_1", "bash")],
        )];
        let out = detect_observed_bloat(&DetectObservedBloatOptions {
            tool_result_events: &events,
            user_turns: &user_turns,
            turns: &turns,
            pricing: &pricing,
            threshold: None,
            min_occurrences: None,
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, SourceKind::Opencode);
        assert_eq!(out[0].tool_name, "Bash");
    }
}
