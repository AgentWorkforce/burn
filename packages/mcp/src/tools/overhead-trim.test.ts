import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { createOverheadTrimTool, type OverheadTrimResult } from './overhead-trim.js';

describe('createOverheadTrimTool', () => {
  it('forwards every supplied option and returns the SDK result verbatim', async () => {
    const expected: OverheadTrimResult = {
      project: '/repo',
      since: '7d',
      recommendations: [],
      summary: {
        filesAnalyzed: 0,
        filesWithRecommendations: 0,
        totalRecommendations: 0,
        totalProjectedSavingsPerSession: 0,
        totalProjectedSavingsAcrossWindow: 0,
      },
    };
    const tool = createOverheadTrimTool({
      overheadTrim: async (opts) => {
        assert.deepEqual(opts, {
          project: '/repo', since: '7d', kind: 'claude-md', top: 3, includeDiff: true,
        });
        return expected;
      },
    });
    assert.equal(await tool.handler({
      project: '/repo', since: '7d', kind: 'claude-md', top: 3, includeDiff: true,
    }), expected);
  });

  it('rejects zero, negative, fractional, and incorrectly typed options', async () => {
    const tool = createOverheadTrimTool();
    await assert.rejects(async () => { await tool.handler({ top: 0 }); }, /positive safe integer/);
    await assert.rejects(async () => { await tool.handler({ top: -1 }); }, /32-bit unsigned integer/);
    await assert.rejects(async () => { await tool.handler({ top: 1.5 }); }, /32-bit unsigned integer/);
    await assert.rejects(async () => { await tool.handler({ top: 0x1_0000_0000 }); }, /32-bit unsigned integer/);
    await assert.rejects(async () => { await tool.handler({ includeDiff: 'yes' }); }, /must be a boolean/);
  });

  it('declares integer and boolean constraints', () => {
    const tool = createOverheadTrimTool();
    const props = tool.inputSchema.properties as Record<string, Record<string, unknown>>;
    assert.equal(props.top?.type, 'integer');
    assert.equal(props.top?.minimum, 1);
    assert.equal(props.top?.maximum, 0xffff_ffff);
    assert.equal(props.includeDiff?.type, 'boolean');
  });
});
