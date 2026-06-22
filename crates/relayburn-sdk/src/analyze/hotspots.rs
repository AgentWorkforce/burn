//! Per-tool cost attribution and aggregation — Rust port of
//! `packages/analyze/src/hotspots.ts`.
//!
//! Composes the per-tool attribution loop with file / bash / bash-verb /
//! subagent rollups. The math runs in `f64` to mirror TS `number`, and the
//! reduce order is preserved so per-tool USD totals match the TS
//! implementation within the 1e-9 USD precision contract that gates analyze
//! ports.

use std::collections::HashMap;

use crate::reader::{
    BashParse, ContentKind, ContentRecord, ToolResultEventRecord, TurnRecord, UserTurnBlockKind,
    UserTurnRecord,
};
use indexmap::IndexMap;
use phf::phf_set;
use serde::{Deserialize, Serialize};

use crate::analyze::cost::{cost_for_turn, lookup_model_rate, PER_MILLION};
use crate::analyze::pricing::PricingTable;
use crate::analyze::util::{
    group_turns_by_session_sorted, stringify_tool_result, tokens_from_utf16_len,
};

/// How a session's attribution loop allocated cost across tool calls.
///
/// `Sized` runs when at least one tool result has a known token size (from
/// user-turn `tool_result` blocks or content-sidecar estimation), so initial
/// and persistence costs flow proportionally by result size. `EvenSplit` is
/// the fallback when no per-result sizes are available — the next turn's
/// new-content cost is divided evenly across the prior emit's tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttributionMethod {
    /// At least one tool result had a known token size; cost was allocated
    /// proportionally by per-result token count.
    Sized,
    /// No per-result sizes were available; the paying turn's new-content cost
    /// was split evenly across the prior emit's tool calls.
    EvenSplit,
}

/// One row of attributed cost for a single tool call.
///
/// Each row captures the tool call's identity (id, name, target, args hash),
/// the session and turn it was emitted in, and the cost split between the
/// initial pay (charged on the next turn at *that* turn's model rate) and
/// the persistence cost accrued while the result rode along in subsequent
/// turns' `cacheRead`. `total_cost` is `initial_cost + persistence_cost`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAttribution {
    pub tool_use_id: String,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub args_hash: String,
    pub session_id: String,
    pub emit_turn_index: u64,
    pub emit_ts: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    pub result_tokens: u64,
    pub result_bytes_estimated: bool,
    /// Raw UTF-8 byte length of the tool result payload at ingest time
    /// (`as_bytes().len()`), pulled from
    /// `ToolResultEventRecord::output_bytes`. `None` when the per-call
    /// tool_result_event row was missing or pre-dates schema v2; the
    /// downstream aggregations treat `None` as 0 when summing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
    /// `Some(true)` when the ingest site detected a truncation marker in
    /// the payload (Claude Code only at the moment). `None` for events
    /// where truncation could not be determined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_truncated: Option<bool>,
    pub initial_cost: f64,
    pub initial_tokens: f64,
    pub persistence_cost: f64,
    pub persistence_tokens: f64,
    pub riding_turns: u64,
    pub total_cost: f64,
}

/// Per-session cost decomposition: how much of the session's grand total was
/// successfully attributed to tool calls, and what was left unattributed.
///
/// `grand_cost` routes through `cost_for_turn` so source-specific reasoning
/// billing semantics (e.g. Codex `included_in_output`) flow through. The
/// invariant `attributed_cost + unattributed_cost == grand_cost` holds within
/// the 1e-9 USD precision contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTotals {
    pub session_id: String,
    pub grand_cost: f64,
    pub attributed_cost: f64,
    pub unattributed_cost: f64,
    pub attribution_method: AttributionMethod,
}

/// Top-level output of [`attribute_hotspots`]: the flat list of per-tool
/// attribution rows plus the per-session and cross-session cost totals.
///
/// Aggregations (file, bash, bash-verb, subagent) are derived from
/// `attributions` via [`aggregate_by_file`], [`aggregate_by_bash`],
/// [`aggregate_by_bash_verb`], and [`aggregate_by_subagent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotspotsResult {
    pub attributions: Vec<ToolAttribution>,
    pub session_totals: Vec<SessionTotals>,
    pub grand_total: f64,
    pub attributed_total: f64,
    pub unattributed_total: f64,
}

