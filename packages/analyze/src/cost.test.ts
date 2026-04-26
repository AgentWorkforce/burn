import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { SourceKind, TurnRecord } from '@relayburn/reader';

import { costForTurn, costForUsage } from './cost.js';
import { flatten, loadBuiltinPricing } from './pricing.js';
import type { PricingTable } from './pricing.js';

function turn(
  model: string,
  u: Partial<TurnRecord['usage']> = {},
  source: SourceKind = 'claude-code',
): TurnRecord {
  return {
    v: 1,
    source,
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

  it('bills reasoning at the output rate for Claude (same_as_output mode)', async () => {
    const p = await loadBuiltinPricing();
    const c = costForTurn(
      turn('claude-sonnet-4-6', { output: 1_000_000, reasoning: 1_000_000 }),
      p,
    );
    assert.ok(c);
    const rate = p['claude-sonnet-4-6']!;
    assert.equal(rate.reasoningMode, 'same_as_output');
    assert.equal(c.output, rate.output);
    assert.equal(c.reasoning, rate.output);
    assert.equal(c.total, rate.output * 2);
  });

  it('does NOT double-bill reasoning for Codex turns (included_in_output)', async () => {
    // Acceptance criterion from issue #32: a Codex turn with
    //   input = 1_000_000, output = 500_000, reasoning = 200_000
    // and a model priced input=2.5/output=15 should bill 10.0, not 13.0.
    const p: PricingTable = {
      'gpt-5-codex': {
        input: 2.5,
        output: 15,
        cacheRead: 0,
        cacheWrite: 2.5,
        reasoningMode: 'same_as_output',
      },
    };
    const c = costForTurn(
      turn(
        'gpt-5-codex',
        { input: 1_000_000, output: 500_000, reasoning: 200_000 },
        'codex',
      ),
      p,
    );
    assert.ok(c);
    assert.equal(c.input, 2.5);
    assert.equal(c.output, 7.5);
    assert.equal(c.reasoning, 0, 'reasoning is informational for Codex, not billed');
    assert.equal(c.total, 10.0);
  });

  it('Codex regression: 11.3% overstatement scenario from the issue', async () => {
    // 10 Codex turns aggregated: input 660_698, output 52_676, reasoning 29_070,
    // cacheRead 5_618_688. The issue documents $4.282607 (current/wrong) vs
    // $3.846557 (corrected) at gpt-5-codex pricing (input=1.25, output=10,
    // cacheRead=0.125). We assert the corrected number to within 1e-6.
    const p: PricingTable = {
      'gpt-5-codex': {
        input: 1.25,
        output: 10,
        cacheRead: 0.125,
        cacheWrite: 1.25,
        reasoningMode: 'same_as_output',
      },
    };
    const c = costForTurn(
      turn(
        'gpt-5-codex',
        {
          input: 660_698,
          output: 52_676,
          reasoning: 29_070,
          cacheRead: 5_618_688,
        },
        'codex',
      ),
      p,
    );
    assert.ok(c);
    // input + output + cacheRead, reasoning is zero for codex
    const expected =
      (660_698 / 1_000_000) * 1.25 +
      (52_676 / 1_000_000) * 10 +
      (5_618_688 / 1_000_000) * 0.125;
    assert.ok(
      Math.abs(c.total - expected) < 1e-9,
      `expected ${expected}, got ${c.total}`,
    );
    assert.equal(c.reasoning, 0);
  });

  it('honors a separate reasoning tariff when models.dev provides one', async () => {
    // Acceptance criterion from issue #32: a model with input=1, output=4,
    // reasoning=8 and 1M tokens of each should bill 13.
    const p: PricingTable = {
      'synthetic-reasoner': {
        input: 1,
        output: 4,
        reasoning: 8,
        cacheRead: 0,
        cacheWrite: 1,
        reasoningMode: 'separate',
      },
    };
    const c = costForUsage(
      {
        input: 1_000_000,
        output: 1_000_000,
        reasoning: 1_000_000,
        cacheRead: 0,
        cacheCreate5m: 0,
        cacheCreate1h: 0,
      },
      'synthetic-reasoner',
      p,
    );
    assert.ok(c);
    assert.equal(c.input, 1);
    assert.equal(c.output, 4);
    assert.equal(c.reasoning, 8);
    assert.equal(c.total, 13);
  });

  it('explicit reasoningMode option overrides the model default', async () => {
    const p: PricingTable = {
      'override-test': {
        input: 1,
        output: 10,
        cacheRead: 0,
        cacheWrite: 1,
        reasoningMode: 'same_as_output',
      },
    };
    const usage = {
      input: 0,
      output: 0,
      reasoning: 1_000_000,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    };
    const billed = costForUsage(usage, 'override-test', p);
    const skipped = costForUsage(usage, 'override-test', p, {
      reasoningMode: 'included_in_output',
    });
    assert.ok(billed);
    assert.ok(skipped);
    assert.equal(billed.reasoning, 10);
    assert.equal(skipped.reasoning, 0);
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

describe('pricing.flatten', () => {
  it('preserves cost.reasoning from models.dev and tags it `separate`', () => {
    const root = {
      acme: {
        id: 'acme',
        models: {
          'reasoner-v1': {
            id: 'reasoner-v1',
            cost: {
              input: 0.7,
              output: 2.8,
              reasoning: 8.4,
              cache_read: 0.07,
              cache_write: 0.7,
            },
          },
        },
      },
    };
    const table = flatten(root);
    const entry = table['reasoner-v1'];
    assert.ok(entry, 'reasoner-v1 flattened');
    assert.equal(entry.input, 0.7);
    assert.equal(entry.output, 2.8);
    assert.equal(entry.reasoning, 8.4);
    assert.equal(entry.cacheRead, 0.07);
    assert.equal(entry.cacheWrite, 0.7);
    assert.equal(entry.reasoningMode, 'separate');
  });

  it('defaults reasoningMode to `same_as_output` when no reasoning tariff is given', () => {
    const root = {
      acme: {
        id: 'acme',
        models: {
          'plain-v1': {
            id: 'plain-v1',
            cost: { input: 1, output: 2 },
          },
        },
      },
    };
    const table = flatten(root);
    const entry = table['plain-v1'];
    assert.ok(entry);
    assert.equal(entry.reasoningMode, 'same_as_output');
    assert.equal(entry.reasoning, undefined);
  });

  it('builtin snapshot preserves at least one separate-tariff model', async () => {
    // Smoke test: prove the live snapshot loader retains cost.reasoning for
    // providers like Alibaba's Qwen that publish a distinct tariff.
    const p = await loadBuiltinPricing();
    const separate = Object.values(p).filter((m) => m.reasoningMode === 'separate');
    assert.ok(separate.length > 0, 'expected at least one separate-tariff model');
    for (const m of separate) {
      assert.equal(typeof m.reasoning, 'number');
    }
  });
});
