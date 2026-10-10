//! Prompt-cache expiry detector.
//!
//! A prompt cache entry lives for its TTL (5 minutes by default, 1 hour when
//! written with the 1-hour TTL) and every read refreshes it. When the next
//! turn of the same agent starts after the TTL has lapsed, the provider no
//! longer has the prefix: the turn re-writes the whole context at the
//! cache-write tariff instead of reading it at the cache-read tariff. This
//! detector finds those turns and prices the difference.
//!
//! Each agent (main thread or subagent) owns its own cache prefix, so turns
//! are bucketed by `(session_id, subagent.agent_id)` and ordered by
//! timestamp. A turn counts as a cache expiry when, relative to the previous
//! turn of the same agent and model:
//!
//! - the gap exceeds the TTL of the most recent cache write,
//! - its cache read covers less than half of the previous context (the
//!   prefix was not served from cache),
//! - its own context is at least half of the previous context (the context
//!   was re-created, not compacted), and
//! - it wrote at least [`MIN_RECREATED_TOKENS`] tokens to the cache.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::analyze::cost::{effective_model_rate, lookup_model_rate, PER_MILLION};
use crate::analyze::findings::{WasteAction, WasteFinding};
use crate::analyze::pricing::PricingTable;
use crate::analyze::util::{fmt_usd, format_with_commas};
use crate::reader::{TurnRecord, Usage, UserTurnBlockKind, UserTurnRecord};
use crate::util::time::parse_iso_ms;

const CACHE_EXPIRY_KIND: &str = "cache-expiry";

const FIVE_MINUTES_MS: i64 = 5 * 60 * 1000;
const ONE_HOUR_MS: i64 = 60 * 60 * 1000;

/// Smallest prefix Anthropic caches; re-creations below it are noise.
const MIN_RECREATED_TOKENS: u64 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheTtl {
    FiveMinutes,
    OneHour,
}

impl CacheTtl {
    fn millis(self) -> i64 {
        match self {
            CacheTtl::FiveMinutes => FIVE_MINUTES_MS,
            CacheTtl::OneHour => ONE_HOUR_MS,
        }
    }

    fn label(self) -> &'static str {
        match self {
            CacheTtl::FiveMinutes => "5-minute",
            CacheTtl::OneHour => "1-hour",
        }
    }

    /// TTL of the cache entries a turn wrote, if it wrote any.
    fn written_by(usage: &Usage) -> Option<Self> {
        if usage.cache_create_1h > 0 {
            Some(CacheTtl::OneHour)
        } else if usage.cache_create_5m > 0 {
            Some(CacheTtl::FiveMinutes)
        } else {
            None
        }
    }
}

/// What the agent was waiting on while the cache expired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpiryCause {
    /// The turn answers a user message: the user came back after the TTL.
    UserIdle,
    /// The turn answers tool results only: a tool outlasted the TTL.
    ToolWait,
    /// No user-turn record links to the turn.
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CacheExpiryEvent {
    pub message_id: String,
    pub gap_ms: i64,
    pub ttl: CacheTtl,
    pub cause: ExpiryCause,
    pub recreated_tokens: u64,
    pub penalty_usd: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CacheExpiry {
    pub session_id: String,
    pub events: Vec<CacheExpiryEvent>,
}

impl CacheExpiry {
    fn total_tokens(&self) -> u64 {
        self.events.iter().map(|e| e.recreated_tokens).sum()
    }

    fn total_usd(&self) -> f64 {
        self.events.iter().map(|e| e.penalty_usd).sum()
    }
}

pub(crate) fn detect_cache_expiry(
    turns: &[TurnRecord],
    user_turns: &[UserTurnRecord],
    pricing: &PricingTable,
) -> Vec<CacheExpiry> {
    let causes = causes_by_following_message(user_turns);
    let mut by_session: IndexMap<&str, Vec<CacheExpiryEvent>> = IndexMap::new();
    for ((session_id, _agent), agent_turns) in turns_by_agent(turns) {
        let events = detect_for_agent(&agent_turns, &causes, pricing);
        if !events.is_empty() {
            by_session.entry(session_id).or_default().extend(events);
        }
    }
    by_session
        .into_iter()
        .map(|(session_id, events)| CacheExpiry {
            session_id: session_id.to_string(),
            events,
        })
        .collect()
}

/// Map each assistant message id to the cause implied by the user turn that
/// precedes it.
fn causes_by_following_message(user_turns: &[UserTurnRecord]) -> HashMap<&str, ExpiryCause> {
    user_turns
        .iter()
        .filter_map(|ut| {
            let following = ut.following_message_id.as_deref()?;
            let has_text = ut.blocks.iter().any(|b| b.kind == UserTurnBlockKind::Text);
            let cause = if has_text {
                ExpiryCause::UserIdle
            } else {
                ExpiryCause::ToolWait
            };
            Some((following, cause))
        })
        .collect()
}

struct TimedTurn<'a> {
    turn: &'a TurnRecord,
    ts_ms: i64,
}

/// Bucket turns per agent cache and order each bucket by timestamp. Turns
/// without a parseable timestamp cannot be placed on the timeline and are
/// dropped.
fn turns_by_agent(turns: &[TurnRecord]) -> IndexMap<(&str, Option<&str>), Vec<TimedTurn<'_>>> {
    let mut out: IndexMap<(&str, Option<&str>), Vec<TimedTurn<'_>>> = IndexMap::new();
    for turn in turns {
        let Some(ts_ms) = parse_iso_ms(&turn.ts) else {
            continue;
        };
        let agent = turn.subagent.as_ref().and_then(|s| s.agent_id.as_deref());
        out.entry((turn.session_id.as_str(), agent))
            .or_default()
            .push(TimedTurn { turn, ts_ms });
    }
    for bucket in out.values_mut() {
        bucket.sort_by_key(|t| (t.ts_ms, t.turn.turn_index));
    }
    out
}

