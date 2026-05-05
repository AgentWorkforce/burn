//! Per-tool cost attribution and aggregation — Rust port of
//! `packages/analyze/src/hotspots.ts`.
//!
//! Composes the per-tool attribution loop with file / bash / bash-verb /
//! subagent rollups. The math runs in `f64` to mirror TS `number`, and the
//! reduce order is preserved so per-tool USD totals match the TS
//! implementation within the 1e-9 USD precision contract that gates analyze
//! ports.

use std::collections::HashMap;

use indexmap::IndexMap;
use phf::phf_set;
use relayburn_reader::{
    BashParse, ContentKind, ContentRecord, TurnRecord, UserTurnBlockKind, UserTurnRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cost::{cost_for_turn, lookup_model_rate};
use crate::pricing::PricingTable;

const PER_MILLION: f64 = 1_000_000.0;
const CHARS_PER_TOKEN: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttributionMethod {
    Sized,
    EvenSplit,
}

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
    pub initial_cost: f64,
    pub initial_tokens: f64,
    pub persistence_cost: f64,
    pub persistence_tokens: f64,
    pub riding_turns: u64,
    pub total_cost: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTotals {
    pub session_id: String,
    pub grand_cost: f64,
    pub attributed_cost: f64,
    pub unattributed_cost: f64,
    pub attribution_method: AttributionMethod,
}

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
}

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
}

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
}

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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentAggregation {
    pub subagent_type: String,
    pub call_count: u64,
    pub total_cost: f64,
    pub initial_tokens: f64,
    pub persistence_tokens: f64,
}

static FILE_TOOLS: phf::Set<&'static str> = phf_set! {
    "Read", "Edit", "Write", "NotebookEdit",
};

