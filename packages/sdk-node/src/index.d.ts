// Type surface for `@relayburn/sdk@2.x`.
//
// Mirrors `packages/sdk/index.d.ts` (the TS 1.x SDK) byte-for-byte modulo:
//   - `bigint` is allowed alongside `number` for u64-typed token counts (the
//     napi-rs binding emits `BigInt` for `u64`; the TS shape is widened so
//     existing callers that pass through `number` keep type-checking once
//     bound to the Rust impl).
//   - Async fns return `Promise<T>` — the napi-rs binding uses `async fn`
//     where the Rust SDK does, which is everywhere except the `Ledger.open`
//     constructor.
//
// Source-of-truth comment: track `packages/sdk/index.d.ts`. Whenever a verb
// shape changes in TS, mirror it here AND in the Rust napi-rs binding (#247-a).

export interface LedgerOpenOptions { home?: string }
export declare class Ledger { static open(opts?: LedgerOpenOptions): Promise<Ledger> }

export interface IngestOptions { sessionId?: string; harness?: 'claude-code'|'codex'|'opencode'; ledgerHome?: string }
export declare function ingest(opts?: IngestOptions): Promise<unknown>

export interface SummaryOptions {
  session?: string;
  project?: string;
  /** ISO timestamp (e.g. `2026-04-01T00:00:00Z`) or relative range (`24h`, `7d`, `4w`, `2m`). */
  since?: string;
  ledgerHome?: string;
  /** Optional logger invoked when the SQLite archive read fails and the SDK falls back to a full ledger walk. */
  onLog?: (msg: string) => void;
}
export declare function summary(opts?: SummaryOptions): Promise<{
  totalTokens: number | bigint;
  totalCost: number;
  turnCount: number;
  byTool: Array<{ tool: string; tokens: number | bigint; cost: number; count: number }>;
  byModel: Array<{ model: string; tokens: number | bigint; cost: number }>;
}>

export interface SessionCostOptions {
  /** Session id to total. Omit for `{ note: 'no session id provided' }`. */
  session?: string;
  ledgerHome?: string;
  onLog?: (msg: string) => void;
}
export interface SessionCostResult {
  sessionId: string | null;
  totalUSD: number;
  totalTokens: number | bigint;
  turnCount: number;
  models: string[];
  note?: string;
}
/** Compact session-scoped cost shape; powers the MCP `burn__sessionCost` tool. */
export declare function sessionCost(opts?: SessionCostOptions): Promise<SessionCostResult>

export type OverheadFileKind = 'claude-md' | 'agents-md';
export type OverheadHarness = 'claude-code' | 'codex' | 'opencode';

export interface OverheadOptions {
  project?: string;
  since?: string;
  kind?: OverheadFileKind;
  ledgerHome?: string;
  onLog?: (msg: string) => void;
}

export interface OverheadSection {
  heading: string;
  startLine: number;
  endLine: number;
  tokens: number | bigint;
}

export interface OverheadSectionCost {
  filePath: string;
  section: OverheadSection;
  tokenShare: number;
  costPerSession: number;
  totalCost: number;
}

export interface OverheadAttributionDetail {
  sessionCount: number;
  perSessionAvg: number;
  perSessionP95: number;
  totalCost: number;
  sectionCosts: OverheadSectionCost[];
}

export interface OverheadFileSummary {
  kind: OverheadFileKind;
  path: string;
  appliesTo: OverheadHarness[];
  totalLines: number;
  bytes: number | bigint;
  tokens: number | bigint;
  sections: OverheadSection[];
  groupingLevel: number;
}

export interface OverheadPerFileEntry {
  path: string;
  kind: OverheadFileKind;
  appliesTo: OverheadHarness[];
  attribution: OverheadAttributionDetail;
}

export interface OverheadResult {
  project: string;
  files: OverheadFileSummary[];
  perFile: OverheadPerFileEntry[];
  grandTotal: number;
}

/** Per-file + per-section overhead cost attribution. Powers `burn overhead`. */
export declare function overhead(opts?: OverheadOptions): Promise<OverheadResult>

export interface OverheadTrimOptions extends OverheadOptions {
  top?: number;
  includeDiff?: boolean;
}

export interface OverheadTrimRecommendation {
  file: string;
  kind: OverheadFileKind;
  appliesTo: OverheadHarness[];
  section: { heading: string; startLine: number; endLine: number; tokens: number | bigint };
  projectedSavings: {
    perSessionUsd: number;
    acrossWindowUsd: number;
    tokens: number | bigint;
    tokenShare: number;
  };
  diff?: string;
}

export interface OverheadTrimResult {
  project: string;
  since: string;
  recommendations: OverheadTrimRecommendation[];
  summary: {
    filesAnalyzed: number;
    filesWithRecommendations: number;
    totalRecommendations: number;
    totalProjectedSavingsPerSession: number;
    totalProjectedSavingsAcrossWindow: number;
  };
}

/** Trim recommendations for high-cost overhead-file sections. Powers `burn overhead trim`. */
export declare function overheadTrim(opts?: OverheadTrimOptions): Promise<OverheadTrimResult>

