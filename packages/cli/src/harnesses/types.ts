import type { Enrichment } from '@relayburn/ledger';

import type { IngestReport } from '../ingest.js';
import type { WatchController } from '../watch-loop.js';

export interface HarnessRunContext {
  cwd: string;
  passthrough: string[];
  tags: Enrichment;
  spawnStartTs: Date;
}

export interface HarnessSpawnPlan {
  binary: string;
  args: string[];
  envOverrides?: Record<string, string>;
  sessionId?: string;
}

export interface HarnessAdapter {
  readonly name: string;
  readonly sessionRoot: () => string;

  plan(ctx: HarnessRunContext): Promise<HarnessSpawnPlan>;

  beforeSpawn(ctx: HarnessRunContext, plan: HarnessSpawnPlan): Promise<void>;

  // Adapters that ingest from a pre-known session file (claude) return null.
  // Adapters that drain a session-store directory while the child runs
  // (codex, opencode) return a controller; the driver stops it on child exit.
  startWatcher?(
    ctx: HarnessRunContext,
    onReport: (report: IngestReport) => void,
  ): WatchController | null;

  afterExit(ctx: HarnessRunContext, plan: HarnessSpawnPlan): Promise<IngestReport>;
}
