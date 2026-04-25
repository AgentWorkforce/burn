import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { TurnRecord } from '@relayburn/reader';

import { costForTurn } from './cost.js';
import { loadBuiltinPricing } from './pricing.js';
import {
  DEFAULT_RULES,
  resolveProvider,
  type ProviderRule,
} from './provider-reattribution.js';

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

describe('resolveProvider — synthetic detection', () => {
  it('reassigns hf:-prefixed models to synthetic and strips the org segment', () => {
    const r = resolveProvider('hf:deepseek-ai/deepseek-r1-distill');
    assert.equal(r.provider, 'synthetic');
    assert.equal(r.normalizedModel, 'deepseek-r1-distill');
    assert.equal(r.matchedRule, 'synthetic-huggingface');
  });

  it('reassigns hf: with no org segment to synthetic', () => {
    const r = resolveProvider('hf:llama-3-70b');
    assert.equal(r.provider, 'synthetic');
    assert.equal(r.normalizedModel, 'llama-3-70b');
  });

  it('reassigns accounts/fireworks/models/... to synthetic', () => {
    const r = resolveProvider('accounts/fireworks/models/deepseek-r1');
    assert.equal(r.provider, 'synthetic');
    assert.equal(r.normalizedModel, 'deepseek-r1');
    assert.equal(r.matchedRule, 'synthetic-fireworks');
  });

  it('reassigns synthetic/... explicit prefix', () => {
    const r = resolveProvider('synthetic/deepseek-r1-0528');
    assert.equal(r.provider, 'synthetic');
    assert.equal(r.normalizedModel, 'deepseek-r1-0528');
    assert.equal(r.matchedRule, 'synthetic-explicit');
  });

  it('leaves non-synthetic models alone', () => {
    const r = resolveProvider('claude-sonnet-4-6');
    assert.equal(r.provider, undefined);
    assert.equal(r.normalizedModel, 'claude-sonnet-4-6');
    assert.equal(r.matchedRule, undefined);
  });

  it('leaves anthropic/-style provider prefixes to the existing cost-lookup fallback', () => {
    // The reattribution layer is intentionally narrow: it does not own the
    // generic `provider/model` strip — that's still cost.ts's job. This
    // ensures we don't accidentally relabel "anthropic/claude-sonnet-4-6" as
    // a synthetic route.
    const r = resolveProvider('anthropic/claude-sonnet-4-6');
    assert.equal(r.provider, undefined);
    assert.equal(r.normalizedModel, 'anthropic/claude-sonnet-4-6');
  });

  it('accepts a custom rule set (extension point for future aggregators)', () => {
    const rules: ProviderRule[] = [
      ...DEFAULT_RULES,
      { name: 'openrouter', provider: 'openrouter', pattern: 'openrouter/' },
    ];
    const r = resolveProvider('openrouter/anthropic/claude-sonnet-4-6', rules);
    assert.equal(r.provider, 'openrouter');
    assert.equal(r.normalizedModel, 'anthropic/claude-sonnet-4-6');
  });

  it('first matching rule wins', () => {
    // Sanity check: explicit `synthetic/` should not also accidentally match
    // `hf:` (different prefix shape) and the fireworks rule should win over
    // any later rule that could swallow it.
    const r = resolveProvider('accounts/fireworks/models/synthetic/foo');
    assert.equal(r.matchedRule, 'synthetic-fireworks');
    assert.equal(r.normalizedModel, 'synthetic/foo');
  });
});

describe('costForTurn — reattribution-aware pricing lookup', () => {
  it('prices hf:deepseek-ai/deepseek-r1 against deepseek-r1, producing non-zero cost', async () => {
    const pricing = await loadBuiltinPricing();
    // Sanity: deepseek-r1 has a price in the vendored snapshot.
    assert.ok(pricing['deepseek-r1'], 'deepseek-r1 expected in builtin pricing');

    const t = turn('hf:deepseek-ai/deepseek-r1', { input: 1_000_000, output: 1_000_000 });
    const c = costForTurn(t, pricing);
    assert.ok(c, 'expected non-null cost for synthetic-routed deepseek-r1');
    const rate = pricing['deepseek-r1']!;
    assert.equal(c.input, rate.input);
    assert.equal(c.output, rate.output);
    assert.equal(c.total, rate.input + rate.output);
  });

  it('prices accounts/fireworks/models/deepseek-r1 against deepseek-r1', async () => {
    const pricing = await loadBuiltinPricing();
    const t = turn('accounts/fireworks/models/deepseek-r1', { input: 500_000 });
    const c = costForTurn(t, pricing);
    assert.ok(c);
    const rate = pricing['deepseek-r1']!;
    assert.equal(c.input, rate.input * 0.5);
  });

  it('still returns null for synthetic-prefixed unknown models', async () => {
    const pricing = await loadBuiltinPricing();
    const c = costForTurn(turn('hf:org/totally-fake-model-xyz', { input: 1000 }), pricing);
    assert.equal(c, null);
  });

  it('does not regress direct-priced models', async () => {
    const pricing = await loadBuiltinPricing();
    const c = costForTurn(turn('claude-sonnet-4-6', { input: 1_000_000 }), pricing);
    assert.ok(c);
    assert.equal(c.input, pricing['claude-sonnet-4-6']!.input);
  });
});
