import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { createOverheadTool, type OverheadResult } from './overhead.js';

describe('createOverheadTool', () => {
  it('forwards supplied options, leaves project defaulting to the SDK, and returns verbatim', async () => {
    const expected: OverheadResult = { project: '/repo', files: [], perFile: [], grandTotal: 0 };
    const tool = createOverheadTool({
      overhead: async (opts) => {
        assert.deepEqual(opts, { since: '24h', kind: 'agents-md' });
        return expected;
      },
    });
    assert.equal(await tool.handler({ since: '24h', kind: 'agents-md' }), expected);
  });

  it('rejects invalid kinds and unknown properties', async () => {
    const tool = createOverheadTool();
    await assert.rejects(async () => { await tool.handler({ kind: 'readme' }); }, /kind must be one of/);
    await assert.rejects(async () => { await tool.handler({ top: 1 }); }, /unknown property top/);
  });

  it('declares the project, since, and kind schema', () => {
    const tool = createOverheadTool();
    assert.equal(tool.name, 'burn__overhead');
    assert.deepEqual(Object.keys(tool.inputSchema.properties ?? {}), ['project', 'since', 'kind']);
  });
});
