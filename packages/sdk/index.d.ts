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
  totalTokens: number;
  totalCost: number;
  turnCount: number;
  byTool: Array<{ tool: string; tokens: number; cost: number; count: number }>;
  byModel: Array<{ model: string; tokens: number; cost: number }>;
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
  totalTokens: number;
  turnCount: number;
  models: string[];
  note?: string;
}
/** Compact session-scoped cost shape; powers the MCP `burn__sessionCost` tool. */
export declare function sessionCost(opts?: SessionCostOptions): Promise<SessionCostResult>

export type OverheadFileKind = 'claude-md' | 'agents-md';
export type OverheadHarness = 'claude-code' | 'codex' | 'opencode';

export interface OverheadOptions {
  /** Project path to inspect; defaults to process.cwd(). */
  project?: string;
  /** ISO timestamp or relative range (`24h`, `7d`, `4w`, `2m`); the SDK normalizes both forms before querying. */
  since?: string;
  /** Narrow to a single overhead file kind. */
  kind?: OverheadFileKind;
  ledgerHome?: string;
  onLog?: (msg: string) => void;
}

export interface OverheadSection {
  heading: string;
  startLine: number;
  endLine: number;
  tokens: number;
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
  bytes: number;
  tokens: number;
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
  /** Recommendations per file. Default 3. */
  top?: number;
  /** Include the unified-diff text per recommendation (requires a file read per recommended file). Default true; pass false to skip. */
  includeDiff?: boolean;
}

export interface OverheadTrimRecommendation {
  file: string;
  kind: OverheadFileKind;
  appliesTo: OverheadHarness[];
  section: { heading: string; startLine: number; endLine: number; tokens: number };
  projectedSavings: {
    perSessionUsd: number;
    acrossWindowUsd: number;
    tokens: number;
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
  /** ISO timestamp (e.g. `2026-04-01T00:00:00Z`) or relative range (`24h`, `7d`, `4w`, `2m`). */
  since?: string;
  /**
   * Narrow the attribution result to a single aggregation axis. When omitted
   * (or `'attribution'`), the full attribution shape is returned. Ignored
   * when `patterns` is set — patterns always returns the `findings` shape.
   */
  groupBy?: HotspotsGroupBy;
  /**
   * Pattern kinds to detect. Supported kinds:
   *   - core (via `detectPatterns`): `retry-loop`, `failure-run`,
   *     `cancellation-run`, `compaction-loss`, `edit-revert`, `edit-heavy`,
   *     `skill-recall-dup`, `skill-pruning-protection`, `system-prompt-tax`
   *   - side-channel: `tool-output-bloat`, `ghost-surface`, `tool-call-pattern`
   *
   * When omitted or empty, returns the attribution result instead of the
   * findings shape.
   */
  patterns?: string[];
  ledgerHome?: string;
  /** Optional logger invoked when the SQLite archive read fails and the SDK falls back to a full ledger walk. */
  onLog?: (msg: string) => void;
}

/** Per-axis aggregation row (file). */
export interface HotspotsFileRow {
  path: string;
  firstEmitTurnIndex: number;
  initialTokens: number;
  persistenceTokens: number;
  ridingTurns: number;
  totalCost: number;
}

/** Per-axis aggregation row (bash, exact command). */
export interface HotspotsBashRow {
  command: string | undefined;
  argsHash: string;
  callCount: number;
  initialTokens: number;
  persistenceTokens: number;
  totalCost: number;
}

/** Per-axis aggregation row (bash, by leading verb). */
export interface HotspotsBashVerbRow {
  verb: string;
  callCount: number;
  distinctCommands: number;
  initialTokens: number;
  persistenceTokens: number;
  avgPersistenceTurns: number;
  totalCost: number;
  topExamples: string[];
}

/** Per-axis aggregation row (subagent / Agent / Task). */
export interface HotspotsSubagentRow {
  subagentType: string;
  callCount: number;
  initialTokens: number;
  persistenceTokens: number;
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
  /** Aggregate fidelity summary for the matched-window turns (analyzed + excluded). */
  summary: unknown;
  refused: boolean;
}

/** Full attribution shape — mirrors the CLI's `burn hotspots --json`. */
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
  /** Set when every matched turn lacked the coverage attribution needs. */
  refused?: boolean;
  refusalReason?: string;
}

/** Narrowed shapes — one aggregation axis only. */
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
  /** Aggregate fidelity summary for the matched-window turns. */
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
  /** Required: ≥2 model names to compare. */
  models: string[];
  session?: string;
  project?: string;
  /** ISO timestamp (e.g. `2026-04-01T00:00:00Z`) or relative range (`24h`, `7d`, `4w`, `2m`). */
  since?: string;
  workflow?: string;
  agent?: string;
  /** Resolved provider filter (e.g. `['anthropic', 'synthetic']`). */
  provider?: string[];
  /** Insufficient-sample threshold; cells below this get flagged. Default 5. */
  minSample?: number;
  /** Minimum fidelity class to include in the aggregate. Default `'usage-only'`. */
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

/**
 * Per-(model, activity) comparison shape. Powers `burn compare` and the
 * future `burn__compare` MCP tool. Reads through the SQLite archive when
 * `minFidelity === 'partial'` and no provider filter is set; otherwise
 * walks the ledger so the fidelity gate / provider filter can be applied
 * per-turn. Falls back transparently to the ledger walk when the archive
 * read fails.
 */
export declare function compare(opts: CompareOptions): Promise<CompareResult>
