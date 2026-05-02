export interface LedgerOpenOptions { home?: string }
export declare class Ledger { static open(opts?: LedgerOpenOptions): Promise<Ledger> }

export interface IngestOptions { sessionId?: string; harness?: 'claude-code'|'codex'|'opencode'; ledgerHome?: string }
export declare function ingest(opts?: IngestOptions): Promise<unknown>

export interface SummaryOptions { session?: string; project?: string; since?: string; ledgerHome?: string }
export declare function summary(opts?: SummaryOptions): Promise<{ totalTokens: number; totalCost: number; byTool: Array<{tool:string;tokens:number;cost:number;count:number}>; byModel: Array<{model:string;tokens:number;cost:number}> }>

export interface HotspotsOptions { session?: string; patterns?: string[]; ledgerHome?: string }
export declare function hotspots(opts?: HotspotsOptions): Promise<unknown>