pub fn attribute_hotspots(turns: &[TurnRecord], opts: &HotspotsOptions<'_>) -> HotspotsResult {
    // First-seen session ordering matches the TS `Map` iteration semantics.
    let mut by_session: IndexMap<String, Vec<TurnRecord>> = IndexMap::new();
    for t in turns {
        by_session
            .entry(t.session_id.clone())
            .or_default()
            .push(t.clone());
    }

    let mut attributions: Vec<ToolAttribution> = Vec::new();
    let mut session_totals: Vec<SessionTotals> = Vec::new();
    let mut grand_total = 0.0_f64;
    let mut attributed_total = 0.0_f64;

    for (session_id, mut session_turns) in by_session.into_iter() {
        // Stable sort matches the TS `Array.prototype.sort` contract.
        session_turns.sort_by_key(|t| t.turn_index);

        let session_content = opts.content_by_session.and_then(|m| m.get(&session_id));
        let tool_results_by_turn =
            session_content.map(|content| index_tool_results(content, &session_turns));

        let user_turns: &[UserTurnRecord] = opts
            .user_turns_by_session
            .and_then(|m| m.get(&session_id))
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let session_result = attribute_session(
            &session_turns,
            opts.pricing,
            tool_results_by_turn.as_ref(),
            user_turns,
        );

        let mut session_grand = 0.0_f64;
        for t in &session_turns {
            // Use the canonical `cost_for_turn` so hotspots totals stay in
            // lock-step with `cost.rs` for sources whose reasoning billing
            // semantics differ (Codex `included_in_output`, models with a
            // separate reasoning tariff, etc.).
            if let Some(b) = cost_for_turn(t, opts.pricing) {
                session_grand += b.total;
            }
        }
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
}

fn attribute_session(
    turns: &[TurnRecord],
    pricing: &PricingTable,
    tool_results_by_turn: Option<&HashMap<u64, PerTurnContent>>,
    user_turns: &[UserTurnRecord],
) -> SessionAttribution {
    if turns.is_empty() {
        return SessionAttribution {
            attributions: Vec::new(),
            method: AttributionMethod::EvenSplit,
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
                size_by_tool_use_id.insert(tu.clone(), estimate_tokens(text));
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

    for turn in turns {
        let turn_rate = lookup_model_rate(&turn.model, pricing);

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
                    let per_token_price = input_share * rate.input + create_share * rate.cache_write;
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
    }
}

fn index_tool_results(
    content: &[ContentRecord],
    turns: &[TurnRecord],
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

fn stringify_tool_result(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(arr) => {
            let mut parts: Vec<String> = Vec::new();
            for block in arr {
                match block {
                    Value::Object(obj) => {
                        let kind = obj.get("type").and_then(Value::as_str);
                        let text = obj.get("text").and_then(Value::as_str);
                        if kind == Some("text") {
                            if let Some(t) = text {
                                parts.push(t.to_string());
                                continue;
                            }
                        }
                        parts.push(serde_json::to_string(block).unwrap_or_default());
                    }
                    // Arrays match `typeof === 'object'` in JS, so JSON.stringify them.
                    Value::Array(_) => {
                        parts.push(serde_json::to_string(block).unwrap_or_default());
                    }
                    Value::String(s) => parts.push(s.clone()),
                    // Numbers, booleans, null: TS skips (`block && typeof === 'object'` is false
                    // and `typeof === 'string'` is false).
                    _ => {}
                }
            }
            parts.join("\n")
        }
        // `JSON.stringify(undefined)` is `undefined` in JS; serde_json can
        // still serialize numbers / booleans / objects deterministically.
        _ => serde_json::to_string(content).unwrap_or_default(),
    }
}

/// Standard chars-per-token heuristic. Anthropic's BPE averages ~3.5–4
/// chars/token for English; we use 4 to slightly under-estimate (better to
/// under-attribute cost than over-attribute). UTF-16 code units match TS's
/// `string.length`, keeping ASCII fixtures bit-for-bit equivalent and
/// preserving the same surrogate-pair behavior on emoji.
fn estimate_tokens(text: &str) -> u64 {
    let utf16_len = text.encode_utf16().count() as u64;
    utf16_len.div_ceil(CHARS_PER_TOKEN)
}

pub fn aggregate_by_file(attributions: &[ToolAttribution]) -> Vec<FileAggregation> {
    let mut by_path: IndexMap<String, FileAggregation> = IndexMap::new();
    for a in attributions {
        if !FILE_TOOLS.contains(a.tool_name.as_str()) {
            continue;
        }
        let target = match a.target.as_ref() {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };
        let row = by_path
            .entry(target.clone())
            .or_insert_with(|| FileAggregation {
                path: target.clone(),
                tool_call_count: 0,
                initial_tokens: 0.0,
                persistence_tokens: 0.0,
                riding_turns: 0,
                total_cost: 0.0,
                first_emit_ts: a.emit_ts.clone(),
                first_emit_turn_index: a.emit_turn_index,
            });
        row.tool_call_count += 1;
        row.initial_tokens += a.initial_tokens;
        row.persistence_tokens += a.persistence_tokens;
        row.riding_turns += a.riding_turns;
        row.total_cost += a.total_cost;
        if a.emit_ts < row.first_emit_ts {
            row.first_emit_ts = a.emit_ts.clone();
            row.first_emit_turn_index = a.emit_turn_index;
        }
    }
    let mut out: Vec<FileAggregation> = by_path.into_values().collect();
    out.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

pub fn aggregate_by_bash(attributions: &[ToolAttribution]) -> Vec<BashAggregation> {
    let mut by_hash: IndexMap<String, BashAggregation> = IndexMap::new();
    for a in attributions {
        if a.tool_name != "Bash" {
            continue;
        }
        let row = by_hash
            .entry(a.args_hash.clone())
            .or_insert_with(|| BashAggregation {
                args_hash: a.args_hash.clone(),
                command: a.target.clone(),
                call_count: 0,
                total_cost: 0.0,
                initial_tokens: 0.0,
                persistence_tokens: 0.0,
            });
        row.call_count += 1;
        row.total_cost += a.total_cost;
        row.initial_tokens += a.initial_tokens;
        row.persistence_tokens += a.persistence_tokens;
    }
    let mut out: Vec<BashAggregation> = by_hash.into_values().collect();
    out.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

struct BashVerbAccumulator {
    verb: String,
    call_count: u64,
    total_cost: f64,
    initial_tokens: f64,
    persistence_tokens: f64,
    riding_turns: u64,
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
                hashes: IndexMap::new(),
                examples: IndexMap::new(),
            });
        row.call_count += 1;
        row.total_cost += a.total_cost;
        row.initial_tokens += a.initial_tokens;
        row.persistence_tokens += a.persistence_tokens;
        row.riding_turns += a.riding_turns;
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
                    .partial_cmp(&a.total_cost)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.command.cmp(&b.command))
            });
            let top_examples: Vec<String> = examples
                .into_iter()
                .take(3)
                .map(|e| e.command)
                .collect();
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
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.verb.cmp(&b.verb))
    });
    out
}

