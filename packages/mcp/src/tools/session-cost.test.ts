import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { createSessionCostTool, type SessionCostResult } from './session-cost.js';

describe('createSessionCostTool', () => {
  it('returns the SDK no-session shape with the MCP-specific note when no id is registered', async () => {
    const tool = createSessionCostTool({
      defaultSessionId: undefined,
      sessionCost: async () => ({
        sessionId: null,
        totalUSD: 0,
        totalTokens: 0,
        turnCount: 0,
        models: [],
        note: 'no session id provided',
      }),
    });
    const result = (await tool.handler({})) as SessionCostResult;
    assert.equal(result.sessionId, null);
    assert.equal(result.totalUSD, 0);
    assert.equal(result.turnCount, 0);
    assert.match(result.note ?? '', /no session id provided and server was not registered/);
  });

  it('uses the override sessionId when provided', async () => {
    let queriedFor: string | undefined;
    const tool = createSessionCostTool({
      defaultSessionId: 'default-id',
      sessionCost: async (opts) => {
        queriedFor = opts.session;
        return {
          sessionId: opts.session ?? null,
          totalUSD: 0,
          totalTokens: 0,
          turnCount: 1,
          models: ['claude-sonnet-4-5'],
        };
      },
    });
    await tool.handler({ sessionId: 'override-id' });
    assert.equal(queriedFor, 'override-id');
  });

  it('falls back to defaultSessionId when no override given', async () => {
    let queriedFor: string | undefined;
    const tool = createSessionCostTool({
      defaultSessionId: 'baked-id',
      sessionCost: async (opts) => {
        queriedFor = opts.session;
        return {
          sessionId: opts.session ?? null,
          totalUSD: 0,
          totalTokens: 0,
          turnCount: 0,
          models: [],
        };
      },
    });
    await tool.handler({});
    assert.equal(queriedFor, 'baked-id');
  });

  it('returns the SDK result verbatim when a session id is present', async () => {
    const tool = createSessionCostTool({
      defaultSessionId: 's1',
      sessionCost: async (opts) => ({
        sessionId: opts.session ?? null,
        totalUSD: 18,
        totalTokens: 2_000_000,
        turnCount: 2,
        models: ['claude-sonnet-4-5'],
      }),
    });
    const result = (await tool.handler({})) as SessionCostResult;
    assert.equal(result.sessionId, 's1');
    assert.equal(result.turnCount, 2);
    assert.equal(result.totalTokens, 2_000_000);
    assert.equal(result.totalUSD, 18);
    assert.deepEqual(result.models, ['claude-sonnet-4-5']);
    assert.equal(result.note, undefined);
  });

  it('declares its tool surface (name, description, schema)', () => {
    const tool = createSessionCostTool({ defaultSessionId: undefined });
    assert.equal(tool.name, 'burn__sessionCost');
    assert.ok(tool.description.length > 0);
    assert.equal(tool.inputSchema.type, 'object');
    assert.equal(tool.inputSchema.additionalProperties, false);
  });
});
