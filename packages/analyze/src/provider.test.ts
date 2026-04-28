import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { TurnRecord } from '@relayburn/reader';

import { aggregateByProvider, providerFor } from './provider.js';
import type { PricingTable } from './pricing.js';

const pricing: PricingTable = {
  'deepseek-r1': {
    input: 1,
    output: 2,
    cacheRead: 0.1,
    cacheWrite: 1.25,
    reasoningMode: 'same_as_output',
  },
  'gpt-5': {
    input: 3,
    output: 6,
    cacheRead: 0.3,
    cacheWrite: 3.75,
    reasoningMode: 'same_as_output',
  },
};

function turn(overrides: Partial<TurnRecord> = {}): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-provider',
    messageId: 'm-provider',
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    usage: {
      input: 1_000_000,
      output: 1_000_000,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    ...overrides,
  };
}

describe('effective provider helpers', () => {
  it('reattributes synthetic-routed models before source fallback', () => {
    const provider = providerFor(turn({ model: 'hf:deepseek-ai/deepseek-r1' }));
    assert.equal(provider.provider, 'synthetic');
    assert.equal(provider.normalizedModel, 'deepseek-r1');
    assert.equal(provider.matchedRule, 'synthetic-huggingface');
  });

  it('falls through to collector-implied providers for normal turns', () => {
    assert.equal(providerFor(turn({ source: 'codex', model: 'gpt-5' })).provider, 'openai');
    assert.equal(
      providerFor(turn({ source: 'opencode', model: 'anthropic/claude-sonnet-4-6' })).provider,
      'anthropic',
    );
  });
});

describe('aggregateByProvider', () => {
  it('aggregates synthetic turns together regardless of routing prefix', () => {
    const rows = aggregateByProvider([
      turn({
        model: 'hf:deepseek-ai/deepseek-r1',
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
        model: 'synthetic/deepseek-r1',
        usage: {
          input: 0,
          output: 1_000_000,
          reasoning: 0,
          cacheRead: 0,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
    ], { pricing });

    assert.equal(rows.length, 1);
    assert.equal(rows[0]!.provider, 'synthetic');
    assert.equal(rows[0]!.turns, 2);
    assert.equal(rows[0]!.usage.input, 1_000_000);
    assert.equal(rows[0]!.usage.output, 1_000_000);
    assert.equal(rows[0]!.cost.total, 3);
  });

  it('falls through to collector provider for non-synthetic turns', () => {
    const rows = aggregateByProvider([
      turn({ source: 'codex', model: 'gpt-5' }),
    ], { pricing });

    assert.equal(rows.length, 1);
    assert.equal(rows[0]!.provider, 'openai');
    assert.equal(rows[0]!.cost.total, 9);
  });

  it('returns an empty row set for empty input', () => {
    assert.deepEqual(aggregateByProvider([], { pricing }), []);
  });
});
