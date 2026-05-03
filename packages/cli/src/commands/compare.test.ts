import { strict as assert } from 'node:assert';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { afterEach, beforeEach, describe, it } from 'node:test';

import { loadBuiltinPricing } from '@relayburn/analyze';
import { __resetIndexCacheForTesting, appendTurns, type EnrichedTurn } from '@relayburn/ledger';
import {
  EMPTY_COVERAGE,
  makeFidelity,
} from '@relayburn/reader';
import type { ActivityCategory, Fidelity, TurnRecord } from '@relayburn/reader';

import { compare as sdkCompare } from '@relayburn/sdk';

import { runCompare, type CompareDeps } from './compare.js';
import type { ParsedArgs } from '../args.js';

async function captureStdout<T>(
  fn: () => Promise<T>,
): Promise<{ result: T; stdout: string; stderr: string }> {
  let stdout = '';
  let stderr = '';
  const origOut = process.stdout.write.bind(process.stdout);
  const origErr = process.stderr.write.bind(process.stderr);
  // node:test pipes diagnostic frames through process.stdout. Pass anything
  // that isn't a plain string straight through to the original sink so the
  // test runner's V8-serialized event traffic still reaches the reporter.
  process.stdout.write = ((c: string | Uint8Array, ...rest: unknown[]) => {
    if (typeof c === 'string') {
      stdout += c;
      return true;
    }
    return origOut(c as Uint8Array, ...(rest as []));
  }) as typeof process.stdout.write;
  process.stderr.write = ((c: string | Uint8Array, ...rest: unknown[]) => {
    if (typeof c === 'string') {
      stderr += c;
      return true;
    }
    return origErr(c as Uint8Array, ...(rest as []));
  }) as typeof process.stderr.write;
  try {
    const result = await fn();
    return { result, stdout, stderr };
  } finally {
    process.stdout.write = origOut;
    process.stderr.write = origErr;
  }
}

// Default positional that satisfies the >=2 model requirement (#159). Tests
// that exercise the new validation override `positional` directly.
const DEFAULT_MODELS_POSITIONAL = ['claude-sonnet-4-6,claude-haiku-4-5'];

function args(
  flags: Record<string, string | true> = {},
  positional: string[] = DEFAULT_MODELS_POSITIONAL,
): ParsedArgs {
  return { flags, tags: {}, positional, passthrough: [] };
}

const FULL_FIDELITY: Fidelity = makeFidelity('per-turn', {
  ...EMPTY_COVERAGE,
  hasInputTokens: true,
  hasOutputTokens: true,
  hasCacheReadTokens: true,
  hasToolCalls: true,
  hasToolResultEvents: true,
  hasSessionRelationships: true,
});

const AGGREGATE_FIDELITY: Fidelity = makeFidelity('per-session-aggregate', {
  ...EMPTY_COVERAGE,
  hasInputTokens: true,
  hasOutputTokens: true,
});

const COST_ONLY_FIDELITY: Fidelity = makeFidelity('cost-only', {
  ...EMPTY_COVERAGE,
});

const PARTIAL_FIDELITY: Fidelity = makeFidelity('per-turn', {
  ...EMPTY_COVERAGE,
  hasInputTokens: true,
  // missing output / cache-read / tool events → "partial"
});

let counter = 0;

function turn(
  model: string,
  activity: ActivityCategory | undefined,
  fidelity: Fidelity | undefined,
  partial: Partial<EnrichedTurn> = {},
): EnrichedTurn {
  counter++;
  const base: EnrichedTurn = {
    v: 1,
    source: 'claude-code',
    sessionId: 's',
    messageId: `m-${counter}`,
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
    enrichment: {},
    ...(activity !== undefined ? { activity } : {}),
    ...partial,
  };
  // exactOptionalPropertyTypes — only set fidelity when defined.
  if (fidelity !== undefined) base.fidelity = fidelity;
  return base;
}

