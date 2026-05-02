export interface LedgerOpenOptions { home?: string }
export declare class Ledger { static open(opts?: LedgerOpenOptions): Promise<Ledger> }

export interface IngestOptions { sessionId?: string; harness?: 'claude-code'|'codex'|'opencode'; ledgerHome?: string }
export declare function ingest(opts?: IngestOptions): Promise<unknown>

export interface SummaryOptions { session?: string; project?: string; since?: string; ledgerHome?: string }
export declare function summary(opts?: SummaryOptions): Promise<{ totalTokens: number; totalCost: number; byTool: Array<{tool:string;tokens:number;cost:number;count:number}>; byModel: Array<{model:string;tokens:number;cost:number}> }>

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
