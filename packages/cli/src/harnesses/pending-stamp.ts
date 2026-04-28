import * as path from 'node:path';

import type { IngestReport } from '../ingest.js';
import { writePendingStamp } from '../pending-stamps.js';
import { startWatchLoop, type WatchController } from '../commands/watch.js';

import type { HarnessAdapter, HarnessRunContext, HarnessSpawnPlan } from './types.js';

// The codex and opencode adapters are structurally identical: pre-spawn pending
// stamp, while-running watch loop draining the session store, post-exit ingest
// pass. Differences are the binary name, the session-store path, and the final
// ingest function. Capture that shape once.
export interface PendingStampAdapterOptions {
  name: 'codex' | 'opencode';
  sessionRoot: () => string;
  ingestSessions: () => Promise<IngestReport>;
}

export function createPendingStampAdapter(opts: PendingStampAdapterOptions): HarnessAdapter {
  return {
    name: opts.name,
    sessionRoot: opts.sessionRoot,
    async plan(ctx: HarnessRunContext): Promise<HarnessSpawnPlan> {
      return { binary: opts.name, args: [...ctx.passthrough] };
    },
    async beforeSpawn(ctx: HarnessRunContext): Promise<void> {
      const pending = await writePendingStamp({
        harness: opts.name,
        cwd: ctx.cwd,
        enrichment: ctx.tags,
        sessionDirHint: opts.sessionRoot(),
        spawnStartTs: ctx.spawnStartTs,
      });
      process.stderr.write(
        `[burn] ${opts.name} spawn: pending stamp ${path.basename(pending.file)}\n`,
      );
    },
    startWatcher(_ctx, onReport): WatchController {
      // The watch loop drains turns silently while the child runs; accumulate
      // its ticks so the final ingest report reflects everything appended
      // during the session, not just the leftovers the post-exit pass picks up
      // (#125 review).
      return startWatchLoop({ immediate: false, onReport });
    },
    async afterExit(): Promise<IngestReport> {
      return opts.ingestSessions();
    },
  };
}