/// Inputs to `attribute_hotspots`. `pricing` is required; the per-session
/// content / user-turn maps are optional and feed the sized attribution path.
pub struct HotspotsOptions<'a> {
    pub pricing: &'a PricingTable,
    /// Source-order content records keyed by session id. Surfaces tool_result
    /// payloads that the sized path estimates token counts from when
    /// `user_turns_by_session` doesn't already carry them.
    pub content_by_session: Option<&'a HashMap<String, Vec<ContentRecord>>>,
    /// Source-order `UserTurnRecord`s keyed by session id. Preferred sized
    /// source because user turns survive hash-only / off content-capture modes.
    pub user_turns_by_session: Option<&'a HashMap<String, Vec<UserTurnRecord>>>,
    /// Source-order `ToolResultEventRecord`s keyed by session id. Used to
    /// thread `output_bytes` / `output_truncated` per tool_use_id onto the
    /// emitted [`ToolAttribution`] rows so downstream aggregations can
    /// rank by raw payload size (#436). `None` skips the byte plumbing —
    /// callers that don't need bytes don't pay for the join.
    pub tool_result_events_by_session:
        Option<&'a HashMap<String, Vec<crate::reader::ToolResultEventRecord>>>,
}

/// File rollup: per-target totals across `Read | Edit | Write | NotebookEdit`
/// tool calls. Sorted by `total_cost` descending. `first_emit_ts` /
/// `first_emit_turn_index` track the earliest occurrence so callers can render
/// "first seen" timestamps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileAggregation {
    pub path: String,
    pub tool_call_count: u64,
    pub initial_tokens: f64,
    pub persistence_tokens: f64,
    pub riding_turns: u64,
    pub total_cost: f64,
    pub first_emit_ts: String,
    pub first_emit_turn_index: u64,
    /// Sum of `ToolAttribution::output_bytes` across calls in this group
    /// (#436). Calls whose tool_result_event row was missing or
    /// pre-schema-v2 contribute 0 here.
    pub total_output_bytes: u64,
    /// Largest single `output_bytes` value observed in this group. Useful
    /// for spotting one-shot 4MB Bash blowouts that get amortized away by
    /// `total_output_bytes / call_count`.
    pub max_output_bytes: u64,
    /// Number of calls whose `output_truncated` flag was `Some(true)`.
    pub truncated_count: u32,
}

/// Bash rollup: collapses repeated invocations by `args_hash` so identical
/// commands (same canonicalized argv) are folded into a single row. The
/// representative `command` is the first-seen literal for that hash. Sorted
/// by `total_cost` descending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashAggregation {
    pub args_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub call_count: u64,
    pub total_cost: f64,
    pub initial_tokens: f64,
    pub persistence_tokens: f64,
    /// Sum of `ToolAttribution::output_bytes` across calls (#436).
    pub total_output_bytes: u64,
    /// Largest single `output_bytes` value observed.
    pub max_output_bytes: u64,
    /// Calls whose `output_truncated` flag was `Some(true)`.
    pub truncated_count: u32,
}

/// Bash-verb rollup: groups bash invocations by their parsed verb (e.g.
/// `git`, `cargo test`). `distinct_commands` counts unique `args_hash` values
/// folded into the verb; `top_examples` carries the three highest-cost
/// representative commands (cost desc, then command asc as tiebreaker).
/// `avg_persistence_turns = riding_turns / call_count` (0 when no calls).
/// Verbs are sorted by `total_cost` desc, then `verb` asc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashVerbAggregation {
    pub verb: String,
    pub call_count: u64,
    pub distinct_commands: u64,
    pub total_cost: f64,
    pub initial_tokens: f64,
    pub persistence_tokens: f64,
    pub avg_persistence_turns: f64,
    pub top_examples: Vec<String>,
    /// Sum of `ToolAttribution::output_bytes` across calls in this verb
    /// bucket (#436).
    pub total_output_bytes: u64,
    /// Largest single `output_bytes` value observed in this bucket.
    pub max_output_bytes: u64,
    /// Calls whose `output_truncated` flag was `Some(true)`.
    pub truncated_count: u32,
}

/// Subagent rollup: groups `Agent` / `Task` spawns by their `subagent_type`
/// (resolved by the reader from the spawn's input payload). Calls without a
/// known type bucket under `"(unknown)"`. Sorted by `total_cost` descending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentAggregation {
    pub subagent_type: String,
    pub call_count: u64,
    pub total_cost: f64,
    pub initial_tokens: f64,
    pub persistence_tokens: f64,
    /// Sum of `ToolAttribution::output_bytes` across calls (#436).
    pub total_output_bytes: u64,
    /// Largest single `output_bytes` value observed.
    pub max_output_bytes: u64,
    /// Calls whose `output_truncated` flag was `Some(true)`.
    pub truncated_count: u32,
}