function makeDeps(turns: EnrichedTurn[]): CompareDeps {
  return {
    ingestAll: async () => undefined,
    queryAll: async () => turns,
    loadPricing: loadBuiltinPricing,
  };
}

describe('burn compare — fidelity gating', () => {
  it('excludes aggregate-only / cost-only / partial turns by default (usage-only floor)', async () => {
    const turns: EnrichedTurn[] = [
      // 5 full-fidelity Sonnet coding turns — should survive.
      ...Array.from({ length: 5 }, () =>
        turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, {
          hasEdits: true,
          retries: 0,
        }),
      ),
      // 3 aggregate-only turns from the same model+activity — must NOT
      // contaminate the average.
      ...Array.from({ length: 3 }, () =>
        turn('claude-sonnet-4-6', 'coding', AGGREGATE_FIDELITY, {
          hasEdits: true,
          retries: 0,
        }),
      ),
      // 1 cost-only and 2 partial turns — also dropped.
      turn('claude-sonnet-4-6', 'coding', COST_ONLY_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', PARTIAL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', PARTIAL_FIDELITY, { hasEdits: true, retries: 0 }),
    ];

    const { result, stdout } = await captureStdout(() =>
      runCompare(args({ json: true }), makeDeps(turns)),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.equal(parsed.analyzedTurns, 5, 'only the 5 full-fidelity turns survive the default gate');
    const cell = parsed.cells.find(
      (c: { model: string; category: string }) =>
        c.model === 'claude-sonnet-4-6' && c.category === 'coding',
    );
    assert.ok(cell);
    assert.equal(cell.turns, 5);
  });

  it('records with no fidelity field still pass the default gate (backward compat)', async () => {
    const turns: EnrichedTurn[] = [
      // Pre-#41 ledger writers don't stamp `fidelity` — keep counting them.
      ...Array.from({ length: 3 }, () =>
        turn('claude-sonnet-4-6', 'coding', undefined, { hasEdits: true, retries: 0 }),
      ),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(args({ json: true }), makeDeps(turns)),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.equal(parsed.analyzedTurns, 3);
    assert.equal(parsed.fidelity.excluded.total, 0);
  });

  it('annotates the rendered table with an "excluded N turns" coverage note', async () => {
    const turns: EnrichedTurn[] = [
      ...Array.from({ length: 4 }, () =>
        turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      ),
      turn('claude-sonnet-4-6', 'coding', AGGREGATE_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', AGGREGATE_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', COST_ONLY_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', PARTIAL_FIDELITY, { hasEdits: true, retries: 0 }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(args(), makeDeps(turns)),
    );
    assert.equal(result, 0);
    assert.match(stdout, /excluded 4 turns below usage-only fidelity/);
    assert.match(stdout, /2 aggregate-only/);
    assert.match(stdout, /1 cost-only/);
    assert.match(stdout, /1 partial/);
  });

  it('omits the excluded note when nothing was filtered', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(args(), makeDeps(turns)),
    );
    assert.equal(result, 0);
    assert.doesNotMatch(stdout, /excluded/);
  });

  it('--fidelity full strictly drops anything below full', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      // usage-only is allowed under the default but NOT under --fidelity full.
      turn('claude-sonnet-4-6', 'coding', makeFidelity('per-turn', {
        ...EMPTY_COVERAGE,
        hasInputTokens: true,
        hasOutputTokens: true,
        hasCacheReadTokens: true,
      }), { hasEdits: true, retries: 0 }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(args({ json: true, fidelity: 'full' }), makeDeps(turns)),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.equal(parsed.analyzedTurns, 1);
    assert.equal(parsed.fidelity.minimum, 'full');
    assert.equal(parsed.fidelity.excluded.total, 1);
    assert.equal(parsed.fidelity.excluded.usageOnly, 1);
  });

  it('--fidelity partial includes everything (no exclusions)', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', AGGREGATE_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', COST_ONLY_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', PARTIAL_FIDELITY, { hasEdits: true, retries: 0 }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(args({ json: true, fidelity: 'partial' }), makeDeps(turns)),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.equal(parsed.analyzedTurns, 4);
    assert.equal(parsed.fidelity.excluded.total, 0);
  });

  it('--include-partial is shorthand for --fidelity partial', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', AGGREGATE_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', COST_ONLY_FIDELITY, { hasEdits: true, retries: 0 }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(args({ json: true, 'include-partial': true }), makeDeps(turns)),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.equal(parsed.fidelity.minimum, 'partial');
    assert.equal(parsed.fidelity.excluded.total, 0);
    assert.equal(parsed.analyzedTurns, 3);
  });

  it('--include-partial together with a conflicting --fidelity exits 2', async () => {
    const { result, stderr } = await captureStdout(() =>
      runCompare(
        args({ 'include-partial': true, fidelity: 'full' }),
        makeDeps([]),
      ),
    );
    assert.equal(result, 2);
    assert.match(stderr, /--include-partial conflicts with --fidelity full/);
  });

  it('--fidelity with an unknown class exits 2', async () => {
    const { result, stderr } = await captureStdout(() =>
      runCompare(args({ fidelity: 'bogus' }), makeDeps([])),
    );
    assert.equal(result, 2);
    assert.match(stderr, /invalid --fidelity: bogus/);
  });

  it('JSON output emits a fidelity block with minimum, excluded, and summary', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', AGGREGATE_FIDELITY, { hasEdits: true, retries: 0 }),
      // unknown bucket — survives the gate, counted in summary.
      turn('claude-sonnet-4-6', 'coding', undefined, { hasEdits: true, retries: 0 }),
    ];
    const { stdout } = await captureStdout(() =>
      runCompare(args({ json: true }), makeDeps(turns)),
    );
    const parsed = JSON.parse(stdout);
    assert.ok(parsed.fidelity, 'JSON has a top-level fidelity block');
    assert.equal(parsed.fidelity.minimum, 'usage-only');
    assert.equal(parsed.fidelity.excluded.total, 1);
    assert.equal(parsed.fidelity.excluded.aggregateOnly, 1);
    // summary mirrors `summarizeFidelity` over the unfiltered slice.
    assert.equal(parsed.fidelity.summary.total, 4);
    assert.equal(parsed.fidelity.summary.byClass.full, 2);
    assert.equal(parsed.fidelity.summary.byClass['aggregate-only'], 1);
    assert.equal(parsed.fidelity.summary.unknown, 1);
  });

  it('renders "—" (not $0.00 / 0%) when a (model, activity) collapses to zero turns post-filter', async () => {
    // Sonnet has only aggregate-only turns in `coding` — under the default
    // floor every turn is dropped, the cell should render as the dash sentinel
    // and the JSON cell flips to noData=true. Haiku keeps a real cell so the
    // category survives.
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', AGGREGATE_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', AGGREGATE_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-haiku-4-5', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
    ];
    const { stdout: jsonOut } = await captureStdout(() =>
      runCompare(
        args({ json: true }, ['claude-sonnet-4-6,claude-haiku-4-5']),
        makeDeps(turns),
      ),
    );
    const parsed = JSON.parse(jsonOut);
    const sonnetCell = parsed.cells.find(
      (c: { model: string; category: string }) =>
        c.model === 'claude-sonnet-4-6' && c.category === 'coding',
    );
    assert.ok(sonnetCell);
    assert.equal(sonnetCell.turns, 0);
    assert.equal(sonnetCell.noData, true);
    assert.equal(sonnetCell.costPerTurn, null);
    assert.equal(sonnetCell.oneShotRate, null);

    const { stdout: ttyOut } = await captureStdout(() =>
      runCompare(
        args({}, ['claude-sonnet-4-6,claude-haiku-4-5']),
        makeDeps(turns),
      ),
    );
    // Find the data row for `coding` — the Sonnet half (3 sub-columns) must
    // be three em-dashes, never $0.00 / 0%. Tightening the regex so we don't
    // accidentally match real money like `$0.0035` from another row.
    const codingLine = ttyOut.split('\n').find((l) => l.startsWith('coding'));
    assert.ok(codingLine, 'expected a coding row');
    assert.match(codingLine, /—\s+—\s+—/);
    assert.doesNotMatch(codingLine, /\$0\.00\b/);
    assert.doesNotMatch(codingLine, /\b0%/);
  });

  it('singular wording when exactly one turn was excluded', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-sonnet-4-6', 'coding', AGGREGATE_FIDELITY, { hasEdits: true, retries: 0 }),
    ];
    const { stdout } = await captureStdout(() =>
      runCompare(args(), makeDeps(turns)),
    );
    assert.match(stdout, /excluded 1 turn below usage-only fidelity/);
  });
});

