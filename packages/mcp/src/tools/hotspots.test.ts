import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { createHotspotsTool, type HotspotsResult } from './hotspots.js';

describe('createHotspotsTool', () => {
  it('supports findings mode, defaults session, and returns the union result with ledger freshness', async () => {
    const ledgerFreshness = { lastWriteAtMs: 1, staleAfterMs: 86_400_000, stale: false };
    const expected: HotspotsResult = { kind: 'findings', findings: [], summary: { count: 0 } };
    const tool = createHotspotsTool({
      defaultSessionId: 'default-session',
      ledgerFreshness: async () => ledgerFreshness,
      hotspots: async (opts) => {
        assert.deepEqual(opts, {
          session: 'default-session',
          project: '/repo',
          since: '7d',
          groupBy: 'findings',
          patterns: ['retry-loop'],
          workflow: 'review',
          provider: ['anthropic'],
        });
        return expected;
      },
    });
    const result = await tool.handler({
      project: '/repo',
      since: '7d',
      groupBy: 'findings',
      patterns: ['retry-loop'],
      workflow: 'review',
      provider: ['anthropic'],
    });
    assert.deepEqual(result, { ...expected, ledgerFreshness });
  });

  it('rejects invalid groupBy values and non-string arrays', async () => {
    const tool = createHotspotsTool({ defaultSessionId: undefined });
    await assert.rejects(async () => { await tool.handler({ groupBy: 'bogus' }); }, /groupBy must be one of/);
    await assert.rejects(async () => { await tool.handler({ provider: [1] }); }, /array of strings/);
  });

  it('declares findings in the groupBy enum', () => {
    const tool = createHotspotsTool({ defaultSessionId: undefined });
    const groupBy = tool.inputSchema.properties?.groupBy as { enum: string[] };
    assert.ok(groupBy.enum.includes('findings'));
    assert.equal(tool.name, 'burn__hotspots');
  });
});