/// MCP-server rollup: groups any `mcp__<server>__<tool>` tool attribution by
/// `<server>` so a chatty MCP server (50+ distinct tools, none individually
/// expensive) shows up as a single row. `top_tools` carries up to three
/// representative tool basenames (cost desc, then name asc). Sorted by
/// `total_cost` descending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerAggregation {
    pub server: String,
    pub call_count: u64,
    pub initial_tokens: f64,
    pub persistence_tokens: f64,
    pub riding_turns: u64,
    pub total_cost: f64,
    pub top_tools: Vec<String>,
}

static FILE_TOOLS: phf::Set<&'static str> = phf_set! {
    "Read", "Edit", "Write", "NotebookEdit",
};

/// Attribute per-tool cost across `turns`, returning the flat attribution
/// list and per-session totals.
///
/// Sessions are processed in first-seen order; turns within each session are
/// stable-sorted by `turn_index`. The session's attribution method (`Sized`
/// vs `EvenSplit`) is selected by whether any tool result has a known token
/// size — see [`AttributionMethod`]. Initial cost is charged at the *paying*
/// turn's model rate using its `(input + cacheCreate)` mix; persistence cost
/// is allocated proportionally by result size against subsequent turns'
/// `cacheRead`, with a single result's size acting as the eviction threshold.
///
/// `grand_total` and per-session `grand_cost` route through `cost_for_turn`,
/// so anything outside the attributable surface (system prompts, reasoning
/// charged via Codex `included_in_output`, etc.) lands in `unattributed_*`.
pub fn attribute_hotspots(turns: &[TurnRecord], opts: &HotspotsOptions<'_>) -> HotspotsResult {
    // First-seen session ordering matches the TS `Map` iteration semantics.
    // Borrow turns rather than cloning — nothing below mutates them and the
    // input slice outlives every aggregation step.
    let by_session = group_turns_by_session_sorted(turns);

    let mut attributions: Vec<ToolAttribution> = Vec::new();
    let mut session_totals: Vec<SessionTotals> = Vec::new();
    let mut grand_total = 0.0_f64;
    let mut attributed_total = 0.0_f64;

    for (session_id, session_turns) in by_session.into_iter() {
        let session_content = opts.content_by_session.and_then(|m| m.get(&session_id));
        let tool_results_by_turn =
            session_content.map(|content| index_tool_results(content, &session_turns));

        let user_turns: &[UserTurnRecord] = opts
            .user_turns_by_session
            .and_then(|m| m.get(&session_id))
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let session_events: &[ToolResultEventRecord] = opts
            .tool_result_events_by_session
            .and_then(|m| m.get(&session_id))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let bytes_index = build_bytes_index(session_events);

        let session_result = attribute_session(
            &session_turns,
            opts.pricing,
            tool_results_by_turn.as_ref(),
            user_turns,
            &bytes_index,
        );

        let session_grand = session_result.grand_total;
        let session_attributed: f64 = session_result
            .attributions
            .iter()
            .map(|a| a.total_cost)
            .sum();
        let session_unattributed = session_grand - session_attributed;

        attributions.extend(session_result.attributions);
        session_totals.push(SessionTotals {
            session_id,
            grand_cost: session_grand,
            attributed_cost: session_attributed,
            unattributed_cost: session_unattributed,
            attribution_method: session_result.method,
        });
        grand_total += session_grand;
        attributed_total += session_attributed;
    }

    HotspotsResult {
        attributions,
        session_totals,
        grand_total,
        attributed_total,
        unattributed_total: grand_total - attributed_total,
    }
}

struct PerTurnContent {
    /// `tool_result` text keyed by `tool_use_id`.
    tool_result_text: HashMap<String, String>,
}

struct SessionAttribution {
    attributions: Vec<ToolAttribution>,
    method: AttributionMethod,
    /// Sum of `cost_for_turn` over the session's turns. Computed inside
    /// `attribute_session`'s turn loop so callers don't need a second pass.
    grand_total: f64,
}

/// Per-tool-use byte-payload metadata, lifted out of the session's
/// `tool_result_events` so the attribution loop doesn't re-scan that
/// vector for every tool call. First-write-wins on duplicate
/// `tool_use_id` keys — `index 0` carries the original payload; later
/// events on the same id are progress / subagent notifications whose
/// `output_bytes` would be noise.
#[derive(Debug, Default, Clone)]
struct ToolBytesEntry {
    output_bytes: Option<u64>,
    output_truncated: Option<bool>,
}

type ToolBytesIndex = HashMap<String, ToolBytesEntry>;