describe('burn compare — --provider filter', () => {
  it('filters the compare table to only synthetic-routed turns', async () => {
    // Pricing classifier maps `hf:deepseek-ai/...` and `synthetic/...` to
    // provider=synthetic. Anthropic turns fall through to source=claude-code →
    // provider=anthropic. The positional list (#159) names two synthetic
    // models so the comparison can render after the provider filter drops the
    // Anthropic turn.
    const turns: EnrichedTurn[] = [
      turn('hf:deepseek-ai/deepseek-r1', 'coding', FULL_FIDELITY, {
        hasEdits: true,
        retries: 0,
      }),
      turn('hf:deepseek-ai/deepseek-r1', 'coding', FULL_FIDELITY, {
        hasEdits: true,
        retries: 0,
      }),
      turn('synthetic/qwen-3', 'coding', FULL_FIDELITY, {
        hasEdits: true,
        retries: 0,
      }),
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, {
        hasEdits: true,
        retries: 0,
      }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(
        args(
          { json: true, provider: 'synthetic' },
          ['hf:deepseek-ai/deepseek-r1,synthetic/qwen-3'],
        ),
        makeDeps(turns),
      ),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.equal(parsed.analyzedTurns, 3);
    assert.deepEqual(
      [...parsed.models].sort(),
      ['hf:deepseek-ai/deepseek-r1', 'synthetic/qwen-3'].sort(),
    );
  });

  it('accepts a comma-separated list (matches summary parser)', async () => {
    const turns: EnrichedTurn[] = [
      turn('hf:deepseek-ai/deepseek-r1', 'coding', FULL_FIDELITY, {
        hasEdits: true,
        retries: 0,
      }),
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, {
        hasEdits: true,
        retries: 0,
      }),
      // Codex source → provider=openai → excluded by anthropic,synthetic.
      turn('gpt-5', 'coding', FULL_FIDELITY, {
        hasEdits: true,
        retries: 0,
        source: 'codex',
      }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(
        args(
          { json: true, provider: 'anthropic,synthetic' },
          ['claude-sonnet-4-6,hf:deepseek-ai/deepseek-r1'],
        ),
        makeDeps(turns),
      ),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.equal(parsed.analyzedTurns, 2);
    assert.deepEqual(
      [...parsed.models].sort(),
      ['claude-sonnet-4-6', 'hf:deepseek-ai/deepseek-r1'].sort(),
    );
  });

  it('renders the empty-table message when the filter excludes every turn', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, {
        hasEdits: true,
        retries: 0,
      }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(args({ provider: 'synthetic' }), makeDeps(turns)),
    );
    assert.equal(result, 0);
    assert.match(stdout, /no data to compare/);
  });

  it('exits 2 when --provider is passed without a value', async () => {
    const { result, stderr } = await captureStdout(() =>
      runCompare(args({ provider: true }), makeDeps([])),
    );
    assert.equal(result, 2);
    assert.match(stderr, /--provider requires a value/);
  });
});

