import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { createCompareTool, type CompareResult } from './compare.js';

describe('createCompareTool', () => {
  it('forwards every SDK option without implicitly scoping to a server session', async () => {
    const expected: CompareResult = {
      analyzedTurns: 0,
      minSample: 2,
      models: ['a', 'b'],
      categories: [],
      totals: {},
      cells: [],
      fidelity: {
        minimum: 'partial',
        excluded: { total: 0, aggregateOnly: 0, costOnly: 0, partial: 0, usageOnly: 0 },
        summary: { total: 0, byClass: { full: 0, 'usage-only': 0, 'aggregate-only': 0, 'cost-only': 0, partial: 0 }, unknown: 0, missingCoverage: {} },
      },
    };
    const tool = createCompareTool({
      compare: async (opts) => {
        assert.deepEqual(opts, {
          models: ['a', 'b'], project: '/repo', since: '4w', workflow: 'review',
          agent: 'worker', provider: ['anthropic'], minSample: 2, minFidelity: 'partial',
        });
        return expected;
      },
    });
    assert.equal(await tool.handler({
      models: ['a', 'b'], project: '/repo', since: '4w', workflow: 'review',
      agent: 'worker', provider: ['anthropic'], minSample: 2, minFidelity: 'partial',
    }), expected);
  });

  it('rejects missing/short model lists and invalid numeric or enum constraints', async () => {
    const tool = createCompareTool();
    await assert.rejects(async () => { await tool.handler({}); }, /models must contain at least 2 strings/);
    await assert.rejects(async () => { await tool.handler({ models: ['a'] }); }, /at least 2 strings/);
    await assert.rejects(async () => { await tool.handler({ models: ['a', 'b'], minSample: -1 }); }, /32-bit unsigned integer/);
    await assert.rejects(
      async () => { await tool.handler({ models: ['a', 'b'], minSample: 0x1_0000_0000 }); },
      /32-bit unsigned integer/,
    );
    await assert.rejects(async () => { await tool.handler({ models: ['a', 'b'], minFidelity: 'bogus' }); }, /must be one of/);
  });

  it('declares models as required with two items', () => {
    const tool = createCompareTool();
    const models = tool.inputSchema.properties?.models as { minItems: number };
    const minSample = tool.inputSchema.properties?.minSample as { maximum: number };
    assert.deepEqual(tool.inputSchema.required, ['models']);
    assert.equal(models.minItems, 2);
    assert.equal(minSample.maximum, 0xffff_ffff);
    assert.equal(tool.name, 'burn__compare');
  });
});
