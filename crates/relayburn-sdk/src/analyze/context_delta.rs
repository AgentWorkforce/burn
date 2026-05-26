//! Per-inference context-window delta attribution.
//!
//! Answers "what blew up my context between inference N and inference
//! N+1?" by walking [`TurnSpanTree`]s in order, pairing same-rail
//! [`SpanKind::Inference`] nodes, and attributing the delta in
//! `context_tokens` to the [`InterveningStep`]s that landed in the
//! prompt between them. See AgentWorkforce/burn#432.
//!
//! # Algorithm
//!
//! 1. Flatten every span across every turn in DFS order into a single
//!    timeline. Each leaf the consumer cares about (`Inference`,
//!    `ToolResult`, `UserPrompt`, system-reminder `UserPrompt`) gets a
//!    position equal to its DFS index across the session.
//! 2. Bucket inferences by **owner rail**: `Main` for spans whose path
//!    from the root does not pass through a `Subagent` node;
//!    `Subagent(agent_id)` for spans that do. Each leaf is attributed to
//!    exactly one rail — no cross-contamination.
//! 3. Within each rail, walk pairwise `(prev, curr)` inferences and
//!    collect the intervening leaves (events whose position falls
//!    strictly between `prev` and `curr`).
//! 4. `context_tokens(inf) = tokens.input + tokens.cache_read +
//!    tokens.cache_write` summed off the `Inference` span's attributes.
//! 5. `delta = curr.context_tokens - prev.context_tokens`.
//! 6. **Compaction handling**: if `delta < 0` AND a [`CompactionEvent`]
//!    sits between `prev` and `curr` (by timestamp), surface the row as
//!    [`InterveningStep::Compaction`] with `tokens_freed =
//!    prev - curr`. The `delta_tokens` on the returned [`ContextDelta`]
//!    stays `0` in that case so a negative number never lands in the
//!    output.
//! 7. **Cost**: charge `max(delta_tokens, 0) * curr_inference_cache_read_rate`.
//!    Cache-read is the rate the *future* will pay for the persisted
//!    prefix, which is the right charge for a "this much got added to
//!    your context window" question (vs. cache-write, which the next
//!    inference pays once when first persisting). The decision is
//!    documented in the issue's open-question #3.
//!
//! # Subagent isolation
//!
//! Main-rail inferences never see subagent tool_results, and vice
//! versa. A subagent's tool_use under its parent `Task` is attributed
//! to the subagent rail; its inferences never enter the main-rail
//! pairwise walk.
//!
//! # Token estimates for tool_results
//!
//! We use `output_bytes / 4` as the approximate token count. The
//! `output_bytes` field comes from the ingest-time byte measurement
//! recorded on [`crate::reader::ToolResultEventRecord::output_bytes`]
//! (issue #444). The 4-bytes-per-token ratio is a first-cut
//! approximation; downstream consumers should treat the number as
//! advisory.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::analyze::pricing::PricingTable;
use crate::analyze::span_tree::{AttrValue, SpanKind, SpanNode, TurnSpanTree};
use crate::reader::CompactionEvent;
use crate::util::time::parse_iso_ms;

/// Approximate bytes-per-token ratio used when no real tokenizer pass is
/// available. Mirrors the rule of thumb used elsewhere in burn for
/// content-size approximation. The output is marked approximate in
/// downstream JSON so consumers know not to bill on it.
const BYTES_PER_TOKEN: u64 = 4;

/// Which "rail" an inference belongs to. The main conversation rail is
/// independent of every subagent rail; deltas are computed per-rail and
/// never cross.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum OwnerRail {
    /// The top-level conversation between the user and the model.
    Main,
    /// A subagent dispatched by a `Task` tool_use. The `agentId` field
    /// carries the `agent_id` attribute from the [`SpanKind::Subagent`]
    /// span (or its parent in a nested case).
    Subagent {
        #[serde(rename = "agentId")]
        agent_id: String,
    },
}

/// Filter for [`ContextDeltaOpts::owner`]. Mirrors the CLI's
/// `--owner main|subagent|all` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OwnerFilter {
    /// No filter: emit deltas for every rail.
    #[default]
    All,
    /// Only main-rail deltas.
    Main,
    /// Only subagent-rail deltas.
    Subagent,
}

/// Source for a synthetic `<system-reminder>` step. First-cut implementation
/// classifies every reminder as [`ReminderSource::Other`]; downstream
/// issue #425 will split into `Relaycast` / `Harness` proper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReminderSource {
    /// Reminder originated from relaycast injection.
    Relaycast,
    /// Reminder originated from the harness (Claude Code, opencode, …).
    Harness,
    /// Unclassified — first-cut default. Refined by #425.
    Other,
}