describe('burn compare — required positional models (#159)', () => {
  it('exits 2 with the discovery message when no positional is given', async () => {
    const { result, stderr } = await captureStdout(() =>
      runCompare(args({}, []), makeDeps([])),
    );
    assert.equal(result, 2);
    assert.match(stderr, /needs at least 2 models/);
    assert.match(stderr, /burn summary --by-provider/);
  });

  it('exits 2 when the positional names only one model', async () => {
    const { result, stderr } = await captureStdout(() =>
      runCompare(args({}, ['claude-sonnet-4-6']), makeDeps([])),
    );
    assert.equal(result, 2);
    assert.match(stderr, /needs at least 2 models/);
  });

  it('exits 2 when the positional has trailing commas / empties resolving to <2', async () => {
    // " claude-sonnet-4-6 ,, " → trim/dedupe/drop-empty → 1 model.
    const { result, stderr } = await captureStdout(() =>
      runCompare(args({}, [' claude-sonnet-4-6 ,, ']), makeDeps([])),
    );
    assert.equal(result, 2);
    assert.match(stderr, /needs at least 2 models/);
  });

  it('runs and renders a table for a valid 2+ model positional', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-haiku-4-5', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(
        args({}, ['claude-sonnet-4-6,claude-haiku-4-5']),
        makeDeps(turns),
      ),
    );
    assert.equal(result, 0);
    // Both models surface in the per-model totals footer.
    assert.match(stdout, /claude-sonnet-4-6:/);
    assert.match(stdout, /claude-haiku-4-5:/);
  });

  it('JSON output works with the positional form', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-haiku-4-5', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(
        args({ json: true }, ['claude-sonnet-4-6,claude-haiku-4-5']),
        makeDeps(turns),
      ),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.deepEqual(
      [...parsed.models].sort(),
      ['claude-haiku-4-5', 'claude-sonnet-4-6'],
    );
  });

  it('CSV output works with the positional form', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-haiku-4-5', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(
        args({ csv: true }, ['claude-sonnet-4-6,claude-haiku-4-5']),
        makeDeps(turns),
      ),
    );
    assert.equal(result, 0);
    // Header + one row per (model, category) pair.
    assert.match(stdout, /^model,category,/);
    assert.match(stdout, /claude-sonnet-4-6,coding,/);
    assert.match(stdout, /claude-haiku-4-5,coding,/);
  });

  it('rejects the legacy --models flag with a pointer to the positional form', async () => {
    const { result, stderr } = await captureStdout(() =>
      runCompare(
        args({ models: 'claude-sonnet-4-6,claude-haiku-4-5' }, []),
        makeDeps([]),
      ),
    );
    assert.equal(result, 2);
    assert.match(stderr, /--models was removed/);
    assert.match(stderr, /positional argument/);
  });

  it('dedupes and trims model names in the positional', async () => {
    const turns: EnrichedTurn[] = [
      turn('claude-sonnet-4-6', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
      turn('claude-haiku-4-5', 'coding', FULL_FIDELITY, { hasEdits: true, retries: 0 }),
    ];
    const { result, stdout } = await captureStdout(() =>
      runCompare(
        args({ json: true }, [' claude-sonnet-4-6 , claude-haiku-4-5 , claude-sonnet-4-6 ']),
        makeDeps(turns),
      ),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.deepEqual(parsed.models, ['claude-sonnet-4-6', 'claude-haiku-4-5']);
  });
});

