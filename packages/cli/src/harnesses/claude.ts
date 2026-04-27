import { randomUUID } from 'node:crypto';
import { homedir } from 'node:os';
import * as path from 'node:path';

import { stamp } from '@relayburn/ledger';

import { ingestSession } from '../commands/claude.js';
import type { IngestReport } from '../ingest.js';

import type { HarnessAdapter, HarnessRunContext, HarnessSpawnPlan } from './types.js';

function claudeProjectsRoot(): string {
  return path.join(homedir(), '.claude', 'projects');
}

export const claudeAdapter: HarnessAdapter = {
  name: 'claude',
  sessionRoot: claudeProjectsRoot,

  async plan(ctx: HarnessRunContext): Promise<HarnessSpawnPlan> {
    const sessionId = randomUUID();
    return {
      binary: 'claude',
      args: ['--session-id', sessionId, ...ctx.passthrough],
      envOverrides: { RELAYBURN_SESSION_ID: sessionId },
      sessionId,
    };
  },

  async beforeSpawn(ctx: HarnessRunContext, plan: HarnessSpawnPlan): Promise<void> {
    if (!plan.sessionId) throw new Error('claude adapter: plan must include sessionId');
    await stamp({ sessionId: plan.sessionId }, ctx.tags);
    process.stderr.write(`[burn] session-id=${plan.sessionId}\n`);
  },

  async afterExit(ctx: HarnessRunContext, plan: HarnessSpawnPlan): Promise<IngestReport> {
    if (!plan.sessionId) throw new Error('claude adapter: plan must include sessionId');
    return ingestSession(ctx.cwd, plan.sessionId);
  },
};
