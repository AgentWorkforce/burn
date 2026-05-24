//! Coverage / fidelity helpers — Rust port of `packages/reader/src/fidelity.ts`.

use crate::reader::types::{Coverage, Fidelity, FidelityClass, UsageGranularity};

impl Fidelity {
    /// Build a `Fidelity` from a granularity + coverage pair. Equivalent to
    /// the TS `makeFidelity`; the derived `class` field is filled in via
    /// [`classify_fidelity`].
    pub fn new(granularity: UsageGranularity, coverage: Coverage) -> Self {
        let class = classify_fidelity(granularity, &coverage);
        Self {
            granularity,
            coverage,
            class,
        }
    }
}

/// Pure derivation of `FidelityClass` from a `granularity` + `coverage` pair.
/// No I/O, no allocation; safe to call from any layer (reader during
/// construction, analyze for re-derivation, CLI for post-hoc filters).
///
///   - `cost-only`        → granularity says we only have a price, not tokens
///   - `aggregate-only`   → per-session totals; per-turn fields are estimates
///   - `usage-only`       → per-turn input/output but no tool-result chronology
///   - `full`             → meets `Coverage::is_full`
///   - `partial`          → has *something* useful but less than usage-only
pub fn classify_fidelity(granularity: UsageGranularity, coverage: &Coverage) -> FidelityClass {
    match granularity {
        UsageGranularity::CostOnly => FidelityClass::CostOnly,
        UsageGranularity::PerSessionAggregate => FidelityClass::AggregateOnly,
        UsageGranularity::PerTurn | UsageGranularity::PerMessage => {
            if coverage.is_full() {
                FidelityClass::Full
            } else if coverage.has_per_turn_usage() {
                FidelityClass::UsageOnly
            } else {
                FidelityClass::Partial
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_flags(set: impl FnOnce(&mut Coverage)) -> Coverage {
        let mut c = Coverage::EMPTY;
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
            FidelityClass::CostOnly,
        );
        assert_eq!(
            classify_fidelity(UsageGranularity::CostOnly, &Coverage::EMPTY),
            FidelityClass::CostOnly,
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
            FidelityClass::AggregateOnly,
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
            FidelityClass::Full,
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
            FidelityClass::UsageOnly,
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
            FidelityClass::Partial,
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
            FidelityClass::Full,
        );
    }

    #[test]
    fn fidelity_new_packs_class() {
        let cov = with_flags(|c| {
            c.has_input_tokens = true;
            c.has_output_tokens = true;
        });
        let f = Fidelity::new(UsageGranularity::PerTurn, cov.clone());
        assert_eq!(f.granularity, UsageGranularity::PerTurn);
        assert_eq!(f.coverage, cov);
        assert_eq!(f.class, FidelityClass::UsageOnly);
    }
}
