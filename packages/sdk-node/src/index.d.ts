// Type surface for `@relayburn/sdk@2.x`.
//
// Mirrors the Rust SDK verb surface through the napi-rs facade, with
// compatibility affordances for callers migrating from the 1.x JS SDK:
//   - `bigint` is allowed alongside `number` for u64-typed token counts (the
//     napi-rs binding emits `BigInt` for `u64`; the facade downcasts safe-range
//     values at runtime).
//   - Async fns return `Promise<T>` — the napi-rs binding uses `async fn`
//     where the Rust SDK does, which is everywhere except the `Ledger.open`
//     constructor.

export interface LedgerOpenOptions { home?: string; contentHome?: string }
/**
 * Stateful ledger handle. The Node facade exposes the static `open()`
 * constructor; instances carry the resolved home for callers that want
 * to confirm which ledger they attached to. Verb methods are reserved
 * for a future facade expansion.
 */
export declare class Ledger {
  readonly home: string;
  static open(opts?: LedgerOpenOptions): Promise<Ledger>;
}

export interface IngestOptions { sessionId?: string; harness?: 'claude-code'|'codex'|'opencode'; ledgerHome?: string }
export interface IngestReport {
  scannedSessions: number | bigint;
  ingestedSessions: number | bigint;
  appendedTurns: number | bigint;
  appliedPendingStamps: number | bigint;
}
export declare function ingest(opts?: IngestOptions): Promise<IngestReport>

export type PendingStampHarness = 'claude' | 'codex' | 'opencode';
export interface WritePendingStampOptions {
  harness: PendingStampHarness;
  cwd: string;
  enrichment: Record<string, string>;
  sessionDirHint?: string;
  /** ISO timestamp, e.g. `2026-04-23T00:00:00.000Z`. Defaults to now. */
  spawnStartTs?: string;
  spawnerPid?: number;
  ledgerHome?: string;
}
export interface PendingStamp {
  v: number;
  harness: PendingStampHarness;
  spawnerPid: number;
  spawnStartTs: string;
  cwd: string;
  enrichment: Record<string, string>;
  sessionDirHint?: string;
}
export interface PendingStampWriteResult {
  file: string;
  stamp: PendingStamp;
}
export declare function writePendingStamp(
  opts: WritePendingStampOptions,
): Promise<PendingStampWriteResult>

export interface WriteStampOptions {
  /** Target a session by exact id. At least one of `sessionId` or `messageId` must be set. */
  sessionId?: string;
  /** Target a single turn by exact message id. At least one of `sessionId` or `messageId` must be set. */
  messageId?: string;
  /** Enrichment key/value pairs to fold onto matched turns. Must be non-empty. */
  enrichment: Record<string, string>;
  /** ISO timestamp the caller observed, e.g. `2026-05-21T12:00:00Z`. Defaults to now when omitted. */
  ts?: string;
  ledgerHome?: string;
}

/**
 * Write a stamp targeting an exact session id or message id. Use when the
 * launcher knows the session id up front — for example, a Claude launcher
 * that preallocates `--session-id <uuid>` before spawn — so the
 * enrichment lands by selector without going through the sidecar
 * `writePendingStamp` manifest matching path.
 */
export declare function writeStamp(opts: WriteStampOptions): Promise<void>

export interface SummaryOptions {
  session?: string;
  project?: string;
  /** ISO timestamp (e.g. `2026-04-01T00:00:00Z`) or relative range (`24h`, `7d`, `4w`, `2m`). */
  since?: string;
  /** Folded enrichment tag filters; every key/value pair must match. */
  tags?: Record<string, string>;
  /** Group summary costs/tokens by this folded enrichment tag key. */
  groupByTag?: string;
  ledgerHome?: string;
}
export declare function summary(opts?: SummaryOptions): Promise<{
  totalTokens: number | bigint;
  totalCost: number;
  turnCount: number;
  byTool: Array<{ tool: string; tokens: number | bigint; cost: number; count: number }>;
  byModel: Array<{ model: string; tokens: number | bigint; cost: number }>;
  byTag?: Array<{
    tag: string;
    value?: string;
    tokens: number | bigint;
    cost: number;
    turnCount: number | bigint;
  }>;
  replacementSavings?: {
    calls: number | bigint;
    collapsedCalls: number | bigint;
    estimatedTokensSaved: number | bigint;
    byTool: Array<{
      tool: string;
      calls: number | bigint;
      collapsedCalls: number | bigint;
      estimatedTokensSaved: number | bigint;
    }>;
  };
}>

