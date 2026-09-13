import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { createFingerprintTool, type FingerprintResult } from './fingerprint.js';

const FRESHNESS_DEP = {
  ledgerFreshness: async () => ({ lastWriteAtMs: 1, staleAfterMs: 86_400_000, stale: false }),
};

describe('createFingerprintTool', () => {
  it('returns the SDK fingerprint string verbatim', async () => {
    const tool = createFingerprintTool({
      ...FRESHNESS_DEP,
      fingerprint: async () => ({ fingerprint: '42:1700000000:9876' }),
    });
    const result = (await tool.handler({})) as FingerprintResult;
    assert.equal(result.fingerprint, '42:1700000000:9876');
    assert.deepEqual(result.ledgerFreshness, {
      lastWriteAtMs: 1,
      staleAfterMs: 86_400_000,
      stale: false,
    });
  });

  it('returns stale freshness metadata unchanged', async () => {
    const tool = createFingerprintTool({
      ledgerFreshness: async () => ({ lastWriteAtMs: 1, staleAfterMs: 2, stale: true }),
      fingerprint: async () => ({ fingerprint: '1:1:1' }),
    });
    const result = (await tool.handler({})) as FingerprintResult;
    assert.deepEqual(result.ledgerFreshness, { lastWriteAtMs: 1, staleAfterMs: 2, stale: true });
  });

  it('passes sessionId through as session', async () => {
    let captured: { session?: string; project?: string } = {};
    const tool = createFingerprintTool({
      ...FRESHNESS_DEP,
      fingerprint: async (opts) => {
        captured = opts;
        return { fingerprint: '1:1:1' };
      },
    });
    await tool.handler({ sessionId: 'sess-xyz' });
    assert.equal(captured.session, 'sess-xyz');
    assert.equal(captured.project, undefined);
  });

  it('passes project through', async () => {
    let captured: { session?: string; project?: string } = {};
    const tool = createFingerprintTool({
      ...FRESHNESS_DEP,
      fingerprint: async (opts) => {
        captured = opts;
        return { fingerprint: '1:1:1' };
      },
    });
    await tool.handler({ project: '/tmp/proj' });
    assert.equal(captured.project, '/tmp/proj');
    assert.equal(captured.session, undefined);
  });

  it('rejects sessionId + project together', async () => {
    const tool = createFingerprintTool({
      ...FRESHNESS_DEP,
      fingerprint: async () => ({ fingerprint: 'unreachable' }),
    });
    await assert.rejects(
      async () => tool.handler({ sessionId: 's', project: '/p' }),
      /pass at most one/,
    );
  });

  it('declares its tool surface (name, description, schema)', () => {
    const tool = createFingerprintTool();
    assert.equal(tool.name, 'burn__fingerprint');
    assert.ok(tool.description.length > 0);
    assert.equal(tool.inputSchema.type, 'object');
    assert.equal(tool.inputSchema.additionalProperties, false);
  });
});
