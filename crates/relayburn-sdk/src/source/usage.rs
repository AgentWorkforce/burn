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
        SourceKind::CopilotCli => copilot_message(obj),
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

/// Copilot CLI's per-message counter: `assistant.message.outputTokens`, the
/// only usage it persists per request. Input and cache stay unmeasured.
fn copilot_message(obj: &Map<String, Value>) -> (Usage, Coverage) {
    let usage = Usage {
        output: n(obj, "outputTokens"),
        ..Usage::default()
    };
    let coverage = Coverage {
        has_output_tokens: obj.get("outputTokens").is_some_and(Value::is_u64),
        ..Coverage::default()
    };
    (usage, coverage)
}

/// One model's running totals from a Copilot `session.shutdown`
/// `modelMetrics[model].usage`: `inputTokens` includes cache reads and
/// writes, `outputTokens` includes reasoning. Cache writes carry no TTL and
/// bill at the 5-minute rate. `None` when a counter is missing or not a
/// non-negative integer.
pub(crate) fn copilot_totals(usage: &Value) -> Option<Usage> {
    let counter = |key: &str| usage.get(key)?.as_u64();
    let (read, write) = (counter("cacheReadTokens")?, counter("cacheWriteTokens")?);
    Some(Usage {
        input: counter("inputTokens")?.checked_sub(read + write)?,
        output: counter("outputTokens")?,
        reasoning: counter("reasoningTokens").unwrap_or(0),
        cache_read: read,
        cache_create_5m: write,
        cache_create_1h: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude(raw: &str) -> (Usage, Coverage) {
        usage_from_raw(SourceKind::ClaudeCode, Some(raw))
    }

    #[test]
    fn claude_cache_writes_are_covered_by_either_tier_or_the_flat_counter() {
        let (usage, coverage) = claude(r#"{"cache_creation": {"ephemeral_1h_input_tokens": 7}}"#);
        assert_eq!((usage.cache_create_5m, usage.cache_create_1h), (0, 7));
        assert!(coverage.has_cache_create_tokens);

        let (usage, coverage) = claude(r#"{"cache_creation": {"ephemeral_5m_input_tokens": 3}}"#);
        assert_eq!((usage.cache_create_5m, usage.cache_create_1h), (3, 0));
        assert!(coverage.has_cache_create_tokens);

        let (usage, coverage) = claude(r#"{"cache_creation_input_tokens": 9}"#);
        assert_eq!((usage.cache_create_5m, usage.cache_create_1h), (9, 0));
        assert!(coverage.has_cache_create_tokens);

        let (_, coverage) = claude(r#"{"input_tokens": 1, "cache_creation": {}}"#);
        assert!(!coverage.has_cache_create_tokens);
        assert!(coverage.has_input_tokens);
    }

    #[test]
    fn copilot_messages_measure_output_only() {
        let (usage, coverage) =
            usage_from_raw(SourceKind::CopilotCli, Some(r#"{"outputTokens": 42}"#));
        assert_eq!(usage.output, 42);
        assert!(coverage.has_output_tokens && !coverage.has_input_tokens);
        let (_, coverage) = usage_from_raw(SourceKind::CopilotCli, Some(r#"{"outputTokens": -1}"#));
        assert!(!coverage.has_output_tokens);
    }

    #[test]
    fn copilot_totals_split_cache_out_of_input() {
        let usage = serde_json::json!({"inputTokens": 30000, "outputTokens": 82,
            "cacheReadTokens": 20000, "cacheWriteTokens": 9000, "reasoningTokens": 4});
        let totals = copilot_totals(&usage).unwrap();
        assert_eq!(
            (
                totals.input,
                totals.cache_read,
                totals.cache_create_5m,
                totals.output,
                totals.reasoning
            ),
            (1000, 20000, 9000, 82, 4)
        );
        assert!(
            copilot_totals(&serde_json::json!({"inputTokens": 1, "outputTokens": 1,
            "cacheReadTokens": 2, "cacheWriteTokens": 0}))
            .is_none()
        );
        assert!(copilot_totals(&serde_json::json!({"inputTokens": 1})).is_none());
    }
}