// SDK integration tests for `compare()`. These exercise the production path
// (no `deps.queryAll`) by writing fixtures to an isolated tmp ledger and
// calling `sdkCompare` directly. The CLI's runCompare wraps this with flag
// parsing + rendering; this suite covers the SDK contract those presenters
// rely on.
describe('@relayburn/sdk compare() — ledger integration', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalArchive = process.env['RELAYBURN_ARCHIVE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-compare-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-compare-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    delete process.env['RELAYBURN_ARCHIVE'];
    __resetIndexCacheForTesting();
  });

  afterEach(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalArchive !== undefined) process.env['RELAYBURN_ARCHIVE'] = originalArchive;
    else delete process.env['RELAYBURN_ARCHIVE'];
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  function fakeLedgerTurn(overrides: Partial<TurnRecord> = {}): TurnRecord {
    return {
      v: 1,
      source: 'claude-code',
      sessionId: 's-cmp',
      messageId: `m-${Math.random().toString(36).slice(2, 10)}`,
      turnIndex: 0,
      ts: '2026-04-20T00:00:00.000Z',
      model: 'claude-sonnet-4-6',
      activity: 'coding',
      hasEdits: true,
      retries: 0,
      usage: {
        input: 1000,
        output: 500,
        reasoning: 0,
        cacheRead: 0,
        cacheCreate5m: 0,
        cacheCreate1h: 0,
      },
      toolCalls: [],
      fidelity: makeFidelity('per-turn', {
        ...EMPTY_COVERAGE,
        hasInputTokens: true,
        hasOutputTokens: true,
        hasCacheReadTokens: true,
        hasToolCalls: true,
        hasToolResultEvents: true,
        hasSessionRelationships: true,
      }),
      ...overrides,
    };
  }

  it('rejects fewer than 2 models', async () => {
    await assert.rejects(
      () => sdkCompare({ models: ['claude-sonnet-4-6'] }),
      /needs at least 2 models/,
    );
  });

  it('rejects an invalid minFidelity value', async () => {
    await assert.rejects(
      () =>
        sdkCompare({
          models: ['claude-sonnet-4-6', 'claude-haiku-4-5'],
          minFidelity: 'bogus' as never,
        }),
      /invalid minFidelity/,
    );
  });

  it('returns the JSON-shaped CompareResult against an isolated ledger', async () => {
    await appendTurns([
      fakeLedgerTurn({ messageId: 'm-1' }),
      fakeLedgerTurn({ messageId: 'm-2', turnIndex: 1, ts: '2026-04-20T00:01:00.000Z' }),
      fakeLedgerTurn({
        messageId: 'm-3',
        turnIndex: 2,
        ts: '2026-04-20T00:02:00.000Z',
        model: 'claude-haiku-4-5',
      }),
    ]);

    const result = await sdkCompare({
      models: ['claude-sonnet-4-6', 'claude-haiku-4-5'],
    });
    assert.equal(result.analyzedTurns, 3);
    assert.deepEqual([...result.models].sort(), ['claude-haiku-4-5', 'claude-sonnet-4-6']);
    assert.equal(result.fidelity.minimum, 'usage-only');
    assert.equal(result.fidelity.excluded.total, 0);
    // cells is a flat array, not the nested CompareTable shape.
    assert.ok(Array.isArray(result.cells));
    const sonnet = result.cells.find(
      (c) => c.model === 'claude-sonnet-4-6' && c.category === 'coding',
    );
    assert.ok(sonnet);
    assert.equal(sonnet.turns, 2);
  });

  it('archive path (partial fidelity, no provider) returns the same shape as the ledger walk', async () => {
    await appendTurns([
      fakeLedgerTurn({ messageId: 'p-1' }),
      fakeLedgerTurn({
        messageId: 'p-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        model: 'claude-haiku-4-5',
      }),
    ]);

    const archive = await sdkCompare({
      models: ['claude-sonnet-4-6', 'claude-haiku-4-5'],
      minFidelity: 'partial',
    });
    process.env['RELAYBURN_ARCHIVE'] = '0';
    const ledger = await sdkCompare({
      models: ['claude-sonnet-4-6', 'claude-haiku-4-5'],
      minFidelity: 'partial',
    });
    delete process.env['RELAYBURN_ARCHIVE'];

    assert.equal(archive.analyzedTurns, ledger.analyzedTurns);
    assert.deepEqual(archive.totals, ledger.totals);
    assert.equal(archive.cells.length, ledger.cells.length);
  });
});
