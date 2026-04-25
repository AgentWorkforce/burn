import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { EnrichedTurn } from '@relayburn/ledger';

import { createCurrentBlockTool, type CurrentBlockResult } from './current-block.js';

function turn(usage: Partial<EnrichedTurn['usage']> = {}, ts = '2026-04-24T10:00:00.000Z'): EnrichedTurn {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's1',
    messageId: 'm1',
    turnIndex: 0,
    ts,
    model: 'claude-sonnet-4-5',
    usage: {
      input: 0,
      output: 0,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
      ...usage,
    },
    toolCalls: [],
    enrichment: {},
  };
}

describe('createCurrentBlockTool', () => {
  // 2026-04-24 12:00 UTC = midpoint of a 5h window starting at 10:00 with reset at 15:00
  const NOW = new Date('2026-04-24T12:00:00.000Z');
  const RESET_AT = '2026-04-24T15:00:00.000Z'; // 3 hours away

  it('combines OAuth percent_used with locally-derived burn rate', async () => {
    // 60k tokens consumed in 2h elapsed → 500 tok/min → 5h projection = 150_000.
    // OAuth says 20% used at the 2h mark of a 5h window, projecting to 50% at
    // reset → on-track on both axes.
    const tool = createCurrentBlockTool({
      now: () => NOW,
      loadOauthToken: async () => 'tok',
      fetchUsage: async () => ({ five_hour: { percent_used: 20, reset_at: RESET_AT } }),
      queryTurns: async () => [turn({ input: 60_000 })],
    });
    const result = (await tool.handler({})) as CurrentBlockResult;
    assert.equal(result.percentUsed, 20);
    assert.equal(result.burnRateTokensPerMin, 500);
    assert.equal(result.projectedBlockTotal, 150_000);
    assert.equal(result.minutesToReset, 180);
    assert.equal(result.advice, 'on-track');
  });

  it('flags at-risk when projected reaches 80%+ at reset', async () => {
    // 40% used at 1h elapsed of a 5h window → projects to 200% at reset.
    const now = new Date('2026-04-24T11:00:00.000Z');
    const tool = createCurrentBlockTool({
      now: () => now,
      loadOauthToken: async () => 'tok',
      fetchUsage: async () => ({ five_hour: { percent_used: 40, reset_at: RESET_AT } }),
      queryTurns: async () => [],
    });
    const result = (await tool.handler({})) as CurrentBlockResult;
    assert.equal(result.advice, 'over-budget');
  });

  it('flags over-budget when current percent already >= 100', async () => {
    const tool = createCurrentBlockTool({
      now: () => NOW,
      loadOauthToken: async () => 'tok',
      fetchUsage: async () => ({ five_hour: { percent_used: 100, reset_at: RESET_AT } }),
      queryTurns: async () => [],
    });
    const result = (await tool.handler({})) as CurrentBlockResult;
    assert.equal(result.advice, 'over-budget');
  });

  it('returns null percentUsed and a note when no OAuth token is available', async () => {
    const tool = createCurrentBlockTool({
      now: () => NOW,
      loadOauthToken: async () => null,
      fetchUsage: async () => {
        throw new Error('should not be called');
      },
      queryTurns: async () => [turn({ input: 30_000 })],
    });
    const result = (await tool.handler({})) as CurrentBlockResult;
    assert.equal(result.percentUsed, null);
    assert.match(result.note ?? '', /no Claude OAuth token/);
    // Local forecast still flows even without OAuth — that's the entire
    // point of the dual-source design.
    assert.ok(result.burnRateTokensPerMin !== null);
  });

  it('keeps a local forecast when the OAuth fetch errors', async () => {
    const tool = createCurrentBlockTool({
      now: () => NOW,
      loadOauthToken: async () => 'tok',
      fetchUsage: async () => {
        throw new Error('502');
      },
      queryTurns: async () => [turn({ input: 30_000 })],
    });
    const result = (await tool.handler({})) as CurrentBlockResult;
    assert.equal(result.percentUsed, null);
    assert.match(result.note ?? '', /oauth usage unavailable/);
    assert.notEqual(result.burnRateTokensPerMin, null);
  });

  it('declares its tool surface (name, description, schema)', () => {
    const tool = createCurrentBlockTool({});
    assert.equal(tool.name, 'burn__currentBlock');
    assert.ok(tool.description.length > 0);
    assert.equal(tool.inputSchema.type, 'object');
  });
});