fn detect_for_agent(
    turns: &[TimedTurn<'_>],
    causes: &HashMap<&str, ExpiryCause>,
    pricing: &PricingTable,
) -> Vec<CacheExpiryEvent> {
    let mut out = Vec::new();
    let mut ttl = CacheTtl::FiveMinutes;
    for pair in turns.windows(2) {
        let (prev, cur) = (pair[0].turn, pair[1].turn);
        ttl = CacheTtl::written_by(&prev.usage).unwrap_or(ttl);
        let gap_ms = pair[1].ts_ms - pair[0].ts_ms;
        if cur.model != prev.model || gap_ms <= ttl.millis() {
            continue;
        }
        let Some(recreated_tokens) = recreated_prefix(&prev.usage, &cur.usage) else {
            continue;
        };
        let Some(penalty_usd) = expiry_penalty(cur, recreated_tokens, pricing) else {
            continue;
        };
        out.push(CacheExpiryEvent {
            message_id: cur.message_id.clone(),
            gap_ms,
            ttl,
            cause: causes
                .get(cur.message_id.as_str())
                .copied()
                .unwrap_or(ExpiryCause::Unknown),
            recreated_tokens,
            penalty_usd,
        });
    }
    out
}

fn context_tokens(usage: &Usage) -> u64 {
    usage
        .input
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_create_5m)
        .saturating_add(usage.cache_create_1h)
}

/// Tokens of the previous context that `cur` re-wrote to the cache, or
/// `None` when `cur` does not look like a cold re-creation of that context.
fn recreated_prefix(prev: &Usage, cur: &Usage) -> Option<u64> {
    let prev_context = context_tokens(prev);
    let half = prev_context / 2;
    if cur.cache_read >= half || context_tokens(cur) < half {
        return None;
    }
    let created = cur.cache_create_5m.saturating_add(cur.cache_create_1h);
    let recreated = created.min(prev_context - cur.cache_read);
    (recreated >= MIN_RECREATED_TOKENS).then_some(recreated)
}

/// Extra USD `turn` paid to re-write `recreated` tokens versus reading them
/// from a warm cache. `None` when the model is unpriced.
fn expiry_penalty(turn: &TurnRecord, recreated: u64, pricing: &PricingTable) -> Option<f64> {
    let rate = effective_model_rate(&turn.usage, lookup_model_rate(&turn.model, pricing)?);
    let created = turn.usage.cache_create_5m + turn.usage.cache_create_1h;
    let recreated_write = rate.cache_create_cost(&turn.usage) * recreated as f64 / created as f64;
    let warm_read = recreated as f64 / PER_MILLION * rate.cache_read;
    Some(recreated_write - warm_read)
}

pub(crate) fn cache_expiry_to_finding(expiry: &CacheExpiry) -> WasteFinding {
    let usd = expiry.total_usd();
    let tokens = expiry.total_tokens();
    let longest = expiry
        .events
        .iter()
        .max_by_key(|e| e.gap_ms)
        .expect("a CacheExpiry carries at least one event");
    let title = format!(
        "Prompt cache expired {}× before resuming (longest gap {})",
        expiry.events.len(),
        format_gap(longest.gap_ms)
    );
    let detail = format!(
        "{tokens} tokens of context were re-written to the prompt cache after its TTL lapsed \
(longest gap {gap} against a {ttl} TTL), costing {usd} more than warm cache reads.",
        tokens = format_with_commas(tokens),
        gap = format_gap(longest.gap_ms),
        ttl = longest.ttl.label(),
        usd = fmt_usd(usd),
    );
    let mut finding =
        WasteFinding::session_cost(CACHE_EXPIRY_KIND, &expiry.session_id, usd, title, detail)
            .with_tokens_per_session(tokens);
    finding.actions.push(advice(dominant_cause(expiry)));
    finding
}

/// The cause behind the largest share of the session's penalty.
fn dominant_cause(expiry: &CacheExpiry) -> ExpiryCause {
    let mut by_cause: Vec<(ExpiryCause, f64)> = Vec::new();
    for e in &expiry.events {
        match by_cause.iter_mut().find(|(c, _)| *c == e.cause) {
            Some((_, usd)) => *usd += e.penalty_usd,
            None => by_cause.push((e.cause, e.penalty_usd)),
        }
    }
    by_cause
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(ExpiryCause::Unknown, |(cause, _)| cause)
}

fn advice(cause: ExpiryCause) -> WasteAction {
    let text = match cause {
        ExpiryCause::ToolWait => {
            "A tool call outlasted the prompt-cache TTL. Run long jobs in the background so \
the agent keeps the cache warm, or use 1-hour prompt caching for long-running work."
        }
        ExpiryCause::UserIdle | ExpiryCause::Unknown => {
            "Before stepping away from a large session, run /compact so the next turn re-caches \
a summary instead of the full context, or start a fresh session when you return."
        }
    };
    WasteAction::Paste {
        label: "Avoid re-caching the full context".to_string(),
        text: text.to_string(),
    }
}

fn format_gap(ms: i64) -> String {
    let minutes = ms / 60_000;
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m:02}m"),
    }
}

#[cfg(test)]
#[path = "cache_expiry_tests.rs"]
mod tests;
