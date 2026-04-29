export {
  appendCompactions,
  appendRelationships,
  appendToolResultEvents,
  appendTurns,
  appendUserTurns,
  stamp,
} from './writer.js';
export {
  query,
  queryAll,
  queryCompactions,
  queryRelationships,
  queryToolResultEvents,
  queryUserTurns,
} from './reader.js';
export type { Query, EnrichedTurn } from './reader.js';
export {
  ledgerHome,
  ledgerPath,
  hwmPath,
  cursorsPath,
  ledgerIndexPath,
  ledgerContentIndexPath,
  lockPath,
  pricingOverridePath,
  configPath,
  plansPath,
  archivePath,
  contentDir,
  contentFilePath,
  isValidSessionId,
} from './paths.js';
export {
  ARCHIVE_VERSION,
  buildArchive,
  getArchiveStatus,
  openArchive,
  rebuildArchive,
  vacuumArchive,
} from './archive.js';
export type { ArchiveStatus, BuildResult, VacuumResult } from './archive.js';
export {
  archiveAvailable,
  queryAllFromArchive,
  queryTurnsFromArchive,
} from './archive-query.js';
export {
  appendContent,
  listContentSessionIds,
  pruneContent,
  readContent,
} from './content.js';
export type { PruneOptions, PruneResult, ReadContentSelector } from './content.js';
export { loadConfig, retentionMs, DEFAULT_CONFIG } from './config.js';
export type { BurnConfig, ContentConfig } from './config.js';
export type {
  CompactionLine,
  Enrichment,
  LedgerLine,
  MessageIdRange,
  SessionRelationshipLine,
  StampLine,
  StampSelector,
  ToolResultEventLine,
  TurnLine,
  UserTurnLine,
} from './schema.js';
export {
  isCompactionLine,
  isSessionRelationshipLine,
  isStampLine,
  isToolResultEventLine,
  isTurnLine,
  isUserTurnLine,
  stampMatches,
} from './schema.js';
export { loadHwm, saveHwm } from './hwm.js';
export type { HwmEntry, HwmMap } from './hwm.js';
export { loadCursors, saveCursors, updateCursors } from './cursors.js';
export type {
  FileCursor,
  ClaudeCursor,
  CodexCursor,
  OpencodeCursor,
  OpencodeStreamCursor,
} from './cursors.js';
export { withLock } from './lock.js';
export {
  rebuildIndex,
  relationshipIdHash,
  toolResultEventIdHash,
  turnIdHash,
  turnContentFingerprint,
  userTurnIdHash,
  // Exposed (underscore-prefixed) so cross-package tests that swap
  // RELAYBURN_HOME between cases can drop the in-memory dedup cache. Not
  // part of the supported runtime surface — see `index-sidecar.ts`.
  __resetIndexCacheForTesting,
} from './index-sidecar.js';
export { reclassifyLedger } from './reclassify.js';
export type { ReclassifyOptions, ReclassifyReport } from './reclassify.js';
export { buildClaudeHookSettings } from './hook-settings.js';
export type {
  BuildClaudeHookSettingsOptions,
  ClaudeHookSettingsResult,
} from './hook-settings.js';
export {
  BUILTIN_PRESETS,
  findPreset,
  loadPlans,
  normalizePlan,
  savePlans,
} from './plans.js';
export type { Plan, PlanPreset, PlanProvider, PlansFile } from './plans.js';
