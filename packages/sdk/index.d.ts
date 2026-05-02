export interface LedgerOpenOptions { home?: string }
export declare class Ledger { static open(opts?: LedgerOpenOptions): Promise<Ledger> }

export interface IngestOptions { sessionId?: string; harness?: 'claude-code'|'codex'|'opencode'; ledgerHome?: string }
export declare function ingest(opts?: IngestOptions): Promise<unknown>

export interface SummaryOptions {
  session?: string;
  project?: string;
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
