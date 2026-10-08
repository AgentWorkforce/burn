//! Burn's accounting of provider token counters. relayhistory hands over
//! each message's counters as the harness wrote them; what they mean —
//! whether input includes cache reads, which cache tier a write lands in,
//! whether reasoning is part of output — is decided here.

use serde_json::{Map, Value};

use crate::reader::types::{Coverage, SourceKind, Usage};

/// `Usage` plus which counters the provider actually reported.
pub(crate) fn usage_from_raw(source: SourceKind, raw: Option<&str>) -> (Usage, Coverage) {
    let parsed: Option<Value> = raw.and_then(|r| serde_json::from_str(r).ok());
    let Some(obj) = parsed.as_ref().and_then(Value::as_object) else {
        return (Usage::default(), Coverage::default());
    };
    match source {
        SourceKind::Opencode => opencode(obj),
        _ => anthropic(obj),
    }
}

fn n(obj: &Map<String, Value>, key: &str) -> u64 {
    obj.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Anthropic Messages usage: `input_tokens` excludes cache reads; cache
/// writes split by TTL under `cache_creation`, with the flat
/// `cache_creation_input_tokens` billed at the 5-minute rate when no split
/// is reported.
fn anthropic(obj: &Map<String, Value>) -> (Usage, Coverage) {
    let split = obj.get("cache_creation").and_then(Value::as_object);
    let tier = |k: &str| {
        split
            .and_then(|s| s.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let (mut create_5m, create_1h) = (
        tier("ephemeral_5m_input_tokens"),
        tier("ephemeral_1h_input_tokens"),
    );
    if create_5m == 0 && create_1h == 0 {
        create_5m = n(obj, "cache_creation_input_tokens");
    }
    let usage = Usage {
        input: n(obj, "input_tokens"),
        output: n(obj, "output_tokens"),
        reasoning: 0,
        cache_read: n(obj, "cache_read_input_tokens"),
        cache_create_5m: create_5m,
        cache_create_1h: create_1h,
    };
    let coverage = Coverage {
        has_input_tokens: obj.contains_key("input_tokens"),
        has_output_tokens: obj.contains_key("output_tokens"),
        has_cache_read_tokens: obj.contains_key("cache_read_input_tokens"),
        has_cache_create_tokens: obj.contains_key("cache_creation_input_tokens")
            || split.is_some_and(|s| {
                s.contains_key("ephemeral_5m_input_tokens")
                    || s.contains_key("ephemeral_1h_input_tokens")
            }),
        ..Coverage::default()
    };
    (usage, coverage)
}

/// OpenCode's per-message `tokens` object: `cache.write` is billed at the
/// 5-minute rate. A counter is covered when the object names it.
fn opencode(obj: &Map<String, Value>) -> (Usage, Coverage) {
    let cache = obj.get("cache").and_then(Value::as_object);
    let c = |k: &str| {
        cache
            .and_then(|c| c.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let has = |k: &str| obj.get(k).is_some_and(Value::is_u64);
    let has_cache = |k: &str| cache.and_then(|c| c.get(k)).is_some_and(Value::is_u64);
    let usage = Usage {
        input: n(obj, "input"),
        output: n(obj, "output"),
        reasoning: n(obj, "reasoning"),
        cache_read: c("read"),
        cache_create_5m: c("write"),
        cache_create_1h: 0,
    };
    let coverage = Coverage {
        has_input_tokens: has("input"),
        has_output_tokens: has("output"),
        has_reasoning_tokens: has("reasoning"),
        has_cache_read_tokens: has_cache("read"),
        has_cache_create_tokens: has_cache("write"),
        ..Coverage::default()
    };
    (usage, coverage)
}
