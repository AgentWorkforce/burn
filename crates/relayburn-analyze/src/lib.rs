//! Rust port of `@relayburn/analyze`. See AgentWorkforce/burn#244.
//!
//! This crate is a work-in-progress port of the TS analyze package. The
//! foundation modules (`pricing`, `cost`, `fidelity`) land first because
//! nearly every higher-level analyzer (compare, hotspots, overhead, ghost
//! surface) consumes them. Follow-up sub-issues fill in the remaining
//! modules.
//!
//! # Numeric precision
//!
//! USD costs are represented as `f64` to match the TS `number` type used in
//! `@relayburn/analyze`. The per-record math in `cost::cost_for_usage`
//! preserves the same accumulation order as the TS implementation so output
//! stays bit-for-bit equivalent on the cost-test fixture corpus, with the
//! 1e-9 USD precision contract that the future `overhead` sub-issue gates
//! against.

pub mod cost;
pub mod fidelity;
pub mod pricing;
pub mod provider;
pub mod provider_reattribution;

pub use cost::{
    cost_for_turn, cost_for_usage, lookup_model_rate, sum_costs, CostBreakdown, CostForUsageOptions,
};
pub use fidelity::{
    empty_fidelity_summary, has_minimum_fidelity, summarize_fidelity, summarize_fidelity_from_iter,
    FidelitySummary, COVERAGE_FIELDS,
};
pub use pricing::{
    flatten_value, load_builtin_pricing, load_pricing, ModelCost, PricingTable, ReasoningMode,
};
pub use provider::{
    aggregate_by_provider, filter_turns_by_provider, filter_turns_by_provider_with_rules,
    provider_for, provider_for_model, provider_for_model_with_rules, provider_for_turn,
    provider_for_with_rules, resolve_turn_provider, AggregateByProviderOptions, AsTurnLike,
    CoverageField, FieldCoverage, ProviderAggregateRow, ProviderFilter, RowCoverage, TurnProvider,
    UsageCostAggregateRow,
};
pub use provider_reattribution::{
    default_rules, extend_default_rules, resolve_provider, resolve_provider_with_rules,
    ProviderPattern, ProviderResolution, ProviderRule,
};
