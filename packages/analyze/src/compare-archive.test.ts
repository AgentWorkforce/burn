import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, beforeEach, describe, it } from 'node:test';

import type { TurnRecord } from '@relayburn/reader';
import {
  appendTurns,
  buildArchive,
  queryAll,
  stamp,
  type Query,
} from '@relayburn/ledger';

import { buildCompareTable, type CompareOptions } from './compare.js';
import { compareFromArchive } from './compare-archive.js';
import { loadBuiltinPricing } from './pricing.js';

describe('compareFromArchive', () => {
  let tmpDir: string;
  const originalHome = process.env['RELAYBURN_HOME'];
  // Per-test unique suffix folded into every messageId AND every default
  // token count. Two layers of dedup live in the writer (`turnIdHash` keyed on
  // (source|sessionId|messageId) and `turnContentFingerprint` keyed on
  // (ts|model|input+output|cacheRead|cacheCreate*|firstToolArgsPrefix)), and
  // both caches are module-scoped — they outlive a `RELAYBURN_HOME` reset.
  // Bumping the suffix per test makes both ids and content fingerprints
  // unique so the second test's appendTurns isn't silently deduped against
  // the first's. Avoids needing to export the private
  // __resetIndexCacheForTesting hook from @relayburn/ledger.
  let testIdSuffix = 0;
  function uid(label: string): string {
    return `${label}-${testIdSuffix}`;
  }

  // Build a TurnRecord with the minimum required fields. Tests override only
  // the dimensions they care about; everything else defaults to a stable shape
  // so parity comparisons aren't perturbed by unrelated fields. The default
  // `usage.input` is keyed off `testIdSuffix` so two tests that don't override
  // tokens still produce distinct content fingerprints (see comment above).
  function fakeTurn(overrides: Partial<TurnRecord> = {}): TurnRecord {
    return {
      v: 1,
      source: 'claude-code',
      sessionId: uid('s-default'),
      messageId: `m-${Math.random().toString(36).slice(2)}`,
      turnIndex: 0,
      ts: '2026-04-20T00:00:00.000Z',
      model: 'claude-sonnet-4-6',
      usage: {
        input: 1000 + testIdSuffix * 17,
        output: 500,
        reasoning: 0,
        cacheRead: 0,
        cacheCreate5m: 0,
        cacheCreate1h: 0,
      },
      toolCalls: [],
      project: '/tmp/project',
      ...overrides,
    };
  }

  before(async () => {
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-compare-archive-test-'));
  });

  beforeEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
    tmpDir = await mkdtemp(path.join(tmpdir(), 'relayburn-compare-archive-test-'));
    process.env['RELAYBURN_HOME'] = tmpDir;
    testIdSuffix++;
  });

  after(async () => {
    if (originalHome !== undefined) {
      process.env['RELAYBURN_HOME'] = originalHome;
    } else {
      delete process.env['RELAYBURN_HOME'];
    }
    await rm(tmpDir, { recursive: true, force: true });
  });

  // The headline acceptance criterion: same fixture, both code paths, same
  // CompareTable + same analyzedTurns. We use a deliberately mixed fixture
  // (multiple models, edit/non-edit categories, retries variation,
  // cache-heavy turns, and an unpriced model) so the parity check exercises
  // every cell-level metric and the sort orderings.
  it('parity: matches in-memory buildCompareTable for a mixed fixture', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    let mid = 0;
    const next = (): string => uid(`m-${++mid}`);

    // 6 Sonnet coding turns, 4 one-shot, varying retries & token weights.
    const sSonnet = uid('s-sonnet');
    for (let i = 0; i < 4; i++) {
      turns.push(
        fakeTurn({
          messageId: next(),
          sessionId: sSonnet,
          turnIndex: i,
          ts: `2026-04-20T00:00:${String(i).padStart(2, '0')}.000Z`,
          model: 'claude-sonnet-4-6',
          activity: 'coding',
          hasEdits: true,
          retries: 0,
          usage: {
            input: 5000,
            output: 800,
            reasoning: 0,
            cacheRead: 12000,
            cacheCreate5m: 0,
            cacheCreate1h: 0,
          },
        }),
      );
    }
    turns.push(
      fakeTurn({
        messageId: next(),
        sessionId: sSonnet,
        turnIndex: 4,
        ts: '2026-04-20T00:00:04.000Z',
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
        retries: 2,
      }),
      fakeTurn({
        messageId: next(),
        sessionId: sSonnet,
        turnIndex: 5,
        ts: '2026-04-20T00:00:05.000Z',
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
        retries: 1,
      }),
    );

    // 5 Haiku coding turns, 2 one-shot. Shape the cacheRead/input mix
    // differently so cacheHitRate parity is non-trivial.
    const sHaiku = uid('s-haiku');
    for (let i = 0; i < 5; i++) {
      turns.push(
        fakeTurn({
          messageId: next(),
          sessionId: sHaiku,
          turnIndex: i,
          ts: `2026-04-20T01:00:${String(i).padStart(2, '0')}.000Z`,
          model: 'claude-haiku-4-5',
          activity: 'coding',
          hasEdits: true,
          retries: i < 2 ? 0 : i,
          usage: {
            input: 2000,
            output: 400,
            reasoning: 0,
            cacheRead: i % 2 === 0 ? 6000 : 0,
            cacheCreate5m: 0,
            cacheCreate1h: 0,
          },
        }),
      );
    }

    // Sonnet exploration (no edits).
    const sExpl = uid('s-expl');
    turns.push(
      fakeTurn({
        messageId: next(),
        sessionId: sExpl,
        model: 'claude-sonnet-4-6',
        activity: 'exploration',
        hasEdits: false,
      }),
      fakeTurn({
        messageId: next(),
        sessionId: sExpl,
        turnIndex: 1,
        ts: '2026-04-20T02:00:01.000Z',
        model: 'claude-sonnet-4-6',
        activity: 'exploration',
        hasEdits: false,
      }),
    );

    // Unpriced model — exercises the costPerTurn=null branch for both paths.
    turns.push(
      fakeTurn({
        messageId: next(),
        sessionId: uid('s-unpriced'),
        model: 'definitely-not-a-model',
        activity: 'coding',
        hasEdits: true,
        retries: 1,
      }),
    );

    // Codex-source turn — exercises the per-source `included_in_output`
    // override that compareFromArchive folds via grouping on (model,
    // activity, source).
    turns.push(
      fakeTurn({
        messageId: next(),
        sessionId: uid('s-codex'),
        source: 'codex',
        model: 'gpt-5-codex',
        activity: 'coding',
        hasEdits: true,
        retries: 0,
        usage: {
          input: 10000,
          output: 2000,
          reasoning: 800,
          cacheRead: 0,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
    );

    await appendTurns(turns);
    await buildArchive();

    const opts: CompareOptions = { pricing, minSample: 5 };
    const inMemoryTurns = await queryAll({});
    const inMemory = buildCompareTable(inMemoryTurns, opts);
    const fromArchive = await compareFromArchive({}, opts);

    assert.deepEqual(fromArchive.table.models, inMemory.models, 'models order');
    assert.deepEqual(fromArchive.table.categories, inMemory.categories, 'categories order');
    assert.deepEqual(fromArchive.table.minSample, inMemory.minSample);
    assert.deepEqual(fromArchive.analyzedTurns, inMemoryTurns.length, 'analyzedTurns');

    // Per-cell deep equality across every (model, category) pair.
    for (const m of inMemory.models) {
      for (const cat of inMemory.categories) {
        const a = fromArchive.table.cells[m]![cat]!;
        const b = inMemory.cells[m]![cat]!;
        assert.equal(a.turns, b.turns, `${m}/${cat} turns`);
        assert.equal(a.editTurns, b.editTurns, `${m}/${cat} editTurns`);
        assert.equal(a.oneShotTurns, b.oneShotTurns, `${m}/${cat} oneShotTurns`);
        assert.equal(a.pricedTurns, b.pricedTurns, `${m}/${cat} pricedTurns`);
        assertNumNear(a.totalCost, b.totalCost, `${m}/${cat} totalCost`);
        assertNumNear(a.costPerTurn, b.costPerTurn, `${m}/${cat} costPerTurn`);
        assertNumNear(a.oneShotRate, b.oneShotRate, `${m}/${cat} oneShotRate`);
        assertNumNear(a.cacheHitRate, b.cacheHitRate, `${m}/${cat} cacheHitRate`);
        assert.equal(a.medianRetries, b.medianRetries, `${m}/${cat} medianRetries`);
        assert.equal(a.noData, b.noData, `${m}/${cat} noData`);
        assert.equal(a.insufficientSample, b.insufficientSample, `${m}/${cat} insufficientSample`);
      }
    }

    // Per-model totals must match (turns + totalCost) within float epsilon.
    for (const m of inMemory.models) {
      const a = fromArchive.table.totals[m]!;
      const b = inMemory.totals[m]!;
      assert.equal(a.turns, b.turns, `${m} totals.turns`);
      assertNumNear(a.totalCost, b.totalCost, `${m} totals.totalCost`);
    }
  });

  it('honors --models filter and pre-seeds requested-but-absent models', async () => {
    const pricing = await loadBuiltinPricing();
    await appendTurns([
      fakeTurn({
        messageId: uid('m-1'),
        sessionId: uid('s-1'),
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
        retries: 0,
      }),
      fakeTurn({
        messageId: uid('m-2'),
        sessionId: uid('s-2'),
        model: 'claude-opus-4-7',
        activity: 'coding',
        hasEdits: true,
        retries: 0,
      }),
    ]);
    await buildArchive();

    const result = await compareFromArchive(
      {},
      { pricing, models: ['claude-sonnet-4-6', 'claude-haiku-4-5'] },
    );
    // Opus filtered out; Haiku pre-seeded as an empty column.
    assert.deepEqual(result.table.models.sort(), ['claude-haiku-4-5', 'claude-sonnet-4-6']);
    assert.equal(result.table.cells['claude-haiku-4-5']!['coding']!.noData, true);
    assert.equal(result.table.totals['claude-haiku-4-5']!.turns, 0);
    // analyzedTurns is the pre-`--models` count and must include both
    // ledger turns (matches the legacy `queryAll(q).length` semantics).
    assert.equal(result.analyzedTurns, 2);
  });

  it('honors --since', async () => {
    const pricing = await loadBuiltinPricing();
    await appendTurns([
      fakeTurn({
        messageId: uid('m-old'),
        sessionId: uid('s-1'),
        ts: '2026-04-19T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
        retries: 0,
      }),
      fakeTurn({
        messageId: uid('m-new'),
        sessionId: uid('s-2'),
        ts: '2026-04-21T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
        retries: 0,
      }),
    ]);
    await buildArchive();

    const q: Query = { since: '2026-04-20T00:00:00.000Z' };
    const result = await compareFromArchive(q, { pricing });
    assert.equal(result.analyzedTurns, 1);
    assert.equal(result.table.cells['claude-sonnet-4-6']!['coding']!.turns, 1);
  });

  it('honors --project (matches both literal project path and projectKey)', async () => {
    const pricing = await loadBuiltinPricing();
    await appendTurns([
      fakeTurn({
        messageId: uid('m-a'),
        sessionId: uid('s-1'),
        project: '/tmp/proj-a',
        projectKey: 'github.com/me/a',
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
      }),
      fakeTurn({
        messageId: uid('m-b'),
        sessionId: uid('s-2'),
        project: '/tmp/proj-b',
        projectKey: 'github.com/me/b',
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
      }),
    ]);
    await buildArchive();

    // Literal path filter.
    const byPath = await compareFromArchive({ project: '/tmp/proj-a' }, { pricing });
    assert.equal(byPath.analyzedTurns, 1);

    // projectKey filter.
    const byKey = await compareFromArchive({ project: 'github.com/me/b' }, { pricing });
    assert.equal(byKey.analyzedTurns, 1);
  });

  it('honors --session', async () => {
    const pricing = await loadBuiltinPricing();
    const sx = uid('s-X');
    await appendTurns([
      fakeTurn({
        messageId: uid('m-x'),
        sessionId: sx,
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
      }),
      fakeTurn({
        messageId: uid('m-y'),
        sessionId: uid('s-Y'),
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
      }),
    ]);
    await buildArchive();

    const result = await compareFromArchive({ sessionId: sx }, { pricing });
    assert.equal(result.analyzedTurns, 1);
  });

  it('honors --workflow and --agent (enrichment from stamps)', async () => {
    const pricing = await loadBuiltinPricing();
    const sw = uid('s-W');
    await appendTurns([
      fakeTurn({
        messageId: uid('m-w'),
        sessionId: sw,
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
      }),
      fakeTurn({
        messageId: uid('m-other'),
        sessionId: uid('s-other'),
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
      }),
    ]);
    await stamp({ sessionId: sw }, { workflowId: 'wf-42', agentId: 'agent-7' });
    await buildArchive();

    const byWorkflow = await compareFromArchive(
      { enrichment: { workflowId: 'wf-42' } },
      { pricing },
    );
    assert.equal(byWorkflow.analyzedTurns, 1);

    const byAgent = await compareFromArchive(
      { enrichment: { agentId: 'agent-7' } },
      { pricing },
    );
    assert.equal(byAgent.analyzedTurns, 1);

    // A workflow that doesn't exist returns an empty table without throwing.
    const empty = await compareFromArchive(
      { enrichment: { workflowId: 'wf-nope' } },
      { pricing },
    );
    assert.equal(empty.analyzedTurns, 0);
    assert.deepEqual(empty.table.models, []);
    assert.deepEqual(empty.table.categories, []);
  });

  it('honors --min-sample (flags low-sample cells as insufficientSample)', async () => {
    const pricing = await loadBuiltinPricing();
    await appendTurns([
      fakeTurn({
        messageId: uid('m-1'),
        sessionId: uid('s-1'),
        model: 'claude-sonnet-4-6',
        activity: 'refactoring',
        hasEdits: true,
      }),
      fakeTurn({
        messageId: uid('m-2'),
        sessionId: uid('s-2'),
        model: 'claude-sonnet-4-6',
        activity: 'refactoring',
        hasEdits: true,
      }),
    ]);
    await buildArchive();

    const result = await compareFromArchive({}, { pricing, minSample: 5 });
    const cell = result.table.cells['claude-sonnet-4-6']!['refactoring']!;
    assert.equal(cell.turns, 2);
    assert.equal(cell.insufficientSample, true);
    assert.equal(cell.noData, false);
  });

  it('empty archive yields an empty table with analyzedTurns=0', async () => {
    const pricing = await loadBuiltinPricing();
    await buildArchive();
    const result = await compareFromArchive({}, { pricing });
    assert.equal(result.analyzedTurns, 0);
    assert.deepEqual(result.table.models, []);
    assert.deepEqual(result.table.categories, []);
    assert.deepEqual(result.table.totals, {});
  });

  it('single-cell archive: one (model, activity) pair populates exactly one cell', async () => {
    const pricing = await loadBuiltinPricing();
    await appendTurns([
      fakeTurn({
        messageId: uid('m-only'),
        sessionId: uid('s-only'),
        model: 'claude-sonnet-4-6',
        activity: 'coding',
        hasEdits: true,
        retries: 0,
      }),
    ]);
    await buildArchive();

    const result = await compareFromArchive({}, { pricing });
    assert.deepEqual(result.table.models, ['claude-sonnet-4-6']);
    assert.deepEqual(result.table.categories, ['coding']);
    const cell = result.table.cells['claude-sonnet-4-6']!['coding']!;
    assert.equal(cell.turns, 1);
    assert.equal(cell.editTurns, 1);
    assert.equal(cell.oneShotTurns, 1);
    assert.equal(cell.medianRetries, 0);
    assert.equal(cell.noData, false);
    // Default minSample (5) makes this insufficient, which is the documented
    // behavior — the cell still reports its metrics, just flagged.
    assert.equal(cell.insufficientSample, true);
  });

  it('groups turns missing activity under "unclassified"', async () => {
    const pricing = await loadBuiltinPricing();
    await appendTurns([
      fakeTurn({ messageId: uid('m-u1'), sessionId: uid('s-1'), model: 'claude-sonnet-4-6' }),
      fakeTurn({ messageId: uid('m-u2'), sessionId: uid('s-2'), model: 'claude-sonnet-4-6' }),
    ]);
    await buildArchive();

    const result = await compareFromArchive({}, { pricing });
    assert.ok(result.table.categories.includes('unclassified'));
    assert.equal(result.table.cells['claude-sonnet-4-6']!['unclassified']!.turns, 2);
  });

  it('Codex turns bill reasoning as included_in_output (parity with costForTurn)', async () => {
    // Regression guard for the source-aware reasoning override: Codex's
    // output_tokens already includes reasoning, so reasoning_tokens must NOT
    // be billed separately. compareFromArchive groups on `source` to apply
    // this per-row before folding into the cell. If the override regressed,
    // archive-path totalCost would diverge from in-memory costForTurn for
    // any Codex turn with reasoning_tokens > 0.
    const pricing = await loadBuiltinPricing();
    await appendTurns([
      fakeTurn({
        messageId: uid('m-cx'),
        sessionId: uid('s-cx'),
        source: 'codex',
        model: 'gpt-5-codex',
        activity: 'coding',
        hasEdits: true,
        retries: 0,
        usage: {
          input: 10000,
          output: 2000,
          reasoning: 800,
          cacheRead: 0,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }),
    ]);
    await buildArchive();

    const opts: CompareOptions = { pricing };
    const inMemoryTurns = await queryAll({});
    const inMemory = buildCompareTable(inMemoryTurns, opts);
    const fromArchive = await compareFromArchive({}, opts);

    const expected = inMemory.cells['gpt-5-codex']?.['coding']?.totalCost ?? 0;
    const got = fromArchive.table.cells['gpt-5-codex']?.['coding']?.totalCost ?? 0;
    assertNumNear(got, expected, 'Codex reasoning-mode parity');
  });
});

// Numeric near-equality that handles `null` symmetrically. We use a generous
// epsilon (1e-9) because both paths sum floats but in different orders, so
// strict equality is brittle at the cent fraction.
function assertNumNear(a: number | null, b: number | null, msg: string): void {
  if (a === null || b === null) {
    assert.equal(a, b, msg);
    return;
  }
  assert.ok(Math.abs(a - b) < 1e-9, `${msg}: ${a} != ${b}`);
}
