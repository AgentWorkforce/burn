import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { EnrichedTurn } from '@relayburn/ledger';
import type { ActivityCategory } from '@relayburn/reader';

import { buildCompareTable } from './compare.js';
import { loadBuiltinPricing } from './pricing.js';

function turn(
  model: string,
  activity: ActivityCategory | undefined,
  partial: Partial<EnrichedTurn> = {},
): EnrichedTurn {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's',
    messageId: `m-${Math.random()}`,
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model,
    usage: {
      input: 1000,
      output: 500,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    ...(activity !== undefined ? { activity } : {}),
    enrichment: {},
    ...partial,
  };
}

describe('buildCompareTable', () => {
  it('buckets turns by (model, activity) and reports per-cell metrics', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      // 6 Sonnet coding turns, 4 one-shot
      ...Array.from({ length: 4 }, () =>
        turn('claude-sonnet-4-6', 'coding', { hasEdits: true, retries: 0 }),
      ),
      turn('claude-sonnet-4-6', 'coding', { hasEdits: true, retries: 2 }),
      turn('claude-sonnet-4-6', 'coding', { hasEdits: true, retries: 1 }),
      // 5 Haiku coding turns, 2 one-shot
      turn('claude-haiku-4-5', 'coding', { hasEdits: true, retries: 0 }),
      turn('claude-haiku-4-5', 'coding', { hasEdits: true, retries: 0 }),
      turn('claude-haiku-4-5', 'coding', { hasEdits: true, retries: 1 }),
      turn('claude-haiku-4-5', 'coding', { hasEdits: true, retries: 2 }),
      turn('claude-haiku-4-5', 'coding', { hasEdits: true, retries: 1 }),
      // Only Sonnet did exploration — Haiku cell should be no-data
      turn('claude-sonnet-4-6', 'exploration', { hasEdits: false }),
      turn('claude-sonnet-4-6', 'exploration', { hasEdits: false }),
    ];

    const t = buildCompareTable(turns, { pricing });

    assert.deepEqual(t.models.sort(), ['claude-haiku-4-5', 'claude-sonnet-4-6']);
    assert.ok(t.categories.includes('coding'));
    assert.ok(t.categories.includes('exploration'));

    const sonnetCoding = t.cells['claude-sonnet-4-6']!['coding']!;
    assert.equal(sonnetCoding.turns, 6);
    assert.equal(sonnetCoding.editTurns, 6);
    assert.equal(sonnetCoding.oneShotTurns, 4);
    assert.equal(sonnetCoding.oneShotRate, 4 / 6);

    const haikuCoding = t.cells['claude-haiku-4-5']!['coding']!;
    assert.equal(haikuCoding.turns, 5);
    assert.equal(haikuCoding.oneShotRate, 2 / 5);

    const haikuExploration = t.cells['claude-haiku-4-5']!['exploration']!;
    assert.equal(haikuExploration.turns, 0);
    // No-data cells use noData=true, NOT insufficientSample=true. The two
    // flags are mutually exclusive so JSON/CSV consumers can distinguish
    // "we never saw this combination" from "we have data but the sample is
    // small."
    assert.equal(haikuExploration.noData, true);
    assert.equal(haikuExploration.insufficientSample, false);
    assert.equal(haikuExploration.costPerTurn, null);
    assert.equal(haikuExploration.oneShotRate, null);
  });

  it('returns null oneShotRate for categories with no edit turns', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'exploration', { hasEdits: false }),
      turn('claude-sonnet-4-6', 'exploration', { hasEdits: false }),
    ];
    const t = buildCompareTable(turns, { pricing });
    const cell = t.cells['claude-sonnet-4-6']!['exploration']!;
    assert.equal(cell.turns, 2);
    assert.equal(cell.editTurns, 0);
    assert.equal(cell.oneShotRate, null);
    assert.equal(cell.medianRetries, null);
  });

  it('flags cells below minSample as insufficient (and not as noData)', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'refactoring', { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'refactoring', { hasEdits: true, retries: 0 }),
    ];
    const t = buildCompareTable(turns, { pricing, minSample: 5 });
    const cell = t.cells['claude-sonnet-4-6']!['refactoring']!;
    assert.equal(cell.insufficientSample, true);
    assert.equal(cell.noData, false);
    assert.equal(cell.turns, 2);
  });

  it('applies the --models filter', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', { hasEdits: true, retries: 0 }),
      turn('claude-haiku-4-5', 'coding', { hasEdits: true, retries: 0 }),
      turn('claude-opus-4-7', 'coding', { hasEdits: true, retries: 0 }),
    ];
    const t = buildCompareTable(turns, {
      pricing,
      models: ['claude-sonnet-4-6', 'claude-haiku-4-5'],
    });
    assert.deepEqual(t.models.sort(), ['claude-haiku-4-5', 'claude-sonnet-4-6']);
  });

  it('keeps explicitly-requested models visible even with zero matching turns', async () => {
    // Without pre-seeding from opts.models, a model the user asked about
    // disappears entirely (no all-empty column, no "no <model> data" note).
    // The whole point of --models is to compare against a target — losing
    // the column hides the gap instead of surfacing it.
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', { hasEdits: true, retries: 0 }),
    ];
    const t = buildCompareTable(turns, {
      pricing,
      models: ['claude-sonnet-4-6', 'claude-haiku-4-5'],
    });
    assert.ok(t.models.includes('claude-haiku-4-5'), 'requested model must remain in the table');
    const haikuCell = t.cells['claude-haiku-4-5']!['coding']!;
    assert.equal(haikuCell.noData, true);
    assert.equal(haikuCell.turns, 0);
    assert.equal(t.totals['claude-haiku-4-5']!.turns, 0);
  });

  it('renders costPerTurn as null when no turn in the cell had pricing', async () => {
    // costForTurn returns null for unknown models. If we divided totalCost
    // (0) by turns (>0) we'd report $0.00 — falsely claiming a model is
    // free. Instead expose pricedTurns and null out costPerTurn.
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      turn('definitely-not-a-model', 'coding', { hasEdits: true, retries: 0 }),
      turn('definitely-not-a-model', 'coding', { hasEdits: true, retries: 1 }),
    ];
    const t = buildCompareTable(turns, { pricing });
    const cell = t.cells['definitely-not-a-model']!['coding']!;
    assert.equal(cell.turns, 2);
    assert.equal(cell.pricedTurns, 0);
    assert.equal(cell.totalCost, 0);
    assert.equal(cell.costPerTurn, null);
  });

  it('uses pricedTurns (not turns) as the costPerTurn denominator', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', {
        hasEdits: true,
        retries: 0,
        usage: { input: 1_000_000, output: 0, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
      turn('claude-sonnet-4-6', 'coding', {
        hasEdits: true,
        retries: 0,
        usage: { input: 1_000_000, output: 0, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];
    const t = buildCompareTable(turns, { pricing });
    const cell = t.cells['claude-sonnet-4-6']!['coding']!;
    assert.equal(cell.pricedTurns, 2);
    assert.equal(cell.turns, 2);
    assert.ok(cell.costPerTurn !== null);
    assert.ok(Math.abs(cell.costPerTurn! - cell.totalCost / 2) < 1e-9);
  });

  it('groups turns missing activity under "unclassified"', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', undefined, { hasEdits: false }),
      turn('claude-sonnet-4-6', undefined, { hasEdits: false }),
    ];
    const t = buildCompareTable(turns, { pricing });
    assert.ok(t.categories.includes('unclassified'));
    assert.equal(t.cells['claude-sonnet-4-6']!['unclassified']!.turns, 2);
  });

  it('total cost per model matches sum of its cells', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', {
        hasEdits: true,
        retries: 0,
        usage: {
          input: 500_000,
          output: 100_000,
          reasoning: 0,
          cacheRead: 0,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
      turn('claude-sonnet-4-6', 'debugging', {
        hasEdits: true,
        retries: 1,
        usage: {
          input: 1_000_000,
          output: 100_000,
          reasoning: 0,
          cacheRead: 0,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
    ];
    const t = buildCompareTable(turns, { pricing });
    const sum =
      t.cells['claude-sonnet-4-6']!['coding']!.totalCost +
      t.cells['claude-sonnet-4-6']!['debugging']!.totalCost;
    assert.ok(Math.abs(sum - t.totals['claude-sonnet-4-6']!.totalCost) < 1e-9);
  });

  it('computes cache hit rate across input + cacheRead + cacheCreate', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', {
        hasEdits: true,
        retries: 0,
        usage: {
          input: 1000,
          output: 200,
          reasoning: 0,
          cacheRead: 3000,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
    ];
    const t = buildCompareTable(turns, { pricing });
    const cell = t.cells['claude-sonnet-4-6']!['coding']!;
    assert.ok(cell.cacheHitRate !== null);
    assert.ok(Math.abs(cell.cacheHitRate! - 3000 / 4000) < 1e-9);
  });
});