export interface SessionCostOptions {
  /** Session id to total. Omit for `{ note: 'no session id provided' }`. */
  session?: string;
  ledgerHome?: string;
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
  /** Restrict to turns folded under a `workflowId` enrichment stamp. */
  workflow?: string;
  /** Provider allow-list (case-insensitive). */
  provider?: string[];
  ledgerHome?: string;
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

export interface HotspotsMcpServerRow {
  /** MCP server name (the `<server>` segment of `mcp__<server>__<tool>`). */
  server: string;
  callCount: number;
  initialTokens: number | bigint;
  persistenceTokens: number | bigint;
  ridingTurns: number;
  totalCost: number;
  /** Up to three representative tool basenames (cost desc, then name asc). */
  topTools: string[];
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
  mcpServers: HotspotsMcpServerRow[];
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
export declare function computeCompareExcluded(
  summary: FidelitySummaryShape,
  minimum: FidelityClass
): CompareExcludedBreakdown

// ---------------------------------------------------------------------------
// 2.x extensions — surfaces present in `relayburn-sdk` (the Rust crate)
// but not in the TS 1.x `packages/sdk/index.d.ts`. Pre-1.0 widening per
// the SDK shape rule; embedders that pinned to 1.x won't see these names.
// The 1.x `onLog` callback is intentionally omitted: it surfaced the
// archive-fallback path that no longer exists in the SQLite-native 2.x
// stack (see issue #374), so there is nothing to log.
// ---------------------------------------------------------------------------

export interface SearchQueryOptions {
  /** FTS5 query string. Phrase, boolean, and prefix syntax supported. */
  query: string;
  /** Hit cap. Defaults to 25 when omitted. */
  limit?: number | bigint;
  /** Restrict to a single session_id. Omit to search all sessions. */
  sessionId?: string;
  ledgerHome?: string;
}

export interface SearchHit {
  sessionId: string;
  messageId: string;
  source: string;
  /** FTS5 BM25 rank (lower = better match). */
  rank: number;
  /** `<b>…</b>`-highlighted snippet around the matching tokens. */
  snippet: string;
}

export interface SearchResult {
  query: string;
  hits: SearchHit[];
}

/** FTS5-backed message-content search. 2.x extension over the TS surface. */
export declare function search(opts: SearchQueryOptions): Promise<SearchResult>

export interface ExportLedgerOptions { ledgerHome?: string }
export interface ExportStampsOptions { ledgerHome?: string }

/**
 * Stream every event row as a JSONL-shaped JSON object. Each value has
 * the form `{ v: 1, kind: '<kind>', record: <json> }`. 2.x extension.
 */
export declare function exportLedger(opts?: ExportLedgerOptions): Promise<unknown[]>

/** Stream every stamp row as a JSONL-shaped JSON object. 2.x extension. */
export declare function exportStamps(opts?: ExportStampsOptions): Promise<unknown[]>

/**
 * Tagged error code surfaced on the thrown JS `Error.code` property.
 * Sync verbs reject with one of these; the async `ingest` verb's
 * rejection currently sets `code: 'GenericFailure'` — see the binding
 * crate's `ingest` doc comment for the napi-rs-2.x rationale.
 *
 * The values are the literal strings written to `e.code`, matching the
 * Rust constants `SDK_ERROR_CODE` / `IO_ERROR_CODE` / `INVALID_ARGUMENT_ERROR_CODE`.
 */
export declare const BurnErrorCode: {
  readonly Sdk: 'BURN_SDK';
  readonly Io: 'BURN_IO';
  readonly InvalidArgument: 'BURN_INVALID_ARGUMENT';
};
export type BurnErrorCodeValue = (typeof BurnErrorCode)[keyof typeof BurnErrorCode];

/** Wire-value enum for `OverheadOptions.kind` and `OverheadTrimOptions.kind`. */
export declare const OverheadFileKind: {
  readonly ClaudeMd: 'claude-md';
  readonly AgentsMd: 'agents-md';
};

/** Wire-value enum for `HotspotsOptions.groupBy`. */
export declare const HotspotsGroupBy: {
  readonly Attribution: 'attribution';
  readonly Bash: 'bash';
  readonly BashVerb: 'bash-verb';
  readonly File: 'file';
  readonly Subagent: 'subagent';
};
