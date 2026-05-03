//! Coverage / fidelity helpers — Rust port of `packages/reader/src/fidelity.ts`.
//!
//! Same policy as the TS helper: parsers default every coverage flag to `false`
//! and explicitly opt fields into `true`. `classify_fidelity` derives the
//! higher-level summary from `granularity` + `coverage`.

use crate::types::{Coverage, Fidelity, FidelityClass, UsageGranularity};

/// Coverage value with every flag defaulted to `false`. Parsers should clone
/// this and flip the flags they actually populate.
pub const fn empty_coverage() -> Coverage {
    Coverage {
        has_input_tokens: false,
        has_output_tokens: false,
        has_reasoning_tokens: false,
        has_cache_read_tokens: false,
        has_cache_create_tokens: false,
        has_tool_calls: false,
        has_tool_result_events: false,
        has_session_relationships: false,
        has_raw_content: false,
    }
}

fn full_required(c: &Coverage) -> bool {
    c.has_input_tokens
        && c.has_output_tokens
        && c.has_cache_read_tokens
        && c.has_tool_calls
        && c.has_tool_result_events
        && c.has_session_relationships
}

fn usage_required(c: &Coverage) -> bool {
    c.has_input_tokens && c.has_output_tokens
}

/// Pure derivation of `FidelityClass` from a `granularity` + `coverage` pair.
/// No I/O, no allocation; safe to call from any layer.
pub fn classify_fidelity(granularity: UsageGranularity, coverage: &Coverage) -> FidelityClass {
    match granularity {
        UsageGranularity::CostOnly => FidelityClass::CostOnly,
        UsageGranularity::PerSessionAggregate => FidelityClass::AggregateOnly,
        UsageGranularity::PerTurn | UsageGranularity::PerMessage => {
            if full_required(coverage) {
                FidelityClass::Full
            } else if usage_required(coverage) {
                FidelityClass::UsageOnly
            } else {
                FidelityClass::Partial
            }
        }
    }
}

/// Convenience constructor — parsers build a `Coverage`, declare their
/// granularity, and get a fully-populated `Fidelity` with the derived class.
pub fn make_fidelity(granularity: UsageGranularity, coverage: Coverage) -> Fidelity {
    let class = classify_fidelity(granularity, &coverage);
    Fidelity {
        granularity,
        coverage,
        class,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_flags(set: impl FnOnce(&mut Coverage)) -> Coverage {
        let mut c = empty_coverage();
        set(&mut c);
        c
    }

    #[test]
    fn cost_only_dominates_coverage() {
        let all_on = with_flags(|c| {
            c.has_input_tokens = true;
            c.has_output_tokens = true;
            c.has_cache_read_tokens = true;
            c.has_tool_calls = true;
            c.has_tool_result_events = true;
            c.has_session_relationships = true;
        });
        assert_eq!(
            classify_fidelity(UsageGranularity::CostOnly, &all_on),
            FidelityClass::CostOnly
        );
        assert_eq!(
            classify_fidelity(UsageGranularity::CostOnly, &empty_coverage()),
            FidelityClass::CostOnly
        );
    }

    #[test]
    fn per_session_aggregate_returns_aggregate_only() {
        let cov = with_flags(|c| {
            c.has_input_tokens = true;
            c.has_output_tokens = true;
        });
        assert_eq!(
            classify_fidelity(UsageGranularity::PerSessionAggregate, &cov),
            FidelityClass::AggregateOnly
        );
    }

    #[test]
    fn per_turn_full_when_all_required_set() {
        let cov = with_flags(|c| {
            c.has_input_tokens = true;
            c.has_output_tokens = true;
            c.has_cache_read_tokens = true;
            c.has_tool_calls = true;
            c.has_tool_result_events = true;
            c.has_session_relationships = true;
        });
        assert_eq!(
            classify_fidelity(UsageGranularity::PerTurn, &cov),
            FidelityClass::Full
        );
    }

    #[test]
    fn usage_only_when_input_output_present_but_tools_missing() {
        let cov = with_flags(|c| {
            c.has_input_tokens = true;
            c.has_output_tokens = true;
            c.has_cache_read_tokens = true;
            c.has_session_relationships = true;
        });
        assert_eq!(
            classify_fidelity(UsageGranularity::PerTurn, &cov),
            FidelityClass::UsageOnly
        );
    }

    #[test]
    fn partial_when_output_missing() {
        let cov = with_flags(|c| {
            c.has_input_tokens = true;
            c.has_cache_read_tokens = true;
            c.has_tool_calls = true;
            c.has_tool_result_events = true;
            c.has_session_relationships = true;
        });
        assert_eq!(
            classify_fidelity(UsageGranularity::PerTurn, &cov),
            FidelityClass::Partial
        );
    }

    #[test]
    fn per_message_with_full_required_is_full() {
        let cov = with_flags(|c| {
            c.has_input_tokens = true;
            c.has_output_tokens = true;
            c.has_cache_read_tokens = true;
            c.has_tool_calls = true;
            c.has_tool_result_events = true;
            c.has_session_relationships = true;
        });
        assert_eq!(
            classify_fidelity(UsageGranularity::PerMessage, &cov),
            FidelityClass::Full
        );
    }

    #[test]
    fn make_fidelity_packs_class() {
        let cov = with_flags(|c| {
            c.has_input_tokens = true;
            c.has_output_tokens = true;
        });
        let f = make_fidelity(UsageGranularity::PerTurn, cov.clone());
        assert_eq!(f.granularity, UsageGranularity::PerTurn);
        assert_eq!(f.coverage, cov);
        assert_eq!(f.class, FidelityClass::UsageOnly);
    }
}
