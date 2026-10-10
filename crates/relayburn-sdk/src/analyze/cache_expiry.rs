//! Prompt-cache expiry detector.
//!
//! A prompt cache entry lives for its TTL (5 minutes by default, 1 hour when
//! written with the 1-hour TTL) and every read refreshes it. When the next
//! turn on the same cache starts after the TTL has lapsed, the provider no
//! longer has the prefix: the turn re-writes the context at the cache-write
//! tariff instead of reading it at the cache-read tariff. This detector finds
//! those turns and prices the difference.
//!
//! A cache belongs to one owner (main thread or subagent) and one model, so
//! turns are bucketed by `(session_id, owner, model)` and ordered by
//! timestamp. A sidechain without a resolved agent id is its own cache only
//! when it is the whole session (OpenCode child sessions); inside a session
//! with main-thread turns its identity is unknown and it is skipped. A turn
//! counts as a cache expiry when, relative to the previous turn on its cache:
//!
//! - the gap exceeds the shortest TTL of the most recent cache write,
//! - its context is at least half of the previous context (re-created, not
//!   compacted), and
//! - it re-wrote at least [`MIN_RECREATED_TOKENS`] of the previous context's
//!   tokens that it did not read from the cache.

use std::collections::{HashMap, HashSet};

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

    /// Shortest TTL among the cache entries a turn wrote, if it wrote any:
    /// the 5-minute portion expires first and its loss is what an early
    /// resume re-writes.
    fn written_by(usage: &Usage) -> Option<Self> {
        if usage.cache_create_5m > 0 {
            Some(CacheTtl::FiveMinutes)
        } else if usage.cache_create_1h > 0 {
            Some(CacheTtl::OneHour)
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

/// Detect cache expiries in `turns`. With `window_start`, turns before it
/// (ISO timestamps, compared as strings like `Query::since`) only supply the
/// previous cache state; expiries are reported for resumed turns at or after
/// it.
pub(crate) fn detect_cache_expiry(
    turns: &[TurnRecord],
    user_turns: &[UserTurnRecord],
    pricing: &PricingTable,
    window_start: Option<&str>,
) -> Vec<CacheExpiry> {
    let causes = causes_by_following_message(user_turns);
    let mut by_session: IndexMap<&str, Vec<CacheExpiryEvent>> = IndexMap::new();
    for ((session_id, _, _), cache_turns) in turns_by_cache(turns) {
        let events = detect_for_cache(&cache_turns, &causes, pricing, window_start);
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

/// Whose prompt cache a turn reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CacheOwner<'a> {
    Main,
    Agent(&'a str),
    /// A sidechain whose agent id the reader could not resolve.
    UnidentifiedSidechain,
}

impl<'a> CacheOwner<'a> {
    fn of(turn: &'a TurnRecord) -> Self {
        match &turn.subagent {
            None => CacheOwner::Main,
            Some(s) => s
                .agent_id
                .as_deref()
                .map_or(CacheOwner::UnidentifiedSidechain, CacheOwner::Agent),
        }
    }
}

type CacheKey<'a> = (&'a str, CacheOwner<'a>, &'a str);

fn cache_key(turn: &TurnRecord) -> CacheKey<'_> {
    (&turn.session_id, CacheOwner::of(turn), &turn.model)
}

/// The turns of `history` that carry each cache's state into a later
/// window: per cache, its latest turn and its latest cache-writing turn
/// (which sets the TTL the latest turn's reads refresh).
pub(crate) fn cache_state_turns(history: &[TurnRecord]) -> Vec<TurnRecord> {
    let mut latest: HashMap<CacheKey<'_>, &TurnRecord> = HashMap::new();
    let mut latest_write: HashMap<CacheKey<'_>, &TurnRecord> = HashMap::new();
    for turn in history {
        let key = cache_key(turn);
        keep_later(&mut latest, key, turn);
        if CacheTtl::written_by(&turn.usage).is_some() {
            keep_later(&mut latest_write, key, turn);
        }
    }
    let mut out: Vec<TurnRecord> = latest.into_values().cloned().collect();
    for turn in latest_write.into_values() {
        if !out.iter().any(|t| t.message_id == turn.message_id) {
            out.push(turn.clone());
        }
    }
    out
}

fn keep_later<'a>(
    map: &mut HashMap<CacheKey<'a>, &'a TurnRecord>,
    key: CacheKey<'a>,
    turn: &'a TurnRecord,
) {
    let slot = map.entry(key).or_insert(turn);
    if turn.ts > slot.ts {
        *slot = turn;
    }
}

struct TimedTurn<'a> {
    turn: &'a TurnRecord,
    ts_ms: i64,
}

/// Bucket turns per cache and order each bucket by timestamp. Turns without
/// a parseable timestamp cannot be placed on the timeline and are dropped.
fn turns_by_cache(turns: &[TurnRecord]) -> IndexMap<CacheKey<'_>, Vec<TimedTurn<'_>>> {
    let mut out: IndexMap<CacheKey<'_>, Vec<TimedTurn<'_>>> = IndexMap::new();
    for turn in turns {
        let Some(ts_ms) = parse_iso_ms(&turn.ts) else {
            continue;
        };
        out.entry(cache_key(turn))
            .or_default()
            .push(TimedTurn { turn, ts_ms });
    }
    let sessions_with_main: HashSet<&str> = out
        .keys()
        .filter(|(_, owner, _)| *owner == CacheOwner::Main)
        .map(|(session, _, _)| *session)
        .collect();
    out.retain(|(session, owner, _), _| {
        *owner != CacheOwner::UnidentifiedSidechain || !sessions_with_main.contains(session)
    });
    for bucket in out.values_mut() {
        bucket.sort_by_key(|t| (t.ts_ms, t.turn.turn_index));
    }
    out
}

fn detect_for_cache(
    turns: &[TimedTurn<'_>],
    causes: &HashMap<&str, ExpiryCause>,
    pricing: &PricingTable,
    window_start: Option<&str>,
) -> Vec<CacheExpiryEvent> {
    let mut out = Vec::new();
    let mut ttl = CacheTtl::FiveMinutes;
    for pair in turns.windows(2) {
        let (prev, cur) = (pair[0].turn, pair[1].turn);
        ttl = CacheTtl::written_by(&prev.usage).unwrap_or(ttl);
        let gap_ms = pair[1].ts_ms - pair[0].ts_ms;
        if window_start.is_some_and(|start| cur.ts.as_str() < start) || gap_ms <= ttl.millis() {
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

/// Tokens of the previous context that `cur` did not read from the cache and
/// re-wrote, or `None` when `cur` compacted the context or re-wrote too
/// little to matter.
fn recreated_prefix(prev: &Usage, cur: &Usage) -> Option<u64> {
    let prev_context = context_tokens(prev);
    if context_tokens(cur) < prev_context / 2 {
        return None;
    }
    let created = cur.cache_create_5m.saturating_add(cur.cache_create_1h);
    let recreated = created.min(prev_context.saturating_sub(cur.cache_read));
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
