export { flatten, loadBuiltinPricing, loadPricing } from './pricing.js';
export type { ModelCost, PricingTable, ReasoningMode } from './pricing.js';
export { costForTurn, costForUsage, sumCosts } from './cost.js';
export type { CostBreakdown, CostForUsageOptions } from './cost.js';
export { buildCompareTable, DEFAULT_MIN_SAMPLE } from './compare.js';
export type { CompareCategory, CompareCell, CompareOptions, CompareTable } from './compare.js';
export { compareFromArchive } from './compare-archive.js';
export type { CompareFromArchiveResult } from './compare-archive.js';
export {
  attributeWaste,
  aggregateByFile,
  aggregateByBash,
  aggregateBySubagent,
} from './waste.js';
export type {
  AttributionMethod,
  AttributeWasteOptions,
  BashAggregation,
  FileAggregation,
  SessionWasteTotals,
  SubagentAggregation,
  ToolAttribution,
  WasteResult,
} from './waste.js';
export {
  aggregateSubagentTypeStats,
  buildSubagentTree,
} from './subagent-tree.js';
export type {
  BuildSubagentTreeOptions,
  SubagentTreeNode,
  SubagentTypeStats,
} from './subagent-tree.js';
export { detectPatterns } from './patterns.js';
export type {
  CompactionLoss,
  DetectPatternsOptions,
  EditRevertCycle,
  FailureRun,
  PatternsResult,
  RetryLoop,
  SessionPatternSummary,
  SkillPruningProtection,
  SkillRecallDup,
  SystemPromptTax,
} from './patterns.js';
export { computeQuality, computeOneShotRate, inferOutcome } from './quality.js';
export type {
  ComputeQualityOptions,
  OneShotMetrics,
  OutcomeConfidence,
  OutcomeLabel,
  QualityResult,
  SessionOutcome,
} from './quality.js';
export {
  attributeClaudeMd,
  buildAdviseRecommendations,
  findClaudeMdFiles,
  loadClaudeMdFile,
  parseClaudeMd,
  renderUnifiedDiffForRecommendation,
} from './claude-md.js';
export type {
  AdviseRecommendation,
  AttributeClaudeMdInput,
  ClaudeMdAttributionResult,
  MarkdownSection,
  ParsedClaudeMd,
  SectionCost,
  SessionClaudeMdCost,
} from './claude-md.js';
export {
  attributeContext,
  describeAppliesTo,
  findContextFiles,
  loadContextFile,
} from './context-md.js';
export { computePlanUsage, cycleBounds, planUsageFromArchive } from './plan-usage.js';
export type {
  ComputePlanUsageFromArchiveOptions,
  ComputePlanUsageOptions,
  PlanUsage,
  PlanUsageFidelity,
} from './plan-usage.js';
export {
  emptyFidelitySummary,
  hasMinimumFidelity,
  summarizeFidelity,
} from './fidelity.js';
export type { FidelitySummary } from './fidelity.js';
export { DEFAULT_RULES, resolveProvider } from './provider-reattribution.js';
export type { ProviderResolution, ProviderRule } from './provider-reattribution.js';
export type {
  AttributeContextInput,
  ContextAttributionResult,
  ContextFile,
  ContextFileAttribution,
  ContextFileKind,
  ParsedContextFile,
} from './context-md.js';
