import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { createSummaryTool, type SummaryResult } from './summary.js';

describe('createSummaryTool', () => {
  it('forwards every option, defaults session, and returns the SDK result verbatim', async () => {
    const expected: SummaryResult = {
      totalTokens: 12,
      totalCost: 0.5,
      turnCount: 2,
      byTool: [],
      byModel: [],
    };
    const tool = createSummaryTool({
      defaultSessionId: 'default-session',
      summary: async (opts) => {
        assert.deepEqual(opts, {
          session: 'default-session',
          project: '/repo',
          since: '24h',
          tags: { workflowId: 'review' },
          groupByTag: 'agentId',
        });
        return expected;
      },
    });
    const result = await tool.handler({
      project: '/repo',
      since: '24h',
      tags: { workflowId: 'review' },
      groupByTag: 'agentId',
    });
    assert.equal(result, expected);
  });

  it('rejects unknown properties and non-string tag values', async () => {
    const tool = createSummaryTool({ defaultSessionId: undefined });
    await assert.rejects(async () => { await tool.handler({ unknown: true }); }, /unknown property unknown/);
    await assert.rejects(async () => { await tool.handler({ tags: { bad: 1 } }); }, /string values/);
  });

  it('declares the SDK option schema', () => {
    const tool = createSummaryTool({ defaultSessionId: undefined });
    assert.equal(tool.name, 'burn__summary');
    assert.deepEqual(Object.keys(tool.inputSchema.properties ?? {}), [
      'session', 'project', 'since', 'tags', 'groupByTag',
    ]);
    assert.equal(tool.inputSchema.additionalProperties, false);
  });
});
