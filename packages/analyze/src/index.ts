export { flatten, loadBuiltinPricing, loadPricing } from './pricing.js';
export type { ModelCost, PricingTable, ReasoningMode } from './pricing.js';
export { costForTurn, costForUsage, sumCosts } from './cost.js';
export type { CostBreakdown, CostForUsageOptions } from './cost.js';
export { buildCompareTable, DEFAULT_MIN_SAMPLE } from './compare.js';
export type { CompareCategory, CompareCell, CompareOptions, CompareTable } from './compare.js';
export { compareFromArchive } from './compare-archive.js';
export type { CompareFromArchiveResult } from './compare-archive.js';
export {
  attributeHotspots,
  aggregateByFile,
  aggregateByBash,
  aggregateByBashVerb,
  aggregateBySubagent,
} from './hotspots.js';
export type {
  AttributionMethod,
  HotspotsOptions,
  BashAggregation,
  BashVerbAggregation,
  FileAggregation,
  SessionTotals,
  SubagentAggregation,
  ToolAttribution,
  HotspotsResult,
} from './hotspots.js';
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
  CancellationRun,
  CompactionLoss,
  DetectPatternsOptions,
  EditHeavySession,
  EditRevertCycle,
  FailureRun,
  PatternEventSource,
  PatternsResult,
  RetryLoop,
  SessionPatternSummary,
  SkillPruningProtection,
  SkillRecallDup,
  SystemPromptTax,
} from './patterns.js';
export {
  cancellationRunToFinding,
  compactionLossToFinding,
  editHeavyToFinding,
  editRevertToFinding,
  failureRunToFinding,
  findingsFromPatterns,
  retryLoopToFinding,
  skillPruningProtectionToFinding,
  skillRecallDupToFinding,
  sortFindings,
  systemPromptTaxToFinding,
} from './findings.js';
export type {
  EstimatedSavings,
  WasteAction,
  WasteFinding,
  WasteSeverity,
} from './findings.js';
export {
  BASH_MAX_OUTPUT_ENV_KEY,
  DEFAULT_BLOAT_TOKEN_THRESHOLD,
  detectObservedBloat,
  detectStaticConfigBloat,
  detectToolOutputBloat,
  loadClaudeSettings,
  projectClaudeSettingsPath,
  toolOutputBloatToFinding,
  userClaudeSettingsPath,
} from './tool-output-bloat.js';
export type {
  ClaudeSettings,
  DetectObservedBloatOptions,
  DetectStaticConfigBloatOptions,
  DetectToolOutputBloatOptions,
  LoadedClaudeSettings,
  ToolOutputBloat,
} from './tool-output-bloat.js';
export {
  detectToolCallPatterns,
  toolCallPatternToFinding,
} from './tool-call-patterns.js';
export type {
  DetectToolCallPatternsOptions,
  ToolCallPatternCategory,
  ToolCallPatternFinding,
} from './tool-call-patterns.js';
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
  buildTrimRecommendations,
  findClaudeMdFiles,
  loadClaudeMdFile,
  parseClaudeMd,
  renderUnifiedDiffForRecommendation,
} from './claude-md.js';
export type {
  TrimRecommendation,
  AttributeClaudeMdInput,
  ClaudeMdAttributionResult,
  MarkdownSection,
  ParsedClaudeMd,
  SectionCost,
  SessionClaudeMdCost,
} from './claude-md.js';
export {
  attributeOverhead,
  describeAppliesTo,
  findOverheadFiles,
  loadOverheadFile,
} from './overhead.js';
export {
  emptyFidelitySummary,
  hasMinimumFidelity,
  summarizeFidelity,
} from './fidelity.js';
export type { FidelitySummary } from './fidelity.js';
export { DEFAULT_RULES, resolveProvider } from './provider-reattribution.js';
export type { ProviderResolution, ProviderRule } from './provider-reattribution.js';
export {
  aggregateByProvider,
  filterTurnsByProvider,
  providerFor,
  providerForModel,
  providerForTurn,
  resolveTurnProvider,
} from './provider.js';
export type {
  AggregateByProviderOptions,
  CoverageField,
  FieldCoverage,
  ProviderAggregateRow,
  ProviderFilter,
  RowCoverage,
  TurnProvider,
  UsageCostAggregateRow,
} from './provider.js';
export type {
  AttributeOverheadInput,
  OverheadAttribution,
  OverheadFile,
  OverheadFileAttribution,
  OverheadFileKind,
  ParsedOverheadFile,
} from './overhead.js';
export {
  claudeGhostAdapter,
  codexGhostAdapter,
  DEFAULT_GHOST_ADAPTERS,
  detectGhostSurface,
  ghostFindingsToWasteFindings,
  ghostSurfaceToFinding,
  opencodeGhostAdapter,
} from './ghost-surface.js';
export {
  buildGhostSurfaceInputs,
  buildObservedNamesBySource,
  buildSessionCountBySource,
  pickRepresentativeCacheReadRate,
} from './ghost-surface-inputs.js';
export type {
  DetectGhostSurfaceOptions,
  GhostCandidate,
  GhostFindingKind,
  GhostSurfaceAdapter,
  GhostSurfaceFinding,
  GhostSurfaceFindingOptions,
  GhostSurfaceInputs,
} from './ghost-surface.js';