export type HotspotsGroupBy = 'attribution' | 'bash' | 'bash-verb' | 'file' | 'subagent';

export interface HotspotsOptions {
  session?: string;
  project?: string;
  since?: string;
  groupBy?: HotspotsGroupBy;
  patterns?: string[];
  ledgerHome?: string;
  onLog?: (msg: string) => void;
}

export interface HotspotsFileRow {
  path: string;
  firstEmitTurnIndex: number;
  initialTokens: number | bigint;
  persistenceTokens: number | bigint;
  ridingTurns: number;
  totalCost: number;
}

export interface HotspotsBashRow {
  command: string | undefined;
  argsHash: string;
  callCount: number;
  initialTokens: number | bigint;
  persistenceTokens: number | bigint;
  totalCost: number;
}

export interface HotspotsBashVerbRow {
  verb: string;
  callCount: number;
  distinctCommands: number;
  initialTokens: number | bigint;
  persistenceTokens: number | bigint;
  avgPersistenceTurns: number;
  totalCost: number;
  topExamples: string[];
}

export interface HotspotsSubagentRow {
  subagentType: string;
  callCount: number;
  initialTokens: number | bigint;
  persistenceTokens: number | bigint;
  totalCost: number;
}

export interface HotspotsSessionTotal {
  sessionId: string;
  grandCost: number;
  attributedCost: number;
  unattributedCost: number;
  attributionMethod: 'sized' | 'even-split';
}

export interface HotspotsFidelityBlock {
  analyzed: number;
  excluded: number;
  summary: unknown;
  refused: boolean;
}

export interface HotspotsAttributionResult {
  kind: 'attribution';
  turnsAnalyzed: number;
  grandTotal: number;
  attributedTotal: number;
  unattributedTotal: number;
  attributionDegraded: boolean;
  sessions: HotspotsSessionTotal[];
  files: HotspotsFileRow[];
  bashVerbs: HotspotsBashVerbRow[];
  bash: HotspotsBashRow[];
  subagents: HotspotsSubagentRow[];
  fidelity: HotspotsFidelityBlock;
  refused?: boolean;
  refusalReason?: string;
}

export interface HotspotsBashResult { kind: 'bash'; rows: HotspotsBashRow[]; refused?: boolean; refusalReason?: string }
export interface HotspotsBashVerbResult { kind: 'bash-verb'; rows: HotspotsBashVerbRow[]; refused?: boolean; refusalReason?: string }
export interface HotspotsFileResult { kind: 'file'; rows: HotspotsFileRow[]; refused?: boolean; refusalReason?: string }
export interface HotspotsSubagentResult { kind: 'subagent'; rows: HotspotsSubagentRow[]; refused?: boolean; refusalReason?: string }

export interface HotspotsFinding {
  kind: string;
  severity: string;
  sessionId: string;
  title: string;
  estimatedSavings: { usdPerSession?: number; [k: string]: unknown };
  [k: string]: unknown;
}

export interface HotspotsFindingsResult {
  kind: 'findings';
  findings: HotspotsFinding[];
  summary: unknown;
}

export type HotspotsResult =
  | HotspotsAttributionResult
  | HotspotsBashResult
  | HotspotsBashVerbResult
  | HotspotsFileResult
  | HotspotsSubagentResult
  | HotspotsFindingsResult;

/**
 * Per-axis hotspot attribution + pattern-finding queries. Returns a
 * discriminated union — see `HotspotsResult`.
 */
export declare function hotspots(opts?: HotspotsOptions): Promise<HotspotsResult>

export type FidelityClass = 'full' | 'usage-only' | 'aggregate-only' | 'cost-only' | 'partial';

export interface FidelitySummaryShape {
  total: number;
  byClass: Record<FidelityClass, number>;
  unknown: number;
  missingCoverage: Record<string, number>;
}

export interface CompareExcludedBreakdown {
  total: number;
  aggregateOnly: number;
  costOnly: number;
  partial: number;
  usageOnly: number;
}

export interface CompareCellResult {
  model: string;
  category: string;
  turns: number;
  editTurns: number;
  oneShotTurns: number;
  pricedTurns: number;
  totalCost: number;
  costPerTurn: number | null;
  oneShotRate: number | null;
  cacheHitRate: number | null;
  medianRetries: number | null;
  noData: boolean;
  insufficientSample: boolean;
}

export interface CompareOptions {
  models: string[];
  session?: string;
  project?: string;
  since?: string;
  workflow?: string;
  agent?: string;
  provider?: string[];
  minSample?: number;
  minFidelity?: FidelityClass;
  ledgerHome?: string;
  onLog?: (msg: string) => void;
}

export interface CompareResult {
  analyzedTurns: number;
  minSample: number;
  models: string[];
  categories: string[];
  totals: Record<string, { turns: number; totalCost: number }>;
  cells: CompareCellResult[];
  fidelity: {
    minimum: FidelityClass;
    excluded: CompareExcludedBreakdown;
    summary: FidelitySummaryShape;
  };
}

/** Per-(model, activity) comparison shape. Powers `burn compare`. */
export declare function compare(opts: CompareOptions): Promise<CompareResult>