fn build_bytes_index(events: &[ToolResultEventRecord]) -> ToolBytesIndex {
    let mut out: ToolBytesIndex = HashMap::new();
    for ev in events {
        // Only the primary tool_result row carries the payload we care
        // about; SubagentNotification / ProgressEvent rows piggyback the
        // same tool_use_id but their output_bytes refers to the
        // notification body, not the original tool output. Skip them so
        // a notification's small payload doesn't overwrite the real one.
        let primary = matches!(
            ev.event_source,
            crate::reader::ToolResultEventSource::ToolResult
                | crate::reader::ToolResultEventSource::FunctionCallOutput
        );
        if !primary {
            continue;
        }
        let entry = out.entry(ev.tool_use_id.clone()).or_default();
        if entry.output_bytes.is_none() {
            entry.output_bytes = ev.output_bytes;
        }
        if entry.output_truncated.is_none() {
            entry.output_truncated = ev.output_truncated;
        }
    }
    out
}

fn attribute_session(
    turns: &[&TurnRecord],
    pricing: &PricingTable,
    tool_results_by_turn: Option<&HashMap<u64, PerTurnContent>>,
    user_turns: &[UserTurnRecord],
    bytes_by_tool_use_id: &ToolBytesIndex,
) -> SessionAttribution {
    if turns.is_empty() {
        return SessionAttribution {
            attributions: Vec::new(),
            method: AttributionMethod::EvenSplit,
            grand_total: 0.0,
        };
    }

    // Build the size index. User-turn blocks win over content-sidecar
    // estimates when both are available (see `prefers user-turn block sizes`).
    let mut size_by_tool_use_id: HashMap<String, u64> = HashMap::new();
    for ut in user_turns {
        for block in &ut.blocks {
            if block.kind != UserTurnBlockKind::ToolResult {
                continue;
            }
            let Some(tu) = block.tool_use_id.as_ref() else {
                continue;
            };
            size_by_tool_use_id.insert(tu.clone(), block.approx_tokens);
        }
    }
    if let Some(map) = tool_results_by_turn {
        for per_turn in map.values() {
            for (tu, text) in &per_turn.tool_result_text {
                if size_by_tool_use_id.contains_key(tu) {
                    continue;
                }
                size_by_tool_use_id.insert(tu.clone(), tokens_from_utf16_len(text));
            }
        }
    }

    let have_any_sizes = !size_by_tool_use_id.is_empty();
    let method = if have_any_sizes {
        AttributionMethod::Sized
    } else {
        AttributionMethod::EvenSplit
    };

    let mut attributions: Vec<ToolAttribution> = Vec::new();
    // Indices into `attributions` for tool_uses emitted on the prior turn
    // that haven't been charged initial cost yet. They pay at the next
    // iteration using the *paying* turn's model rate and (input + cacheCreate)
    // mix.
    let mut pending_initial: Vec<usize> = Vec::new();
    // Indices for results whose initial cost has already been paid; eligible
    // to ride along (persistence) on subsequent turns until the cacheRead
    // eviction signal drops them.
    let mut riding_active: Vec<usize> = Vec::new();
    let mut grand_total = 0.0_f64;

    for &turn in turns {
        let turn_rate = lookup_model_rate(&turn.model, pricing);

        // Accumulate the per-turn grand total in this same pass. Routes
        // through the canonical `cost_for_turn` so hotspots stays in
        // lock-step with `cost.rs` for source-specific reasoning billing
        // (Codex `included_in_output`, models with a separate reasoning
        // tariff, etc.).
        if let Some(b) = cost_for_turn(turn, pricing) {
            grand_total += b.total;
        }

        // 1) Initial cost: this turn pays for tool_results emitted on the
        //    previous turn. Use THIS turn's rate and (input/cacheCreate) mix
        //    — not the emit turn's.
        if !pending_initial.is_empty() {
            if let Some(rate) = turn_rate {
                let new_content = (turn.usage.input
                    + turn.usage.cache_create_5m
                    + turn.usage.cache_create_1h) as f64;
                if new_content > 0.0 {
                    let input_share = turn.usage.input as f64 / new_content;
                    let create_share = 1.0 - input_share;
                    let per_token_price =
                        input_share * rate.input + create_share * rate.cache_write;
                    if have_any_sizes {
                        let sibling_total: f64 = pending_initial
                            .iter()
                            .map(|&i| attributions[i].result_tokens as f64)
                            .sum();
                        if sibling_total > 0.0 {
                            // Cap at what turn N+1 actually paid for new
                            // content — otherwise multiple tool_results
                            // entering on the same turn could over-attribute
                            // past the actual paid total.
                            let cap = sibling_total.min(new_content);
                            for &i in &pending_initial {
                                let result_tokens_f = attributions[i].result_tokens as f64;
                                let tokens = (result_tokens_f / sibling_total) * cap;
                                let cost = (tokens / PER_MILLION) * per_token_price;
                                attributions[i].initial_cost = cost;
                                attributions[i].initial_tokens = tokens;
                                attributions[i].total_cost += cost;
                            }
                        }
                    } else {
                        // Even-split: with no per-result sizes, divide this
                        // turn's (input + cacheCreate) cost evenly across the
                        // prior emit's tool calls.
                        let k = pending_initial.len() as f64;
                        let tokens_per_call = new_content / k;
                        let cost_per_call = ((turn.usage.input as f64 / PER_MILLION) * rate.input
                            + ((turn.usage.cache_create_5m + turn.usage.cache_create_1h) as f64
                                / PER_MILLION)
                                * rate.cache_write)
                            / k;
                        for &i in &pending_initial {
                            attributions[i].initial_tokens = tokens_per_call;
                            attributions[i].initial_cost = cost_per_call;
                            attributions[i].total_cost += cost_per_call;
                        }
                    }
                }
            }
        }

        // 2) Persistence cost: each still-cached prior tool_result rides
        //    along in this turn's cacheRead. Allocate proportionally by size
        //    so the sum across active results never exceeds the actual
        //    cacheRead tokens. Eviction signal: a result drops out once the
        //    turn's cacheRead falls below that single result's size.
        if have_any_sizes && !riding_active.is_empty() && turn.usage.cache_read > 0 {
            if let Some(rate) = turn_rate {
                let still_cached: Vec<usize> = riding_active
                    .iter()
                    .copied()
                    .filter(|&i| {
                        let rt = attributions[i].result_tokens;
                        rt > 0 && turn.usage.cache_read >= rt
                    })
                    .collect();
                if !still_cached.is_empty() {
                    let active_total: f64 = still_cached
                        .iter()
                        .map(|&i| attributions[i].result_tokens as f64)
                        .sum();
                    let allocatable = (turn.usage.cache_read as f64).min(active_total);
                    for &i in &still_cached {
                        let rt = attributions[i].result_tokens as f64;
                        let tokens = (rt / active_total) * allocatable;
                        let cost = (tokens / PER_MILLION) * rate.cache_read;
                        attributions[i].persistence_tokens += tokens;
                        attributions[i].persistence_cost += cost;
                        attributions[i].total_cost += cost;
                        attributions[i].riding_turns += 1;
                    }
                }
            }
        }

        // 3) Promote yesterday's pendingInitial into the riding-active set,
        //    then emit attributions for this turn's own tool_uses (they'll
        //    pay initial next iteration).
        if !pending_initial.is_empty() {
            riding_active.append(&mut pending_initial);
        }
        for tc in &turn.tool_calls {
            let size_tokens = size_by_tool_use_id.get(&tc.id).copied().unwrap_or(0);
            // For Agent / Task spawns, identify the *spawned* subagent. The
            // spawning tool call's own input carries `subagent_type`, which
            // the reader's `pickTarget` resolves into `tc.target`.
            let subagent_type = if tc.name == "Agent" || tc.name == "Task" {
                tc.target.clone()
            } else {
                None
            };
            let bytes_entry = bytes_by_tool_use_id
                .get(&tc.id)
                .cloned()
                .unwrap_or_default();
            attributions.push(ToolAttribution {
                tool_use_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                target: tc.target.clone(),
                args_hash: tc.args_hash.clone(),
                session_id: turn.session_id.clone(),
                emit_turn_index: turn.turn_index,
                emit_ts: turn.ts.clone(),
                model: turn.model.clone(),
                project: turn.project.clone(),
                project_key: turn.project_key.clone(),
                subagent_type,
                result_tokens: size_tokens,
                result_bytes_estimated: have_any_sizes,
                output_bytes: bytes_entry.output_bytes,
                output_truncated: bytes_entry.output_truncated,
                initial_cost: 0.0,
                initial_tokens: 0.0,
                persistence_cost: 0.0,
                persistence_tokens: 0.0,
                riding_turns: 0,
                total_cost: 0.0,
            });
            pending_initial.push(attributions.len() - 1);
        }
    }

    SessionAttribution {
        attributions,
        method,
        grand_total,
    }
}