pub fn aggregate_by_subagent(attributions: &[ToolAttribution]) -> Vec<SubagentAggregation> {
    let mut by_type: IndexMap<String, SubagentAggregation> = IndexMap::new();
    for a in attributions {
        if a.tool_name != "Agent" && a.tool_name != "Task" {
            continue;
        }
        let key = a
            .subagent_type
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string());
        let row = by_type
            .entry(key.clone())
            .or_insert_with(|| SubagentAggregation {
                subagent_type: key,
                call_count: 0,
                total_cost: 0.0,
                initial_tokens: 0.0,
                persistence_tokens: 0.0,
            });
        row.call_count += 1;
        row.total_cost += a.total_cost;
        row.initial_tokens += a.initial_tokens;
        row.persistence_tokens += a.persistence_tokens;
    }
    let mut out: Vec<SubagentAggregation> = by_type.into_values().collect();
    out.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::load_builtin_pricing;
    use relayburn_reader::{
        parse_bash_command, ContentRole, ContentToolResult, SourceKind, ToolCall, Usage,
        UserTurnBlock,
    };
    use serde_json::json;

    fn empty_usage() -> Usage {
        Usage {
            input: 0,
            output: 0,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    fn turn(
        session_id: &str,
        message_id: &str,
        turn_index: u64,
        ts: &str,
        model: &str,
        usage: Usage,
        tool_calls: Vec<ToolCall>,
        source: SourceKind,
    ) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session_id.into(),
            session_path: None,
            message_id: message_id.into(),
            turn_index,
            ts: ts.into(),
            model: model.into(),
            project: None,
            project_key: None,
            usage,
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

    fn tc(id: &str, name: &str, target: Option<&str>) -> ToolCall {
        let target_part = target.unwrap_or(id);
        ToolCall {
            id: id.into(),
            name: name.into(),
            target: target.map(String::from),
            args_hash: format!("{name}:{target_part}"),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn tc_with_hash(id: &str, name: &str, target: &str, args_hash: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            target: Some(target.into()),
            args_hash: args_hash.into(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn tool_result_content(
        session_id: &str,
        tool_use_id: &str,
        text: &str,
        ts: &str,
    ) -> ContentRecord {
        ContentRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.into(),
            message_id: format!("m-{tool_use_id}"),
            ts: ts.into(),
            role: ContentRole::ToolResult,
            kind: ContentKind::ToolResult,
            text: None,
            tool_use: None,
            tool_result: Some(ContentToolResult {
                tool_use_id: tool_use_id.into(),
                content: json!(text),
                is_error: None,
            }),
        }
    }

    fn user_turn(
        session_id: &str,
        user_uuid: &str,
        blocks: Vec<UserTurnBlock>,
    ) -> UserTurnRecord {
        UserTurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.into(),
            user_uuid: user_uuid.into(),
            ts: "2026-04-20T00:00:00.500Z".into(),
            preceding_message_id: Some("msg-0".into()),
            following_message_id: Some("msg-1".into()),
            blocks,
        }
    }

    fn tool_result_block(tool_use_id: &str, byte_len: u64, approx_tokens: u64) -> UserTurnBlock {
        UserTurnBlock {
            kind: UserTurnBlockKind::ToolResult,
            tool_use_id: Some(tool_use_id.into()),
            byte_len,
            approx_tokens,
            is_error: None,
        }
    }

    fn bash_attribution(
        command: &str,
        args_hash: &str,
        total_cost: f64,
        initial_tokens: f64,
        persistence_tokens: f64,
        riding_turns: u64,
    ) -> ToolAttribution {
        ToolAttribution {
            tool_use_id: format!("tu-{args_hash}"),
            tool_name: "Bash".into(),
            target: Some(command.into()),
            args_hash: args_hash.into(),
            session_id: "s-bash-verb".into(),
            emit_turn_index: 0,
            emit_ts: "2026-04-20T00:00:00.000Z".into(),
            model: "claude-sonnet-4-6".into(),
            project: None,
            project_key: None,
            subagent_type: None,
            result_tokens: 0,
            result_bytes_estimated: true,
            initial_cost: total_cost,
            initial_tokens,
            persistence_cost: 0.0,
            persistence_tokens,
            riding_turns,
            total_cost,
        }
    }

    #[test]
    fn attributes_persistence_of_8k_read_across_20_ride_along_turns_within_10_pct() {
        let pricing = load_builtin_pricing();
        let rate = pricing.get("claude-sonnet-4-6").expect("sonnet present").clone();
        const READ_TOKENS: u64 = 8000;
        let read_text: String = "x".repeat((READ_TOKENS as usize) * 4);

        let session_id = "s-hotspots-1";
        let mut turns: Vec<TurnRecord> = Vec::new();

        // Turn 0: assistant emits the Read tool_use.
        turns.push(turn(
            session_id,
            "msg-0",
            0,
            "2026-04-20T00:00:00.000Z",
            "claude-sonnet-4-6",
            Usage {
                input: 200,
                output: 50,
                reasoning: 0,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            vec![tc("tu_read_1", "Read", Some("/src/big.ts"))],
            SourceKind::ClaudeCode,
        ));

        // Turn 1 pays initial: 8000 tokens enter as fresh input.
        turns.push(turn(
            session_id,
            "msg-1",
            1,
            "2026-04-20T00:00:01.000Z",
            "claude-sonnet-4-6",
            Usage {
                input: READ_TOKENS,
                output: 30,
                reasoning: 0,
                cache_read: 250,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            vec![],
            SourceKind::ClaudeCode,
        ));

        // Turns 2..=21: 20 ride-along turns each with cacheRead >= READ_TOKENS.
        for i in 2..=21u64 {
            turns.push(turn(
                session_id,
                &format!("msg-{i}"),
                i,
                &format!("2026-04-20T00:00:{:02}.000Z", i),
                "claude-sonnet-4-6",
                Usage {
                    input: 50,
                    output: 30,
                    reasoning: 0,
                    cache_read: READ_TOKENS + 2000,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ));
        }

        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![tool_result_content(
                session_id,
                "tu_read_1",
                &read_text,
                "2026-04-20T00:00:00.500Z",
            )],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
            },
        );
        assert_eq!(result.attributions.len(), 1);
        let a = &result.attributions[0];
        assert_eq!(a.tool_use_id, "tu_read_1");

        let expected_initial = (READ_TOKENS as f64 / 1_000_000.0) * rate.input;
        let expected_persistence = 20.0 * (READ_TOKENS as f64 / 1_000_000.0) * rate.cache_read;
        let expected_total = expected_initial + expected_persistence;
        assert!(
            (a.total_cost - expected_total).abs() <= expected_total * 0.10,
            "total={} expected={} diff>10%",
            a.total_cost,
            expected_total
        );
        assert_eq!(a.riding_turns, 20);
    }

    #[test]
    fn aggregates_by_file_and_ranks_most_expensive_read_first() {
        let pricing = load_builtin_pricing();
        let session_id = "s-files";
        const READ_TOKENS: u64 = 5000;
        const SMALL_TOKENS: u64 = 200;
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc("tu_a", "Read", Some("/big.ts")),
                    tc("tu_b", "Read", Some("/small.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: READ_TOKENS + SMALL_TOKENS,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-2",
                2,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 100,
                    output: 5,
                    reasoning: 0,
                    cache_read: READ_TOKENS + SMALL_TOKENS + 500,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-3",
                3,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 100,
                    output: 5,
                    reasoning: 0,
                    cache_read: READ_TOKENS + SMALL_TOKENS + 500,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![
                tool_result_content(
                    session_id,
                    "tu_a",
                    &"x".repeat((READ_TOKENS as usize) * 4),
                    "2026-04-20T00:00:00.100Z",
                ),
                tool_result_content(
                    session_id,
                    "tu_b",
                    &"y".repeat((SMALL_TOKENS as usize) * 4),
                    "2026-04-20T00:00:00.101Z",
                ),
            ],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
            },
        );
        let files = aggregate_by_file(&result.attributions);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "/big.ts");
        assert_eq!(files[1].path, "/small.ts");
        assert!(files[0].total_cost > files[1].total_cost);
    }

    #[test]
    fn aggregates_by_bash_args_hash_so_repeated_commands_collapse() {
        let pricing = load_builtin_pricing();
        let session_id = "s-bash";
        let mut turns: Vec<TurnRecord> = Vec::new();
        let mut ts = 0u64;
        for i in 0..3 {
            turns.push(turn(
                session_id,
                &format!("msg-emit-{i}"),
                ts,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![tc_with_hash(
                    &format!("tu_b_{i}"),
                    "Bash",
                    "ls -la",
                    "Bash:ls",
                )],
                SourceKind::ClaudeCode,
            ));
            ts += 1;
            turns.push(turn(
                session_id,
                &format!("msg-pay-{i}"),
                ts,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 1000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ));
            ts += 1;
        }
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![
                tool_result_content(
                    session_id,
                    "tu_b_0",
                    &"x".repeat(4000),
                    "2026-04-20T00:00:00.100Z",
                ),
                tool_result_content(
                    session_id,
                    "tu_b_1",
                    &"x".repeat(4000),
                    "2026-04-20T00:00:00.200Z",
                ),
                tool_result_content(
                    session_id,
                    "tu_b_2",
                    &"x".repeat(4000),
                    "2026-04-20T00:00:00.300Z",
                ),
            ],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
            },
        );
        let bash = aggregate_by_bash(&result.attributions);
        assert_eq!(bash.len(), 1);
        assert_eq!(bash[0].call_count, 3);
    }

    #[test]
    fn aggregates_bash_cost_by_normalized_verb_with_distinct_command_and_examples() {
        let attrs = vec![
            bash_attribution("git status", "git:status", 2.0, 20.0, 5.0, 0),
            bash_attribution("git status", "git:status", 2.0, 20.0, 5.0, 0),
            bash_attribution("git status", "git:status", 2.0, 20.0, 5.0, 0),
            bash_attribution("git diff src/a.ts", "git:diff:a", 5.0, 100.0, 10.0, 1),
            bash_attribution("git diff src/a.ts", "git:diff:a", 5.0, 100.0, 10.0, 1),
            bash_attribution("git diff src/b.ts", "git:diff:b", 7.0, 100.0, 20.0, 2),
            bash_attribution("git diff src/b.ts", "git:diff:b", 7.0, 100.0, 20.0, 2),
            bash_attribution("git diff src/b.ts", "git:diff:b", 7.0, 100.0, 20.0, 2),
            bash_attribution("pnpm run test", "pnpm:test", 4.0, 40.0, 8.0, 1),
        ];

        let verbs = aggregate_by_bash_verb(&attrs, parse_bash_command);
        assert_eq!(verbs[0].verb, "git diff");
        assert_eq!(verbs[0].call_count, 5);
        assert_eq!(verbs[0].distinct_commands, 2);
        assert!((verbs[0].total_cost - 31.0).abs() < 1e-9);
        assert!((verbs[0].initial_tokens - 500.0).abs() < 1e-9);
        assert!((verbs[0].persistence_tokens - 80.0).abs() < 1e-9);
        assert!((verbs[0].avg_persistence_turns - 1.6).abs() < 1e-9);
        assert_eq!(
            verbs[0].top_examples,
            vec!["git diff src/b.ts", "git diff src/a.ts"]
        );

        assert_eq!(verbs[1].verb, "git status");
        assert_eq!(verbs[1].call_count, 3);
        assert_eq!(verbs[1].distinct_commands, 1);
        assert_eq!(verbs[2].verb, "pnpm test");
    }

    #[test]
    fn aggregates_subagent_calls_by_subagent_type() {
        let pricing = load_builtin_pricing();
        let session_id = "s-agent";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![tc_with_hash("tu_a1", "Agent", "general-purpose", "Agent:gp")],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 2000,
                    output: 10,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![tool_result_content(
                session_id,
                "tu_a1",
                &"z".repeat(8000),
                "2026-04-20T00:00:00.100Z",
            )],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
            },
        );
        let subagents = aggregate_by_subagent(&result.attributions);
        assert_eq!(subagents.len(), 1);
        assert_eq!(subagents[0].subagent_type, "general-purpose");
        assert_eq!(subagents[0].call_count, 1);
        assert!(subagents[0].total_cost > 0.0);
    }

    #[test]
    fn falls_back_to_even_split_when_no_content_is_provided() {
        let pricing = load_builtin_pricing();
        let session_id = "s-fallback";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc("tu_x", "Read", Some("/a.ts")),
                    tc("tu_y", "Read", Some("/b.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 4000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: None,
                user_turns_by_session: None,
            },
        );
        assert_eq!(result.attributions.len(), 2);
        let rate = pricing.get("claude-sonnet-4-6").unwrap();
        let expected = ((4000.0 / 1_000_000.0) * rate.input) / 2.0;
        for a in &result.attributions {
            assert!((a.initial_cost - expected).abs() < 1e-9);
            assert_eq!(a.persistence_cost, 0.0);
        }
        assert_eq!(
            result.session_totals[0].attribution_method,
            AttributionMethod::EvenSplit
        );
    }

    #[test]
    fn uses_user_turn_block_sizes_when_content_sidecar_is_unavailable() {
        let pricing = load_builtin_pricing();
        let session_id = "s-user-turn-fallback";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc("tu_big", "Read", Some("/big.ts")),
                    tc("tu_small", "Read", Some("/small.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 4000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-2",
                2,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 100,
                    output: 5,
                    reasoning: 0,
                    cache_read: 4500,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];

        let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
        user_turns_by_session.insert(
            session_id.into(),
            vec![user_turn(
                session_id,
                "u-1",
                vec![
                    tool_result_block("tu_big", 12_000, 3000),
                    tool_result_block("tu_small", 4000, 1000),
                ],
            )],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: None,
                user_turns_by_session: Some(&user_turns_by_session),
            },
        );
        let by_id: HashMap<&str, &ToolAttribution> = result
            .attributions
            .iter()
            .map(|a| (a.tool_use_id.as_str(), a))
            .collect();
        assert_eq!(
            result.session_totals[0].attribution_method,
            AttributionMethod::Sized
        );
        assert!((by_id["tu_big"].initial_tokens - 3000.0).abs() < 1e-9);
        assert!((by_id["tu_small"].initial_tokens - 1000.0).abs() < 1e-9);
        assert!((by_id["tu_big"].persistence_tokens - 3000.0).abs() < 1e-9);
        assert!((by_id["tu_small"].persistence_tokens - 1000.0).abs() < 1e-9);
        assert!(by_id["tu_big"].total_cost > by_id["tu_small"].total_cost);
    }

    #[test]
    fn prefers_user_turn_block_sizes_over_content_sidecar_estimates() {
        let pricing = load_builtin_pricing();
        let session_id = "s-sidecar-primary";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![tc("tu_read", "Read", Some("/file.ts"))],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 10_000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![tool_result_content(
                session_id,
                "tu_read",
                &"x".repeat(1000 * 4),
                "2026-04-20T00:00:00.100Z",
            )],
        );
        let mut user_turns_by_session: HashMap<String, Vec<UserTurnRecord>> = HashMap::new();
        user_turns_by_session.insert(
            session_id.into(),
            vec![user_turn(
                session_id,
                "u-1",
                vec![tool_result_block("tu_read", 36_000, 9000)],
            )],
        );

        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: Some(&user_turns_by_session),
            },
        );
        assert_eq!(
            result.session_totals[0].attribution_method,
            AttributionMethod::Sized
        );
        assert!((result.attributions[0].initial_tokens - 9000.0).abs() < 1e-9);
    }

    #[test]
    fn caps_sibling_initial_cost_at_next_turns_actual_new_content() {
        let pricing = load_builtin_pricing();
        let session_id = "s-cap";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc("tu_big", "Read", Some("/big.ts")),
                    tc("tu_med", "Read", Some("/med.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 5000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![
                tool_result_content(
                    session_id,
                    "tu_big",
                    &"x".repeat(6000 * 4),
                    "2026-04-20T00:00:00.100Z",
                ),
                tool_result_content(
                    session_id,
                    "tu_med",
                    &"y".repeat(4000 * 4),
                    "2026-04-20T00:00:00.101Z",
                ),
            ],
        );
        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
            },
        );
        let summed: f64 = result
            .attributions
            .iter()
            .map(|a| a.initial_tokens)
            .sum();
        assert!(
            summed <= 5000.0 + 1e-6,
            "summed={summed} > newContent=5000"
        );
        let big = result
            .attributions
            .iter()
            .find(|a| a.tool_use_id == "tu_big")
            .unwrap();
        let med = result
            .attributions
            .iter()
            .find(|a| a.tool_use_id == "tu_med")
            .unwrap();
        assert!((big.initial_tokens - 3000.0).abs() < 1e-6);
        assert!((med.initial_tokens - 2000.0).abs() < 1e-6);
    }

    #[test]
    fn caps_sibling_persistence_at_turns_actual_cache_read() {
        let pricing = load_builtin_pricing();
        let session_id = "s-persist-cap";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![
                    tc("tu_a", "Read", Some("/a.ts")),
                    tc("tu_b", "Read", Some("/b.ts")),
                ],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 8000,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-2",
                2,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 50,
                    output: 5,
                    reasoning: 0,
                    cache_read: 5000,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![
                tool_result_content(
                    session_id,
                    "tu_a",
                    &"x".repeat(4000 * 4),
                    "2026-04-20T00:00:00.100Z",
                ),
                tool_result_content(
                    session_id,
                    "tu_b",
                    &"y".repeat(4000 * 4),
                    "2026-04-20T00:00:00.101Z",
                ),
            ],
        );
        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
            },
        );
        let summed_persist: f64 = result
            .attributions
            .iter()
            .map(|a| a.persistence_tokens)
            .sum();
        assert!(
            summed_persist <= 5000.0 + 1e-6,
            "summedPersist={summed_persist} > cacheRead=5000"
        );
        for a in &result.attributions {
            assert!((a.persistence_tokens - 2500.0).abs() < 1e-6);
        }
    }

    #[test]
    fn uses_paying_turns_model_rate_not_emit_turns() {
        let pricing = load_builtin_pricing();
        let sonnet = pricing.get("claude-sonnet-4-6").unwrap().clone();
        let haiku = pricing.get("claude-haiku-4-5").unwrap().clone();
        assert_ne!(haiku.input, sonnet.input, "test prerequisite: rates differ");

        let session_id = "s-cross-model";
        const TOK: u64 = 4000;
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                empty_usage(),
                vec![tc("tu_x", "Read", Some("/x.ts"))],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-haiku-4-5",
                Usage {
                    input: TOK,
                    output: 5,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-2",
                2,
                "2026-04-20T00:00:00.000Z",
                "claude-haiku-4-5",
                Usage {
                    input: 50,
                    output: 5,
                    reasoning: 0,
                    cache_read: TOK + 100,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![tool_result_content(
                session_id,
                "tu_x",
                &"z".repeat((TOK as usize) * 4),
                "2026-04-20T00:00:00.100Z",
            )],
        );
        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
            },
        );
        let a = &result.attributions[0];
        let expected_initial = (TOK as f64 / 1_000_000.0) * haiku.input;
        let expected_persistence = (TOK as f64 / 1_000_000.0) * haiku.cache_read;
        assert!(
            (a.initial_cost - expected_initial).abs() < 1e-9,
            "initial_cost={} expected={}",
            a.initial_cost,
            expected_initial
        );
        assert!(
            (a.persistence_cost - expected_persistence).abs() < 1e-9,
            "persistence_cost={} expected={}",
            a.persistence_cost,
            expected_persistence
        );
    }

    #[test]
    fn session_grand_honors_source_aware_reasoning_for_codex() {
        // Regression: hotspots must use `cost_for_turn` so its `session_grand`
        // inherits Codex's `included_in_output` reasoning semantics. Otherwise
        // it overstates by `reasoning × output_rate` and drifts away from the
        // canonical `cost.rs` totals.
        let pricing = load_builtin_pricing();
        let codex_model = if pricing.contains_key("gpt-5-codex") {
            "gpt-5-codex"
        } else {
            "claude-sonnet-4-6"
        };
        let session_id = "s-codex-reasoning";
        let turns = vec![turn(
            session_id,
            "msg-0",
            0,
            "2026-04-20T00:00:00.000Z",
            codex_model,
            Usage {
                input: 1000,
                // Codex `output_tokens` already includes reasoning.
                output: 500,
                reasoning: 200,
                cache_read: 0,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            vec![],
            SourceKind::Codex,
        )];
        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: None,
                user_turns_by_session: None,
            },
        );

        let rate = pricing.get(codex_model).unwrap();
        let expected = (1000.0 / 1_000_000.0) * rate.input + (500.0 / 1_000_000.0) * rate.output;
        assert!(
            (result.grand_total - expected).abs() < 1e-9,
            "Codex sessionGrand should not bill reasoning at output rate: got={} expected={}",
            result.grand_total,
            expected
        );
    }

    #[test]
    fn grand_total_plus_unattributed_equals_session_grand_within_rounding() {
        let pricing = load_builtin_pricing();
        let session_id = "s-totals";
        let turns = vec![
            turn(
                session_id,
                "msg-0",
                0,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 100,
                    output: 50,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![tc("tu_z", "Read", Some("/z.ts"))],
                SourceKind::ClaudeCode,
            ),
            turn(
                session_id,
                "msg-1",
                1,
                "2026-04-20T00:00:00.000Z",
                "claude-sonnet-4-6",
                Usage {
                    input: 2000,
                    output: 30,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
                vec![],
                SourceKind::ClaudeCode,
            ),
        ];
        let mut content_by_session: HashMap<String, Vec<ContentRecord>> = HashMap::new();
        content_by_session.insert(
            session_id.into(),
            vec![tool_result_content(
                session_id,
                "tu_z",
                &"q".repeat(2000 * 4),
                "2026-04-20T00:00:00.500Z",
            )],
        );
        let result = attribute_hotspots(
            &turns,
            &HotspotsOptions {
                pricing: &pricing,
                content_by_session: Some(&content_by_session),
                user_turns_by_session: None,
            },
        );
        assert!(
            (result.attributed_total + result.unattributed_total - result.grand_total).abs() < 1e-9
        );
    }

    #[test]
    fn attribution_method_serializes_to_kebab_case() {
        // The CLI/MCP presenters round-trip these enums through JSON, so the
        // wire format must match the TS string union ('sized' | 'even-split').
        assert_eq!(
            serde_json::to_string(&AttributionMethod::Sized).unwrap(),
            "\"sized\""
        );
        assert_eq!(
            serde_json::to_string(&AttributionMethod::EvenSplit).unwrap(),
            "\"even-split\""
        );
    }
}
