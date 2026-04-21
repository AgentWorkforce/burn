import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { TurnRecord } from '@relayburn/reader';

import { costForTurn, costForUsage } from './cost.js';
import { loadBuiltinPricing } from './pricing.js';

function turn(model: string, u: Partial<TurnRecord['usage']> = {}): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's',
    messageId: 'm',
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model,
    usage: {
      input: 0,
      output: 0,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
      ...u,
    },
    toolCalls: [],
  };
}

describe('cost', () => {
  it('loads builtin pricing and finds claude models', async () => {
    const p = await loadBuiltinPricing();
    assert.ok(p['claude-opus-4-7'], 'opus-4-7 present');
    assert.ok(p['claude-sonnet-4-6'], 'sonnet-4-6 present');
    assert.ok(p['claude-haiku-4-5'], 'haiku-4-5 present');
  });

  it('computes $ for a simple sonnet turn', async () => {
    const p = await loadBuiltinPricing();
    const c = costForTurn(
      turn('claude-sonnet-4-6', { input: 1_000_000, output: 1_000_000 }),
      p,
    );
    assert.ok(c);
    const rate = p['claude-sonnet-4-6']!;
    assert.equal(c.input, rate.input);
    assert.equal(c.output, rate.output);
    assert.equal(c.total, rate.input + rate.output);
  });

  it('applies cache_write rate to both 5m and 1h cache creation', async () => {
    const p = await loadBuiltinPricing();
    const c = costForUsage(
      {
        input: 0,
        output: 0,
        reasoning: 0,
        cacheRead: 0,
        cacheCreate5m: 500_000,
        cacheCreate1h: 500_000,
      },
      'claude-opus-4-7',
      p,
    );
    assert.ok(c);
    assert.equal(c.cacheCreate, p['claude-opus-4-7']!.cacheWrite);
  });

  it('returns null for unknown model', async () => {
    const p = await loadBuiltinPricing();
    const c = costForTurn(turn('definitely-not-a-model', { input: 100 }), p);
    assert.equal(c, null);
  });

  it('cache_read is much cheaper than input', async () => {
    const p = await loadBuiltinPricing();
    const rate = p['claude-opus-4-7']!;
    assert.ok(rate.cacheRead < rate.input);
  });
});