/// One leaf that landed in the prompt between two consecutive inferences
/// on the same rail. The `approx_tokens` fields are best-effort estimates
/// derived from `output_bytes / 4` (tool_result) or text-byte / 4
/// (prompts and reminders).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum InterveningStep {
    /// A tool_result block paired to a tool_use the model issued before
    /// `prev` inference. Carries the tool name and an approximate
    /// token / byte count for the result payload.
    ToolResult {
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "approxTokens")]
        approx_tokens: u64,
        #[serde(rename = "approxBytes")]
        approx_bytes: u64,
        truncated: bool,
    },
    /// A user prompt that landed between the two inferences. Rare on
    /// single-turn flows but happens on multi-turn auto-responses
    /// (e.g. plan-mode confirmations).
    UserPrompt {
        #[serde(rename = "approxTokens")]
        approx_tokens: u64,
        #[serde(rename = "hasSystemReminder")]
        has_system_reminder: bool,
    },
    /// A `<system-reminder>` content block.
    SystemReminder {
        source: ReminderSource,
        #[serde(rename = "approxTokens")]
        approx_tokens: u64,
    },
    /// A compaction event sat between the two inferences and the delta
    /// went negative as a result. The negative delta is replaced by a
    /// `Compaction` row in the intervening list and the delta on the
    /// containing [`ContextDelta`] is clamped to `0`.
    Compaction {
        #[serde(rename = "tokensFreed")]
        tokens_freed: u64,
    },
    /// Catch-all for spans that don't fall into the above categories
    /// (kept for forward-compatibility — current builders never emit
    /// this variant).
    Other,
}

impl InterveningStep {
    /// Approximate token count attributed to this step. `Compaction`
    /// counts as zero — it doesn't add tokens, it frees them.
    pub fn approx_tokens(&self) -> u64 {
        match self {
            Self::ToolResult { approx_tokens, .. } => *approx_tokens,
            Self::UserPrompt { approx_tokens, .. } => *approx_tokens,
            Self::SystemReminder { approx_tokens, .. } => *approx_tokens,
            Self::Compaction { .. } => 0,
            Self::Other => 0,
        }
    }

    /// Short label for the "driver" column in human renderers.
    pub fn driver_label(&self) -> String {
        match self {
            Self::ToolResult { tool_name, .. } => format!("{tool_name} result"),
            Self::UserPrompt { .. } => "user prompt".to_string(),
            Self::SystemReminder { .. } => "system-reminder".to_string(),
            Self::Compaction { tokens_freed } => format!("compaction -{tokens_freed} tok"),
            Self::Other => "other".to_string(),
        }
    }
}

/// One per-rail (`prev`, `curr`) pair. The list returned by
/// [`LedgerHandle::context_delta`](crate::LedgerHandle::context_delta)
/// is sorted by `delta_tokens` descending (then `inference_idx`
/// ascending) and truncated to [`ContextDeltaOpts::top`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextDelta {
    pub session_id: String,
    pub turn_id: String,
    /// Position of `curr` inference within its rail, 1-indexed. The
    /// first inference of a rail has no `prev` so it never appears here.
    pub inference_idx: u32,
    pub owner_rail: OwnerRail,
    pub prior_context_tokens: u64,
    pub current_context_tokens: u64,
    /// Always `>= 0` in the output. Negative raw deltas are surfaced as
    /// [`InterveningStep::Compaction`] rows instead. `i64` is preserved
    /// in the type so a future "raw delta" surface can use it without a
    /// schema change.
    pub delta_tokens: i64,
    pub intervening: Vec<InterveningStep>,
    #[serde(rename = "attributedCostUSD")]
    pub attributed_cost_usd: f64,
}

/// Options for the context-delta verb. Each field has a sensible
/// default; callers usually only need to set `session` and possibly
/// `top` or `min_delta`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextDeltaOpts {
    /// When set, narrow to a single session. When `None`, every session
    /// in the ledger window contributes.
    pub session: Option<String>,
    /// Time window (relative — `Duration::from_secs(24 * 3600)` by
    /// default). Sessions whose latest activity falls before
    /// `now - since` are skipped.
    pub since: Option<Duration>,
    /// Output cap. Defaults to 20.
    pub top: Option<u32>,
    /// Hide deltas below this threshold. Defaults to 1000 tokens — the
    /// "noise floor" the issue specifies. Compaction rows ignore this
    /// (a compaction with `tokens_freed < min_delta` would otherwise
    /// vanish, defeating the point).
    pub min_delta: Option<u64>,
    /// Rail filter.
    #[serde(default)]
    pub owner: OwnerFilter,
}

impl ContextDeltaOpts {
    pub fn effective_top(&self) -> u32 {
        self.top.unwrap_or(20)
    }

    pub fn effective_min_delta(&self) -> u64 {
        self.min_delta.unwrap_or(1000)
    }

    pub fn effective_since(&self) -> Duration {
        self.since.unwrap_or(Duration::from_secs(24 * 3600))
    }
}

// ---------------------------------------------------------------------------
// Pure algorithm: span trees + compactions + pricing -> Vec<ContextDelta>
// ---------------------------------------------------------------------------

