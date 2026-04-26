import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { PricingTable } from '@relayburn/analyze';
import type { EnrichedTurn } from '@relayburn/ledger';

import { createSessionCostTool, type SessionCostResult } from './session-cost.js';

const PRICING: PricingTable = {
  'claude-sonnet-4-5': {
    input: 3,
    output: 15,
    cacheRead: 0.3,
    cacheWrite: 3.75,
    reasoningMode: 'same_as_output',
  },
};

function turn(overrides: Partial<EnrichedTurn> = {}): EnrichedTurn {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's1',
    messageId: 'm1',
    turnIndex: 0,
    ts: '2026-04-24T10:00:00.000Z',
    model: 'claude-sonnet-4-5',
    usage: {
      input: 1000,
      output: 500,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    enrichment: {},
    ...overrides,
  };
}

describe('createSessionCostTool', () => {
  it('returns zero totals when defaultSessionId is missing and no override given', async () => {
    const tool = createSessionCostTool({
      defaultSessionId: undefined,
      queryTurns: async () => [],
      loadPricing: async () => PRICING,
    });
    const result = (await tool.handler({})) as SessionCostResult;
    assert.equal(result.sessionId, null);
    assert.equal(result.totalUSD, 0);
    assert.equal(result.turnCount, 0);
    assert.match(result.note ?? '', /no session id/);
  });

  it('uses the override sessionId when provided', async () => {
    let queriedFor: string | undefined;
    const tool = createSessionCostTool({
      defaultSessionId: 'default-id',
      queryTurns: async (id) => {
        queriedFor = id;
        return [turn({ sessionId: id })];
      },
      loadPricing: async () => PRICING,
    });
    await tool.handler({ sessionId: 'override-id' });
    assert.equal(queriedFor, 'override-id');
  });

  it('falls back to defaultSessionId when no override given', async () => {
    let queriedFor: string | undefined;
    const tool = createSessionCostTool({
      defaultSessionId: 'baked-id',
      queryTurns: async (id) => {
        queriedFor = id;
        return [];
      },
      loadPricing: async () => PRICING,
    });
    await tool.handler({});
    assert.equal(queriedFor, 'baked-id');
  });

  it('aggregates usage and cost across turns', async () => {
    const turns: EnrichedTurn[] = [
      turn({
        usage: {
          input: 1_000_000,
          output: 0,
          reasoning: 0,
          cacheRead: 0,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
      turn({
        messageId: 'm2',
        turnIndex: 1,
        usage: {
          input: 0,
          output: 1_000_000,
          reasoning: 0,
          cacheRead: 0,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
    ];
    const tool = createSessionCostTool({
      defaultSessionId: 's1',
      queryTurns: async () => turns,
      loadPricing: async () => PRICING,
    });
    const result = (await tool.handler({})) as SessionCostResult;
    assert.equal(result.turnCount, 2);
    assert.equal(result.totalTokens, 2_000_000);
    // 1M input @ $3/M + 1M output @ $15/M = $18.
    assert.equal(result.totalUSD, 18);
    assert.deepEqual(result.models, ['claude-sonnet-4-5']);
  });

  it('records a note when the session has no turns yet', async () => {
    const tool = createSessionCostTool({
      defaultSessionId: 's1',
      queryTurns: async () => [],
      loadPricing: async () => PRICING,
    });
    const result = (await tool.handler({})) as SessionCostResult;
    assert.equal(result.turnCount, 0);
    assert.match(result.note ?? '', /no turns recorded/);
  });

  it('declares its tool surface (name, description, schema)', () => {
    const tool = createSessionCostTool({ defaultSessionId: undefined });
    assert.equal(tool.name, 'burn__sessionCost');
    assert.ok(tool.description.length > 0);
    assert.equal(tool.inputSchema.type, 'object');
    assert.equal(tool.inputSchema.additionalProperties, false);
  });
});