fn index_tool_results(
    content: &[ContentRecord],
    turns: &[&TurnRecord],
) -> HashMap<u64, PerTurnContent> {
    let mut by_turn: HashMap<u64, PerTurnContent> = HashMap::new();
    let mut turn_index_by_tool_use_id: HashMap<String, u64> = HashMap::new();
    for t in turns {
        for tc in &t.tool_calls {
            turn_index_by_tool_use_id.insert(tc.id.clone(), t.turn_index);
        }
    }
    for c in content {
        if c.kind != ContentKind::ToolResult {
            continue;
        }
        let Some(tr) = c.tool_result.as_ref() else {
            continue;
        };
        let Some(&idx) = turn_index_by_tool_use_id.get(&tr.tool_use_id) else {
            continue;
        };
        let bucket = by_turn.entry(idx).or_insert_with(|| PerTurnContent {
            tool_result_text: HashMap::new(),
        });
        let text = stringify_tool_result(&tr.content);
        bucket.tool_result_text.insert(tr.tool_use_id.clone(), text);
    }
    by_turn
}

/// Shared shape for the simple aggregations: filter attributions by a key
/// extractor, accumulate into a per-key row, and sort by `total_cost` desc.
/// `aggregate_by_bash_verb` does not use this because it tracks distinct
/// hashes and per-verb examples on top of the basic shape.
fn aggregate<K, R, KeyFn, InitFn, AccFn, CostFn>(
    attributions: &[ToolAttribution],
    key: KeyFn,
    init: InitFn,
    accumulate: AccFn,
    cost: CostFn,
) -> Vec<R>
where
    K: Eq + std::hash::Hash + Clone,
    KeyFn: Fn(&ToolAttribution) -> Option<K>,
    InitFn: Fn(&K, &ToolAttribution) -> R,
    AccFn: Fn(&mut R, &ToolAttribution),
    CostFn: Fn(&R) -> f64,
{
    let mut by_key: IndexMap<K, R> = IndexMap::new();
    for a in attributions {
        let Some(k) = key(a) else { continue };
        let row = by_key.entry(k.clone()).or_insert_with(|| init(&k, a));
        accumulate(row, a);
    }
    let mut out: Vec<R> = by_key.into_values().collect();
    out.sort_by(|a, b| cost(b).total_cmp(&cost(a)));
    out
}