/// Compute per-rail context deltas across one session, given the
/// session's [`TurnSpanTree`]s in turn order plus its
/// [`CompactionEvent`]s (in any order — they're sorted internally).
///
/// Pure derivation: no I/O, no DB writes, no caching. The
/// [`LedgerHandle`](crate::LedgerHandle) wrapper does the loading and
/// then calls into here.
///
/// `pricing` is consulted for the per-million `cache_read` rate of the
/// `curr` inference's model. Models the pricing table doesn't recognize
/// charge `0.0` (matching the rest of the analyze surface, which never
/// surfaces costs it can't price).
pub fn deltas_for_session(
    trees: &[TurnSpanTree],
    compactions: &[CompactionEvent],
    pricing: &PricingTable,
    opts: &ContextDeltaOpts,
) -> Vec<ContextDelta> {
    if trees.is_empty() {
        return Vec::new();
    }
    let timeline = build_timeline(trees);
    let mut compactions_sorted: Vec<&CompactionEvent> = compactions.iter().collect();
    // `sort_by_cached_key` so the relatively expensive `parse_iso_ms` runs once
    // per element rather than once per comparison.
    compactions_sorted.sort_by_cached_key(|c| parse_iso_ms(&c.ts).unwrap_or(0));

    let mut per_rail: HashMap<OwnerRail, Vec<usize>> = HashMap::new();
    for (idx, item) in timeline.iter().enumerate() {
        if matches!(item.kind, TimelineKind::Inference { .. }) {
            per_rail.entry(item.owner.clone()).or_default().push(idx);
        }
    }

    let min_delta = opts.effective_min_delta() as i64;
    let mut out: Vec<ContextDelta> = Vec::new();
    for (rail, inf_indices) in per_rail.iter() {
        if !rail_passes_filter(rail, opts.owner) {
            continue;
        }
        for (pair_idx, window) in inf_indices.windows(2).enumerate() {
            let prev_pos = window[0];
            let curr_pos = window[1];
            let TimelineKind::Inference {
                context_tokens: prev_ctx,
                ..
            } = timeline[prev_pos].kind
            else {
                continue;
            };
            let TimelineKind::Inference {
                context_tokens: curr_ctx,
                model: ref curr_model,
            } = timeline[curr_pos].kind
            else {
                continue;
            };

            let raw_delta = curr_ctx as i64 - prev_ctx as i64;

            // Collect intervening leaves between (prev_pos, curr_pos) on
            // the same rail. Walk the flat timeline; ignore items on
            // other rails so subagent leaves never enter a main-rail
            // delta (and vice versa).
            let mut intervening: Vec<InterveningStep> = Vec::new();
            for item in &timeline[prev_pos + 1..curr_pos] {
                if item.owner != *rail {
                    continue;
                }
                if let Some(step) = item.to_intervening_step() {
                    intervening.push(step);
                }
            }

            // Compaction handling: if there's a compaction event between
            // prev.end_ms and curr.start_ms AND the delta is negative,
            // surface it as a Compaction row and clamp delta to 0.
            let prev_end = timeline[prev_pos].end_ms;
            let curr_start = timeline[curr_pos].start_ms;
            let compaction_between = compactions_sorted.iter().any(|c| {
                let ms = parse_iso_ms(&c.ts).unwrap_or(0);
                ms >= prev_end && ms <= curr_start
            });
            let (delta_tokens, intervening) = if raw_delta < 0 && compaction_between {
                let freed = prev_ctx - curr_ctx;
                let mut steps = intervening;
                steps.push(InterveningStep::Compaction {
                    tokens_freed: freed,
                });
                (0i64, steps)
            } else {
                (raw_delta, intervening)
            };

            if delta_tokens < min_delta
                && !intervening
                    .iter()
                    .any(|s| matches!(s, InterveningStep::Compaction { .. }))
            {
                continue;
            }

            let session_id = timeline[curr_pos].session_id.clone();
            let turn_id = timeline[curr_pos].turn_id.clone();
            let cost = attributed_cost(delta_tokens, curr_model, pricing);

            out.push(ContextDelta {
                session_id,
                turn_id,
                // 1-indexed position within the rail. `windows(2)`
                // gives us pair index 0 = first pair = curr is the
                // second inference, so the curr inference index is
                // `pair_idx + 2` in 1-indexed terms.
                inference_idx: (pair_idx as u32) + 2,
                owner_rail: rail.clone(),
                prior_context_tokens: prev_ctx,
                current_context_tokens: curr_ctx,
                delta_tokens,
                intervening,
                attributed_cost_usd: cost,
            });
        }
    }

    // Sort by delta descending, with a full lex chain so the output is
    // deterministic across HashMap iteration order even when multiple
    // rails / sessions tie on (delta_tokens, turn_id, inference_idx).
    out.sort_by(|a, b| {
        b.delta_tokens
            .cmp(&a.delta_tokens)
            .then_with(|| a.turn_id.cmp(&b.turn_id))
            .then_with(|| a.inference_idx.cmp(&b.inference_idx))
            .then_with(|| {
                owner_rail_sort_key(&a.owner_rail).cmp(&owner_rail_sort_key(&b.owner_rail))
            })
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    let top = opts.effective_top() as usize;
    if out.len() > top {
        out.truncate(top);
    }
    out
}

/// Stable lex key for sorting `OwnerRail` so tie-breakers are deterministic
/// regardless of HashMap iteration order. `Main` sorts before any subagent;
/// subagents sort by `agent_id`.
fn owner_rail_sort_key(rail: &OwnerRail) -> (&str, &str) {
    match rail {
        OwnerRail::Main => ("main", ""),
        OwnerRail::Subagent { agent_id } => ("subagent", agent_id.as_str()),
    }
}

fn rail_passes_filter(rail: &OwnerRail, filter: OwnerFilter) -> bool {
    matches!(
        (rail, filter),
        (_, OwnerFilter::All)
            | (OwnerRail::Main, OwnerFilter::Main)
            | (OwnerRail::Subagent { .. }, OwnerFilter::Subagent)
    )
}

fn attributed_cost(delta_tokens: i64, model: &str, pricing: &PricingTable) -> f64 {
    if delta_tokens <= 0 {
        return 0.0;
    }
    let Some(rate) = crate::analyze::cost::lookup_model_rate(model, pricing) else {
        return 0.0;
    };
    // Charge at cache_read because cache_read is what every *future*
    // inference pays for the persisted prefix this delta added. The
    // model's first inference after the prompt grows pays cache_write
    // once; every subsequent inference pays cache_read. We bill at
    // cache_read here so the "what did this cost me" number reflects
    // the steady-state, not the one-shot.
    (delta_tokens as f64 / 1_000_000.0) * rate.cache_read
}

// ---------------------------------------------------------------------------
// Timeline construction (DFS of spans across the session)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TimelineItem {
    session_id: String,
    turn_id: String,
    owner: OwnerRail,
    kind: TimelineKind,
    start_ms: i64,
    end_ms: i64,
}

#[derive(Debug, Clone)]
enum TimelineKind {
    Inference {
        context_tokens: u64,
        model: String,
    },
    ToolResult {
        tool_use_id: String,
        tool_name: String,
        approx_bytes: u64,
        truncated: bool,
    },
    UserPrompt {
        approx_tokens: u64,
        has_system_reminder: bool,
    },
    // Reserved for the system-reminder detection follow-up (#425). The
    // span-tree builders do not yet synthesize `SystemReminder` leaves,
    // so this variant is unreachable from the live timeline today; it's
    // kept in the shape so the day #425 lands no consumer surface
    // changes. Suppress dead_code until the builder wires it up.
    #[allow(dead_code)]
    SystemReminder {
        source: ReminderSource,
        approx_tokens: u64,
    },
}

impl TimelineItem {
    fn to_intervening_step(&self) -> Option<InterveningStep> {
        match &self.kind {
            TimelineKind::ToolResult {
                tool_use_id,
                tool_name,
                approx_bytes,
                truncated,
            } => Some(InterveningStep::ToolResult {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                approx_tokens: *approx_bytes / BYTES_PER_TOKEN,
                approx_bytes: *approx_bytes,
                truncated: *truncated,
            }),
            TimelineKind::UserPrompt {
                approx_tokens,
                has_system_reminder,
            } => Some(InterveningStep::UserPrompt {
                approx_tokens: *approx_tokens,
                has_system_reminder: *has_system_reminder,
            }),
            TimelineKind::SystemReminder {
                source,
                approx_tokens,
            } => Some(InterveningStep::SystemReminder {
                source: *source,
                approx_tokens: *approx_tokens,
            }),
            TimelineKind::Inference { .. } => None,
        }
    }
}

fn build_timeline(trees: &[TurnSpanTree]) -> Vec<TimelineItem> {
    let mut out: Vec<TimelineItem> = Vec::new();
    for tree in trees {
        walk_node(
            &tree.root,
            &tree.session_id,
            &tree.turn_id,
            &OwnerRail::Main,
            &mut out,
        );
    }
    out
}

fn walk_node(
    node: &SpanNode,
    session_id: &str,
    turn_id: &str,
    parent_owner: &OwnerRail,
    out: &mut Vec<TimelineItem>,
) {
    // If this span is a Subagent root, switch the owner rail for the
    // subtree to `Subagent(agent_id)`. The Subagent span itself does
    // not emit a timeline item — it's a rail boundary, not a leaf the
    // delta consumer cares about.
    let owner_for_subtree = if matches!(node.kind, SpanKind::Subagent) {
        let agent_id = match node.attributes.get("agent_id") {
            Some(AttrValue::String(s)) => s.clone(),
            _ => String::new(),
        };
        OwnerRail::Subagent { agent_id }
    } else {
        parent_owner.clone()
    };

    match node.kind {
        SpanKind::Inference => {
            let input = attr_int(node, "tokens.input").unwrap_or(0);
            let cache_read = attr_int(node, "tokens.cache_read").unwrap_or(0);
            let cache_write = attr_int(node, "tokens.cache_write").unwrap_or(0);
            let context_tokens = (input + cache_read + cache_write).max(0) as u64;
            let model = match node.attributes.get("model") {
                Some(AttrValue::String(s)) => s.clone(),
                _ => node.name.clone(),
            };
            out.push(TimelineItem {
                session_id: session_id.to_string(),
                turn_id: turn_id.to_string(),
                owner: parent_owner.clone(),
                kind: TimelineKind::Inference {
                    context_tokens,
                    model,
                },
                start_ms: node.start_ms,
                end_ms: node.end_ms,
            });
        }
        SpanKind::ToolResult => {
            let tool_use_id = match node.attributes.get("tool_use_id") {
                Some(AttrValue::String(s)) => s.clone(),
                _ => String::new(),
            };
            let approx_bytes = attr_int(node, "output_bytes").unwrap_or(0).max(0) as u64;
            let truncated = matches!(
                node.attributes.get("output_truncated"),
                Some(AttrValue::Bool(true))
            );
            // Tool name lives on the parent ToolUse, not the
            // ToolResult — we don't have a back-pointer here, so we
            // emit an empty string and let the parent-loop replace it
            // before pushing. The caller (`walk_node` for ToolUse
            // below) fills it in.
            out.push(TimelineItem {
                session_id: session_id.to_string(),
                turn_id: turn_id.to_string(),
                owner: parent_owner.clone(),
                kind: TimelineKind::ToolResult {
                    tool_use_id,
                    tool_name: String::new(),
                    approx_bytes,
                    truncated,
                },
                start_ms: node.start_ms,
                end_ms: node.end_ms,
            });
        }
        SpanKind::ToolUse => {
            // Walk children; if a child ToolResult lands in `out`,
            // backfill its tool_name from this ToolUse's name.
            let tool_name = node.name.clone();
            let before = out.len();
            for child in &node.children {
                walk_node(child, session_id, turn_id, &owner_for_subtree, out);
            }
            for item in out.iter_mut().skip(before) {
                if let TimelineKind::ToolResult {
                    tool_name: ref mut tn,
                    ..
                } = item.kind
                {
                    if tn.is_empty() {
                        *tn = tool_name.clone();
                    }
                }
            }
            return;
        }
        SpanKind::UserPrompt => {
            out.push(TimelineItem {
                session_id: session_id.to_string(),
                turn_id: turn_id.to_string(),
                owner: parent_owner.clone(),
                kind: TimelineKind::UserPrompt {
                    approx_tokens: 0,
                    has_system_reminder: false,
                },
                start_ms: node.start_ms,
                end_ms: node.end_ms,
            });
        }
        SpanKind::Subagent | SpanKind::Skill | SpanKind::Turn => {
            // Pass-through containers — recurse with the (possibly
            // adjusted) owner rail.
        }
    }

    for child in &node.children {
        walk_node(child, session_id, turn_id, &owner_for_subtree, out);
    }
}

fn attr_int(node: &SpanNode, key: &str) -> Option<i64> {
    match node.attributes.get(key) {
        Some(AttrValue::Int(i)) => Some(*i),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::span_tree::{SpanKind, SpanNode, SpanStatus, TurnSpanTree};
    use crate::reader::{CompactionEvent, SourceKind};

    fn make_inf(
        req_id: &str,
        model: &str,
        input: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> SpanNode {
        let mut n = SpanNode::new(SpanKind::Inference, model);
        n.set_attr("model", AttrValue::str(model));
        n.set_attr("request_id", AttrValue::str(req_id));
        n.set_attr("tokens.input", AttrValue::Int(input));
        n.set_attr("tokens.output", AttrValue::Int(0));
        n.set_attr("tokens.cache_read", AttrValue::Int(cache_read));
        n.set_attr("tokens.cache_write", AttrValue::Int(cache_write));
        n.set_attr("tokens.reasoning", AttrValue::Int(0));
        n
    }

    fn make_tool_use(name: &str, tool_use_id: &str) -> SpanNode {
        let mut n = SpanNode::new(SpanKind::ToolUse, name);
        n.set_attr("tool_use_id", AttrValue::str(tool_use_id));
        n
    }

    fn make_tool_result(tool_use_id: &str, bytes: i64, truncated: bool) -> SpanNode {
        let mut n = SpanNode::new(SpanKind::ToolResult, "tool-result");
        n.set_attr("tool_use_id", AttrValue::str(tool_use_id));
        n.set_attr("output_bytes", AttrValue::Int(bytes));
        if truncated {
            n.set_attr("output_truncated", AttrValue::Bool(true));
        }
        n
    }

    fn make_user_prompt() -> SpanNode {
        SpanNode::new(SpanKind::UserPrompt, "user-prompt")
    }

    fn turn_tree(session: &str, turn: &str, root: SpanNode) -> TurnSpanTree {
        TurnSpanTree {
            session_id: session.to_string(),
            turn_id: turn.to_string(),
            turn_number: 0,
            root,
        }
    }

    /// Build a single-turn fixture: two inferences on the main rail
    /// with a Bash tool_result between them whose 40_000 bytes
    /// translates to a ~10k token jump. The delta should surface as
    /// the top row with Bash as the driver.
    #[test]
    fn bash_blowup_surfaces_as_top_delta_with_bash_driver() {
        // inference #1: context = 1000
        let inf1 = make_inf("req-1", "claude-sonnet-4-6", 1000, 0, 0);
        let mut bash_use = make_tool_use("Bash", "tu-1");
        bash_use
            .children
            .push(make_tool_result("tu-1", 40_000, false));
        let mut inf1 = inf1;
        inf1.children.push(bash_use);

        // inference #2: context jumped to 12_000 — delta of 11_000.
        let inf2 = make_inf("req-2", "claude-sonnet-4-6", 12_000, 0, 0);

        let mut root = SpanNode::new(SpanKind::Turn, "turn");
        root.status = SpanStatus::Ok;
        root.children.push(make_user_prompt());
        root.children.push(inf1);
        root.children.push(inf2);

        let tree = turn_tree("sess-1", "msg-1", root);
        let pricing = crate::analyze::pricing::load_builtin_pricing();
        let opts = ContextDeltaOpts::default();
        let deltas = deltas_for_session(&[tree], &[], &pricing, &opts);
        assert_eq!(deltas.len(), 1, "one pairwise delta expected");
        let d = &deltas[0];
        assert_eq!(d.session_id, "sess-1");
        assert_eq!(d.turn_id, "msg-1");
        assert_eq!(d.owner_rail, OwnerRail::Main);
        assert_eq!(d.prior_context_tokens, 1000);
        assert_eq!(d.current_context_tokens, 12_000);
        assert_eq!(d.delta_tokens, 11_000);
        assert_eq!(d.intervening.len(), 1);
        match &d.intervening[0] {
            InterveningStep::ToolResult {
                tool_name,
                approx_bytes,
                approx_tokens,
                truncated,
                ..
            } => {
                assert_eq!(tool_name, "Bash");
                assert_eq!(*approx_bytes, 40_000);
                assert_eq!(*approx_tokens, 10_000);
                assert!(!*truncated);
            }
            other => panic!("expected ToolResult step, got {other:?}"),
        }
        // The driver_label helper should mention Bash.
        assert!(d.intervening[0].driver_label().contains("Bash"));
        // Cost is non-negative.
        assert!(d.attributed_cost_usd >= 0.0);
    }

    /// Compaction handling: a CompactionEvent between two inferences
    /// where the second has *less* context than the first must surface
    /// as `Compaction { tokens_freed }`, NOT a negative `delta_tokens`.
    #[test]
    fn compaction_replaces_negative_delta() {
        let inf1 = make_inf("req-1", "claude-sonnet-4-6", 50_000, 0, 0);
        // After compaction, context drops to 8_000.
        let inf2 = make_inf("req-2", "claude-sonnet-4-6", 8_000, 0, 0);

        // Stamp timestamps so the compaction event sits between them.
        let mut inf1 = inf1;
        inf1.start_ms = 1_776_643_201_000;
        inf1.end_ms = 1_776_643_202_000;
        let mut inf2 = inf2;
        inf2.start_ms = 1_776_643_204_000;
        inf2.end_ms = 1_776_643_205_000;

        let mut root = SpanNode::new(SpanKind::Turn, "turn");
        root.children.push(make_user_prompt());
        root.children.push(inf1);
        root.children.push(inf2);
        let tree = turn_tree("sess-1", "msg-1", root);

        let compaction = CompactionEvent {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: "sess-1".into(),
            ts: "2026-04-20T00:00:03.000Z".into(),
            preceding_message_id: Some("msg-1".into()),
            tokens_before_compact: Some(50_000),
        };

        let pricing = crate::analyze::pricing::load_builtin_pricing();
        // Opts: min_delta 0 so the row isn't filtered out (delta_tokens is 0).
        let opts = ContextDeltaOpts {
            min_delta: Some(0),
            ..ContextDeltaOpts::default()
        };
        let deltas = deltas_for_session(&[tree], &[compaction], &pricing, &opts);
        assert_eq!(deltas.len(), 1);
        let d = &deltas[0];
        assert_eq!(d.delta_tokens, 0, "compaction clamps to 0");
        let has_compaction = d
            .intervening
            .iter()
            .any(|s| matches!(s, InterveningStep::Compaction { tokens_freed } if *tokens_freed == 42_000));
        assert!(
            has_compaction,
            "expected Compaction step with tokens_freed=42000, got {:?}",
            d.intervening
        );
    }

    /// Subagent isolation: a main-rail Inference and a subagent
    /// Inference both happen, with a subagent tool_result between
    /// them. The main-rail delta must NOT include the subagent's
    /// tool_result.
    #[test]
    fn subagent_isolation_main_rail_excludes_subagent_results() {
        // Main rail: two inferences with a 10k context jump.
        let mut main_inf1 = make_inf("req-main-1", "claude-sonnet-4-6", 1000, 0, 0);
        // Add a Task tool_use that fans out to a Subagent with its own
        // inference + tool_result. The subagent's tool_result has
        // 40k bytes (~10k tokens) — and the main rail's tool_use also
        // gets a small result so the main delta is non-zero.
        let mut task_use = make_tool_use("Task", "tu-task");

        let mut sub_node = SpanNode::new(SpanKind::Subagent, "general-purpose");
        sub_node.set_attr("agent_id", AttrValue::str("agent-a"));
        // Subagent inferences:
        let sub_inf1 = make_inf("req-sub-1", "claude-sonnet-4-6", 2000, 0, 0);
        let mut sub_bash = make_tool_use("Bash", "tu-sub-bash");
        sub_bash
            .children
            .push(make_tool_result("tu-sub-bash", 40_000, false));
        let mut sub_inf1 = sub_inf1;
        sub_inf1.children.push(sub_bash);
        let sub_inf2 = make_inf("req-sub-2", "claude-sonnet-4-6", 12_000, 0, 0);
        sub_node.children.push(sub_inf1);
        sub_node.children.push(sub_inf2);
        task_use.children.push(sub_node);
        main_inf1.children.push(task_use);

        // Main rail #2: small jump only (no big tool_result on its own).
        let main_inf2 = make_inf("req-main-2", "claude-sonnet-4-6", 3000, 0, 0);

        let mut root = SpanNode::new(SpanKind::Turn, "turn");
        root.children.push(make_user_prompt());
        root.children.push(main_inf1);
        root.children.push(main_inf2);
        let tree = turn_tree("sess-1", "msg-1", root);

        let pricing = crate::analyze::pricing::load_builtin_pricing();
        let opts = ContextDeltaOpts {
            min_delta: Some(0),
            ..ContextDeltaOpts::default()
        };
        let deltas = deltas_for_session(&[tree], &[], &pricing, &opts);

        // We expect one main-rail delta and one subagent-rail delta.
        let main_delta = deltas
            .iter()
            .find(|d| d.owner_rail == OwnerRail::Main)
            .expect("main-rail delta missing");
        let sub_delta = deltas
            .iter()
            .find(|d| matches!(&d.owner_rail, OwnerRail::Subagent { agent_id } if agent_id == "agent-a"))
            .expect("subagent-rail delta missing");

        // Main delta intervening must NOT include the subagent's tool_result.
        for step in &main_delta.intervening {
            if let InterveningStep::ToolResult { tool_use_id, .. } = step {
                assert_ne!(
                    tool_use_id, "tu-sub-bash",
                    "main rail must NOT see the subagent's tool_result"
                );
            }
        }

        // Subagent delta intervening SHOULD include its own Bash result.
        let sub_has_bash = sub_delta.intervening.iter().any(|s| match s {
            InterveningStep::ToolResult { tool_use_id, .. } => tool_use_id == "tu-sub-bash",
            _ => false,
        });
        assert!(
            sub_has_bash,
            "subagent rail must see its own Bash tool_result"
        );
    }

    /// Empty rail (single inference, no prev) → no delta emitted, no
    /// panic.
    #[test]
    fn single_inference_yields_no_delta() {
        let inf1 = make_inf("req-1", "claude-sonnet-4-6", 1000, 0, 0);
        let mut root = SpanNode::new(SpanKind::Turn, "turn");
        root.children.push(make_user_prompt());
        root.children.push(inf1);
        let tree = turn_tree("sess-1", "msg-1", root);
        let pricing = crate::analyze::pricing::load_builtin_pricing();
        let deltas = deltas_for_session(&[tree], &[], &pricing, &ContextDeltaOpts::default());
        assert!(
            deltas.is_empty(),
            "single inference must not emit a pairwise delta"
        );
    }

    /// `min_delta` filters out small jumps.
    #[test]
    fn min_delta_filters_small_jumps() {
        let inf1 = make_inf("req-1", "claude-sonnet-4-6", 1000, 0, 0);
        // Small jump: +500 tokens.
        let inf2 = make_inf("req-2", "claude-sonnet-4-6", 1500, 0, 0);

        let mut root = SpanNode::new(SpanKind::Turn, "turn");
        root.children.push(make_user_prompt());
        root.children.push(inf1);
        root.children.push(inf2);
        let tree = turn_tree("sess-1", "msg-1", root);
        let pricing = crate::analyze::pricing::load_builtin_pricing();
        // Default min_delta is 1000; 500 < 1000 → filtered out.
        let deltas = deltas_for_session(&[tree], &[], &pricing, &ContextDeltaOpts::default());
        assert!(deltas.is_empty(), "500 token jump must be filtered");

        // Lower the threshold to 100 → row appears.
        let opts = ContextDeltaOpts {
            min_delta: Some(100),
            ..ContextDeltaOpts::default()
        };
        let deltas = deltas_for_session(
            &[turn_tree("sess-1", "msg-1", root_with_two_infs(1000, 1500))],
            &[],
            &pricing,
            &opts,
        );
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].delta_tokens, 500);
    }

    fn root_with_two_infs(ctx1: i64, ctx2: i64) -> SpanNode {
        let mut root = SpanNode::new(SpanKind::Turn, "turn");
        root.children.push(make_user_prompt());
        root.children
            .push(make_inf("req-1", "claude-sonnet-4-6", ctx1, 0, 0));
        root.children
            .push(make_inf("req-2", "claude-sonnet-4-6", ctx2, 0, 0));
        root
    }

    /// `--top N` caps output.
    #[test]
    fn top_caps_output() {
        // Build a tree with 5 inferences, each adding 5000 tokens.
        let mut root = SpanNode::new(SpanKind::Turn, "turn");
        root.children.push(make_user_prompt());
        let ctx_steps = [1000, 6000, 11_000, 16_000, 21_000];
        for (i, c) in ctx_steps.iter().enumerate() {
            root.children
                .push(make_inf(&format!("req-{i}"), "claude-sonnet-4-6", *c, 0, 0));
        }
        let tree = turn_tree("sess-1", "msg-1", root);
        let pricing = crate::analyze::pricing::load_builtin_pricing();

        // No cap → 4 pairwise deltas (5 inferences = 4 windows).
        let opts = ContextDeltaOpts {
            min_delta: Some(0),
            ..ContextDeltaOpts::default()
        };
        let all = deltas_for_session(std::slice::from_ref(&tree), &[], &pricing, &opts);
        assert_eq!(all.len(), 4);

        // Cap at 2 → only the top 2 deltas.
        let opts = ContextDeltaOpts {
            min_delta: Some(0),
            top: Some(2),
            ..ContextDeltaOpts::default()
        };
        let top2 = deltas_for_session(&[tree], &[], &pricing, &opts);
        assert_eq!(top2.len(), 2);
    }

    /// `--owner main` filter excludes subagent rails.
    #[test]
    fn owner_filter_main_excludes_subagent_rail() {
        // Reuse the subagent-isolation fixture shape.
        let mut main_inf1 = make_inf("req-main-1", "claude-sonnet-4-6", 1000, 0, 0);
        let mut task_use = make_tool_use("Task", "tu-task");
        let mut sub_node = SpanNode::new(SpanKind::Subagent, "general-purpose");
        sub_node.set_attr("agent_id", AttrValue::str("agent-a"));
        sub_node
            .children
            .push(make_inf("req-sub-1", "claude-sonnet-4-6", 2000, 0, 0));
        sub_node
            .children
            .push(make_inf("req-sub-2", "claude-sonnet-4-6", 22_000, 0, 0));
        task_use.children.push(sub_node);
        main_inf1.children.push(task_use);
        let main_inf2 = make_inf("req-main-2", "claude-sonnet-4-6", 3000, 0, 0);

        let mut root = SpanNode::new(SpanKind::Turn, "turn");
        root.children.push(make_user_prompt());
        root.children.push(main_inf1);
        root.children.push(main_inf2);
        let tree = turn_tree("sess-1", "msg-1", root);

        let pricing = crate::analyze::pricing::load_builtin_pricing();
        let opts = ContextDeltaOpts {
            min_delta: Some(0),
            owner: OwnerFilter::Main,
            ..ContextDeltaOpts::default()
        };
        let deltas = deltas_for_session(&[tree], &[], &pricing, &opts);
        for d in &deltas {
            assert_eq!(
                d.owner_rail,
                OwnerRail::Main,
                "owner filter Main must exclude subagent rails"
            );
        }
        assert!(!deltas.is_empty(), "expected at least one main-rail delta");
    }

    /// JSON shape: rail serializes with a `kind` discriminant, steps
    /// keep their kebab-case kind tag. Catch wire-format drift early.
    #[test]
    fn json_shape_uses_kebab_case_discriminants() {
        let d = ContextDelta {
            session_id: "s".into(),
            turn_id: "t".into(),
            inference_idx: 2,
            owner_rail: OwnerRail::Subagent {
                agent_id: "agent-x".into(),
            },
            prior_context_tokens: 10,
            current_context_tokens: 20,
            delta_tokens: 10,
            intervening: vec![InterveningStep::ToolResult {
                tool_use_id: "tu-1".into(),
                tool_name: "Bash".into(),
                approx_tokens: 5,
                approx_bytes: 20,
                truncated: false,
            }],
            attributed_cost_usd: 0.0,
        };
        let s = serde_json::to_string(&d).unwrap();
        assert!(s.contains("\"kind\":\"subagent\""), "got {s}");
        assert!(s.contains("\"agentId\":\"agent-x\""), "got {s}");
        assert!(s.contains("\"kind\":\"tool-result\""), "got {s}");
        assert!(s.contains("\"toolUseId\":\"tu-1\""), "got {s}");
        let back: ContextDelta = serde_json::from_str(&s).unwrap();
        assert_eq!(back, d);
    }

    /// System reminder step surfaces as its own intervening step row.
    /// We construct one directly (the timeline builder doesn't yet
    /// synthesize SystemReminder spans from content sidecars; first-
    /// cut behavior per the issue).
    #[test]
    fn system_reminder_step_round_trips_in_json() {
        let step = InterveningStep::SystemReminder {
            source: ReminderSource::Other,
            approx_tokens: 250,
        };
        let s = serde_json::to_string(&step).unwrap();
        assert!(s.contains("\"kind\":\"system-reminder\""), "got {s}");
        assert!(s.contains("\"source\":\"other\""), "got {s}");
        let back: InterveningStep = serde_json::from_str(&s).unwrap();
        assert_eq!(back, step);
    }
}
