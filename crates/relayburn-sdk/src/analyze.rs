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
//! stays bit-for-bit equivalent on the cost-test fixture corpus, holding to
//! the 1e-9 USD precision contract that `overhead` and `hotspots` gate
//! against.

pub mod claude_md;
pub mod compare;
pub mod context_delta;
pub mod cost;
pub mod fidelity;
pub mod findings;
pub mod flow_graph;
pub mod ghost_surface;
pub mod ghost_surface_inputs;
pub mod hotspots;
pub mod overhead;
pub mod patterns;
pub mod pricing;
pub mod provider;
pub mod provider_reattribution;
pub mod quality;
pub mod replacement_savings;
pub mod span_tree;
pub mod subagent_tree;
pub mod tool_call_patterns;
pub mod tool_output_bloat;
mod util;

// Items below are split into `pub use` (mirrored at the SDK lib root) and
// `pub(crate) use` (internal-only; the verb layer reaches them via
// `crate::analyze::*`, but they are deliberately kept off the published
// surface).
pub(crate) use claude_md::{build_trim_recommendations, render_unified_diff_for_recommendation};
pub use claude_md::{MarkdownSection, SessionClaudeMdCost};
pub use compare::{
    build_compare_table, compare_from_archive, CompareCategory, CompareCell,
    CompareFromArchiveResult, CompareOptions, CompareTable, CompareTotals, DEFAULT_MIN_SAMPLE,
};
pub(crate) use context_delta::deltas_for_session;
pub use context_delta::{
    ContextDelta, ContextDeltaOpts, InterveningStep, OwnerFilter, OwnerRail, ReminderSource,
};
pub(crate) use cost::sum_costs;
pub use cost::{cost_for_turn, tally_unpriced, CostBreakdown};
pub use fidelity::{
    has_minimum_fidelity, summarize_fidelity, summarize_fidelity_from_iter, FidelitySummary,
};
pub(crate) use findings::findings_from_patterns;
pub use findings::{sort_findings, WasteFinding, WasteSeverity};
pub use flow_graph::{
    flow_graph_from_trees, FlowEdge, FlowEdgeKind, FlowGraph, FlowNode, FlowNodeKind, FlowOpts,
    TurnTokens, INTER_TURN_GAP, RAIL_GAP,
};
pub use ghost_surface::{
    detect_ghost_surface, ghost_surface_to_finding, GhostSurfaceFindingOptions,
};
pub use ghost_surface_inputs::build_ghost_surface_inputs;
pub(crate) use hotspots::{
    aggregate_by_bash, aggregate_by_bash_verb, aggregate_by_file, aggregate_by_mcp_server,
    aggregate_by_subagent, attribute_hotspots,
};
pub use hotspots::{
    AttributionMethod, BashAggregation, BashVerbAggregation, FileAggregation, HotspotsOptions,
    McpServerAggregation, SubagentAggregation,
};
pub(crate) use overhead::{
    attribute_overhead, find_overhead_files, load_overhead_file, AttributeOverheadInput,
    OverheadAttribution, OverheadFile, ParsedOverheadFile,
};
pub use overhead::{describe_applies_to, OverheadFileKind};
pub(crate) use patterns::detect_patterns;
pub use patterns::DetectPatternsOptions;
pub(crate) use pricing::PricingTable;
pub use pricing::{load_pricing, ModelCost, ReasoningMode};
pub(crate) use provider::{
    aggregate_by_provider, AggregateByProviderOptions, ProviderAggregateRow,
};
pub use provider::{
    provider_for, CoverageField, FieldCoverage, ProviderFilter, RowCoverage, TurnProvider,
    UsageCostAggregateRow,
};
pub(crate) use quality::{compute_quality, ComputeQualityOptions};
pub use quality::{OneShotMetrics, OutcomeLabel, QualityResult, SessionOutcome};
pub(crate) use replacement_savings::summarize_replacement_savings;
pub use replacement_savings::{ReplacementSavingsSummary, ToolSavingsAggregate};
pub use span_tree::{AttrValue, SpanEvent, SpanKind, SpanNode, SpanStatus, TurnSpanTree};
pub(crate) use subagent_tree::{
    aggregate_subagent_type_stats, build_subagent_tree, BuildSubagentTreeOptions,
};
pub use subagent_tree::{SubagentTreeNode, SubagentTypeStats};
pub(crate) use tool_call_patterns::detect_tool_call_patterns;
pub use tool_call_patterns::{tool_call_pattern_to_finding, DetectToolCallPatternsOptions};
pub(crate) use tool_output_bloat::detect_tool_output_bloat;
pub use tool_output_bloat::{
    load_claude_settings, project_claude_settings_path, tool_output_bloat_to_finding,
    user_claude_settings_path, DetectToolOutputBloatOptions, LoadedClaudeSettings,
};
