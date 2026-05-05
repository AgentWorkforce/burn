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

pub mod claude_md;
pub mod compare;
pub mod compare_archive;
pub mod cost;
pub mod fidelity;
pub mod findings;
pub mod ghost_surface;
pub mod ghost_surface_inputs;
pub mod overhead;
pub mod pricing;
pub mod provider;
pub mod provider_reattribution;
pub mod quality;
pub mod replacement_savings;
pub mod subagent_tree;
pub mod tool_call_patterns;
pub mod tool_output_bloat;

pub use claude_md::{
    attribute_claude_md, build_trim_recommendations, find_claude_md_files, load_claude_md_file,
    parse_claude_md, render_unified_diff_for_recommendation, AttributeClaudeMdInput,
    ClaudeMdAttributionResult, MarkdownSection, ParsedClaudeMd, SectionCost, SessionClaudeMdCost,
    TrimRecommendation,
};
pub use compare::{
    build_compare_table, CompareCategory, CompareCell, CompareOptions, CompareTable, CompareTotals,
    DEFAULT_MIN_SAMPLE,
};
pub use compare_archive::{compare_from_archive, CompareFromArchiveResult};
pub use cost::{
    cost_for_turn, cost_for_usage, lookup_model_rate, sum_costs, CostBreakdown, CostForUsageOptions,
};
pub use overhead::{
    attribute_overhead, describe_applies_to, find_overhead_files, load_overhead_file,
    AttributeOverheadInput, OverheadAttribution, OverheadFile, OverheadFileAttribution,
    OverheadFileKind, ParsedOverheadFile,
};
pub use fidelity::{
    empty_fidelity_summary, has_minimum_fidelity, summarize_fidelity, summarize_fidelity_from_iter,
    FidelitySummary, COVERAGE_FIELDS,
};
pub use findings::{
    cancellation_run_to_finding, compaction_loss_to_finding, edit_heavy_to_finding,
    edit_revert_to_finding, failure_run_to_finding, findings_from_patterns, retry_loop_to_finding,
    skill_pruning_protection_to_finding, skill_recall_dup_to_finding, sort_findings,
    system_prompt_tax_to_finding, CancellationRun, CompactionLoss, CompactionLostWork,
    EditHeavySession, EditPreview, EditRevertCycle, EditRevertSamplePreview, EstimatedSavings,
    FailureRun, FailureRunErrorSignature, PatternEventSource, PatternsResult, RetryLoop,
    SessionPatternSummary, SkillPruningProtection, SkillRecallDup, SystemPromptTax, WasteAction,
    WasteFinding, WasteSeverity,
};
pub use ghost_surface::{
    default_ghost_adapters, detect_ghost_surface, detect_ghost_surface_with_adapters,
    ghost_findings_to_waste_findings, ghost_surface_to_finding, mine_claude_command_names,
    mine_codex_slash_invocations, ClaudeGhostAdapter, CodexGhostAdapter, DetectGhostSurfaceOptions,
    GhostCandidate, GhostFindingKind, GhostSurfaceAdapter, GhostSurfaceFinding,
    GhostSurfaceFindingOptions, GhostSurfaceInputs, OpenCodeGhostAdapter,
};
pub use ghost_surface_inputs::{
    build_ghost_surface_inputs, build_observed_names_by_source, build_session_count_by_source,
    pick_representative_cache_read_rate,
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
pub use quality::{
    compute_one_shot_rate, compute_quality, infer_outcome, ComputeQualityOptions, OneShotMetrics,
    OutcomeConfidence, OutcomeLabel, OutcomeReason, QualityResult, SessionOutcome,
};
pub use replacement_savings::{
    estimate_savings_for_tool_call, summarize_replacement_savings, ReplacementSavingsOptions,
    ReplacementSavingsSummary, ToolCallSavings, ToolSavingsAggregate,
    DEFAULT_FALLBACK_TOKEN_COST, DEFAULT_REPLACED_TOOL_TOKEN_COST,
};
pub use subagent_tree::{
    aggregate_subagent_type_stats, build_subagent_tree, BuildSubagentTreeOptions, SubagentTreeNode,
    SubagentTypeStats,
};
pub use tool_call_patterns::{
    detect_tool_call_patterns, tool_call_pattern_to_finding, DetectToolCallPatternsOptions,
    ToolCallPatternCategory, ToolCallPatternFinding,
};
pub use tool_output_bloat::{
    detect_observed_bloat, detect_static_config_bloat, detect_tool_output_bloat,
    load_claude_settings, project_claude_settings_path, tool_output_bloat_to_finding,
    user_claude_settings_path, ClaudeSettings, DetectObservedBloatOptions,
    DetectStaticConfigBloatOptions, DetectToolOutputBloatOptions, LoadedClaudeSettings,
    ToolOutputBloat, ToolOutputBloatKind, BASH_MAX_OUTPUT_ENV_KEY, DEFAULT_BLOAT_TOKEN_THRESHOLD,
};