/// Fold one tool call's output-byte metrics into a rollup row's running
/// `(total_output_bytes, max_output_bytes, truncated_count)` triple (#436).
/// Shared by the file / bash / bash-verb / subagent aggregations, which all
/// carry these three fields.
fn accumulate_output_bytes(
    total: &mut u64,
    max: &mut u64,
    truncated: &mut u32,
    a: &ToolAttribution,
) {
    let bytes = a.output_bytes.unwrap_or(0);
    *total = total.saturating_add(bytes);
    if bytes > *max {
        *max = bytes;
    }
    if a.output_truncated == Some(true) {
        *truncated = truncated.saturating_add(1);
    }
}

/// Roll up file-touching tool attributions (`Read | Edit | Write |
/// NotebookEdit`) by their target path. Rows missing or with an empty target
/// are skipped. Output is sorted by `total_cost` descending.
pub fn aggregate_by_file(attributions: &[ToolAttribution]) -> Vec<FileAggregation> {
    aggregate(
        attributions,
        |a| {
            if !FILE_TOOLS.contains(a.tool_name.as_str()) {
                return None;
            }
            match a.target.as_ref() {
                Some(t) if !t.is_empty() => Some(t.clone()),
                _ => None,
            }
        },
        |path, a| FileAggregation {
            path: path.clone(),
            tool_call_count: 0,
            initial_tokens: 0.0,
            persistence_tokens: 0.0,
            riding_turns: 0,
            total_cost: 0.0,
            first_emit_ts: a.emit_ts.clone(),
            first_emit_turn_index: a.emit_turn_index,
            total_output_bytes: 0,
            max_output_bytes: 0,
            truncated_count: 0,
        },
        |row, a| {
            row.tool_call_count += 1;
            row.initial_tokens += a.initial_tokens;
            row.persistence_tokens += a.persistence_tokens;
            row.riding_turns += a.riding_turns;
            row.total_cost += a.total_cost;
            accumulate_output_bytes(
                &mut row.total_output_bytes,
                &mut row.max_output_bytes,
                &mut row.truncated_count,
                a,
            );
            if a.emit_ts < row.first_emit_ts {
                row.first_emit_ts = a.emit_ts.clone();
                row.first_emit_turn_index = a.emit_turn_index;
            }
        },
        |row| row.total_cost,
    )
}

/// Roll up `Bash` tool attributions by `args_hash`, collapsing repeated
/// invocations of the same canonicalized command into a single row. The
/// representative `command` is the first-seen literal target. Output is
/// sorted by `total_cost` descending.
pub fn aggregate_by_bash(attributions: &[ToolAttribution]) -> Vec<BashAggregation> {
    aggregate(
        attributions,
        |a| (a.tool_name == "Bash").then(|| a.args_hash.clone()),
        |_, a| BashAggregation {
            args_hash: a.args_hash.clone(),
            command: a.target.clone(),
            call_count: 0,
            total_cost: 0.0,
            initial_tokens: 0.0,
            persistence_tokens: 0.0,
            total_output_bytes: 0,
            max_output_bytes: 0,
            truncated_count: 0,
        },
        |row, a| {
            row.call_count += 1;
            row.total_cost += a.total_cost;
            row.initial_tokens += a.initial_tokens;
            row.persistence_tokens += a.persistence_tokens;
            accumulate_output_bytes(
                &mut row.total_output_bytes,
                &mut row.max_output_bytes,
                &mut row.truncated_count,
                a,
            );
        },
        |row| row.total_cost,
    )
}

