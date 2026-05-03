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

export interface HotspotsOptions {
  session?: string;
  /**
   * Pattern kinds to detect. Supported kinds:
   *   - core (via `detectPatterns`): `retry-loop`, `failure-run`,
   *     `cancellation-run`, `compaction-loss`, `edit-revert`, `edit-heavy`,
   *     `skill-recall-dup`, `skill-pruning-protection`, `system-prompt-tax`
   *   - side-channel: `tool-output-bloat`, `ghost-surface`, `tool-call-pattern`
   *
   * When omitted or empty, returns the attribution result instead of a
   * findings array.
   */
  patterns?: string[];
  ledgerHome?: string;
}
export declare function hotspots(opts?: HotspotsOptions): Promise<unknown>