struct BashVerbAccumulator {
    verb: String,
    call_count: u64,
    total_cost: f64,
    initial_tokens: f64,
    persistence_tokens: f64,
    riding_turns: u64,
    total_output_bytes: u64,
    max_output_bytes: u64,
    truncated_count: u32,
    /// Distinct `args_hash` values seen for this verb. `IndexMap` preserves
    /// first-seen order for the example sort tiebreaker (insertion order
    /// before sorting by cost desc / command asc).
    hashes: IndexMap<String, ()>,
    /// `args_hash -> (command, total_cost)` for the per-verb example
    /// drill-down. Insertion order matches first-seen.
    examples: IndexMap<String, BashVerbExample>,
}

struct BashVerbExample {
    command: String,
    total_cost: f64,
}

/// Roll up `Bash` tool attributions by their parsed verb (e.g. `git`,
/// `cargo test`).
///
/// `parse` is the verb-extraction callback (typically the reader's bash
/// parser) — it receives the raw command string and returns the normalized
/// verb when one is recognized. Calls whose target the parser declines fall
/// into the `"(unknown)"` bucket. The per-verb `top_examples` field carries
/// up to three highest-cost representative commands (cost desc, then command
/// asc as tiebreaker). Output is sorted by `total_cost` desc, then `verb`
/// asc.
pub fn aggregate_by_bash_verb<F>(
    attributions: &[ToolAttribution],
    parse: F,
) -> Vec<BashVerbAggregation>
where
    F: Fn(&str) -> Option<BashParse>,
{
    let mut by_verb: IndexMap<String, BashVerbAccumulator> = IndexMap::new();
    for a in attributions {
        if a.tool_name != "Bash" {
            continue;
        }
        let parsed = a.target.as_deref().and_then(&parse);
        let verb_key = parsed
            .as_ref()
            .map(|p| p.normalized.clone())
            .unwrap_or_else(|| "(unknown)".to_string());
        let row = by_verb
            .entry(verb_key.clone())
            .or_insert_with(|| BashVerbAccumulator {
                verb: verb_key.clone(),
                call_count: 0,
                total_cost: 0.0,
                initial_tokens: 0.0,
                persistence_tokens: 0.0,
                riding_turns: 0,
                total_output_bytes: 0,
                max_output_bytes: 0,
                truncated_count: 0,
                hashes: IndexMap::new(),
                examples: IndexMap::new(),
            });
        row.call_count += 1;
        row.total_cost += a.total_cost;
        row.initial_tokens += a.initial_tokens;
        row.persistence_tokens += a.persistence_tokens;
        row.riding_turns += a.riding_turns;
        accumulate_output_bytes(
            &mut row.total_output_bytes,
            &mut row.max_output_bytes,
            &mut row.truncated_count,
            a,
        );
        row.hashes.insert(a.args_hash.clone(), ());

        let example = row
            .examples
            .entry(a.args_hash.clone())
            .or_insert_with(|| BashVerbExample {
                command: a.target.clone().unwrap_or_else(|| {
                    let prefix: String = a.args_hash.chars().take(8).collect();
                    format!("(hash {prefix})")
                }),
                total_cost: 0.0,
            });
        example.total_cost += a.total_cost;
    }

    let mut out: Vec<BashVerbAggregation> = by_verb
        .into_values()
        .map(|row| {
            let mut examples: Vec<BashVerbExample> = row.examples.into_values().collect();
            examples.sort_by(|a, b| {
                b.total_cost
                    .total_cmp(&a.total_cost)
                    .then_with(|| a.command.cmp(&b.command))
            });
            let top_examples: Vec<String> =
                examples.into_iter().take(3).map(|e| e.command).collect();
            BashVerbAggregation {
                verb: row.verb,
                call_count: row.call_count,
                distinct_commands: row.hashes.len() as u64,
                total_cost: row.total_cost,
                initial_tokens: row.initial_tokens,
                persistence_tokens: row.persistence_tokens,
                avg_persistence_turns: if row.call_count > 0 {
                    row.riding_turns as f64 / row.call_count as f64
                } else {
                    0.0
                },
                top_examples,
                total_output_bytes: row.total_output_bytes,
                max_output_bytes: row.max_output_bytes,
                truncated_count: row.truncated_count,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.total_cost
            .total_cmp(&a.total_cost)
            .then_with(|| a.verb.cmp(&b.verb))
    });
    out
}

/// Split an `mcp__<server>__<tool>` tool name into `(server, tool)`. Returns
/// `None` for any name that doesn't carry the `mcp__` prefix, has no server /
/// tool separator, or has an empty server or tool segment. Tool basenames may
/// themselves contain underscores; only the *first* `__` after the `mcp__`
/// prefix separates server from tool.
fn parse_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let sep = rest.find("__")?;
    let (server, tool) = (&rest[..sep], &rest[sep + 2..]);
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// Roll up `Agent` / `Task` spawn attributions by `subagent_type`. Spawns
/// without a resolved type bucket under `"(unknown)"`. Output is sorted by
/// `total_cost` descending.
pub fn aggregate_by_subagent(attributions: &[ToolAttribution]) -> Vec<SubagentAggregation> {
    aggregate(
        attributions,
        |a| {
            if a.tool_name != "Agent" && a.tool_name != "Task" {
                return None;
            }
            Some(
                a.subagent_type
                    .clone()
                    .unwrap_or_else(|| "(unknown)".to_string()),
            )
        },
        |key, _| SubagentAggregation {
            subagent_type: key.clone(),
            call_count: 0,
            total_cost: 0.0,
            initial_tokens: 0.0,
            persistence_tokens: 0.0,
            total_output_bytes: 0,
            max_output_bytes: 0,
            truncated_count: 0,
        },
        |row, a| {
            row.call_count += 1;
            row.total_cost += a.total_cost;
            row.initial_tokens += a.initial_tokens;
            row.persistence_tokens += a.persistence_tokens;
            accumulate_output_bytes(
                &mut row.total_output_bytes,
                &mut row.max_output_bytes,
                &mut row.truncated_count,
                a,
            );
        },
        |row| row.total_cost,
    )
}

struct McpServerAccumulator {
    server: String,
    call_count: u64,
    total_cost: f64,
    initial_tokens: f64,
    persistence_tokens: f64,
    riding_turns: u64,
    /// `tool basename -> (cost, first-seen-order via IndexMap)`. Insertion
    /// order is the example-sort tiebreaker before we sort by cost desc /
    /// name asc.
    tools: IndexMap<String, f64>,
}

/// Roll up any `mcp__<server>__<tool>` tool attribution by its server
/// segment so a chatty MCP server collapses into a single row. Non-MCP
/// tools (and malformed `mcp__…` names that fail to split into a
/// non-empty server + tool) are skipped. Output is sorted by `total_cost`
/// desc, then `server` asc as a stable tiebreaker.
pub fn aggregate_by_mcp_server(attributions: &[ToolAttribution]) -> Vec<McpServerAggregation> {
    let mut by_server: IndexMap<String, McpServerAccumulator> = IndexMap::new();
    for a in attributions {
        let Some((server, tool)) = parse_mcp_tool_name(&a.tool_name) else {
            continue;
        };
        let row = by_server
            .entry(server.to_string())
            .or_insert_with(|| McpServerAccumulator {
                server: server.to_string(),
                call_count: 0,
                total_cost: 0.0,
                initial_tokens: 0.0,
                persistence_tokens: 0.0,
                riding_turns: 0,
                tools: IndexMap::new(),
            });
        row.call_count += 1;
        row.total_cost += a.total_cost;
        row.initial_tokens += a.initial_tokens;
        row.persistence_tokens += a.persistence_tokens;
        row.riding_turns += a.riding_turns;
        *row.tools.entry(tool.to_string()).or_insert(0.0) += a.total_cost;
    }

    let mut out: Vec<McpServerAggregation> = by_server
        .into_values()
        .map(|row| {
            let mut tools: Vec<(String, f64)> = row.tools.into_iter().collect();
            tools.sort_by(|(an, ac), (bn, bc)| bc.total_cmp(ac).then_with(|| an.cmp(bn)));
            let top_tools: Vec<String> = tools.into_iter().take(3).map(|(n, _)| n).collect();
            McpServerAggregation {
                server: row.server,
                call_count: row.call_count,
                initial_tokens: row.initial_tokens,
                persistence_tokens: row.persistence_tokens,
                riding_turns: row.riding_turns,
                total_cost: row.total_cost,
                top_tools,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.total_cost
            .total_cmp(&a.total_cost)
            .then_with(|| a.server.cmp(&b.server))
    });
    out
}

#[cfg(test)]
#[path = "hotspots_tests.rs"]
mod tests;
