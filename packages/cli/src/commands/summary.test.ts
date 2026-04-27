import { strict as assert } from 'node:assert';
import { mkdtemp, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, beforeEach, describe, it } from 'node:test';

import {
  appendTurns,
  archivePath,
  stamp,
  __resetIndexCacheForTesting,
} from '@relayburn/ledger';
import type { Fidelity, TurnRecord } from '@relayburn/reader';

import { runSummary } from './summary.js';

function fakeTurn(overrides: Partial<TurnRecord> = {}): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId: 's-1',
    messageId: 'msg-1',
    turnIndex: 0,
    ts: '2026-04-20T00:00:00.000Z',
    model: 'claude-sonnet-4-6',
    usage: {
      input: 1000,
      output: 500,
      reasoning: 0,
      cacheRead: 1000,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
    project: '/tmp/project',
    ...overrides,
  };
}

interface CapturedOutput {
  stdout: string;
  stderr: string;
  code: number;
}

async function captureSummary(
  flags: Record<string, string | true> = {},
): Promise<CapturedOutput> {
  const origStdout = process.stdout.write.bind(process.stdout);
  const origStderr = process.stderr.write.bind(process.stderr);
  let stdout = '';
  let stderr = '';
  process.stdout.write = ((chunk: string | Uint8Array): boolean => {
    stdout += typeof chunk === 'string' ? chunk : chunk.toString();
    return true;
  }) as typeof process.stdout.write;
  process.stderr.write = ((chunk: string | Uint8Array): boolean => {
    stderr += typeof chunk === 'string' ? chunk : chunk.toString();
    return true;
  }) as typeof process.stderr.write;
  let code: number;
  try {
    code = await runSummary({ flags, tags: {}, positional: [], passthrough: [] });
  } finally {
    process.stdout.write = origStdout;
    process.stderr.write = origStderr;
  }
  return { stdout, stderr, code };
}

describe('burn summary archive integration (#82)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalArchive = process.env['RELAYBURN_ARCHIVE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-summary-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-summary-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    delete process.env['RELAYBURN_ARCHIVE'];
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalArchive !== undefined) process.env['RELAYBURN_ARCHIVE'] = originalArchive;
    else delete process.env['RELAYBURN_ARCHIVE'];
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  it('--json output is identical between archive and ledger paths (parity)', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-A', messageId: 'pa-1' }),
      fakeTurn({
        sessionId: 's-A',
        messageId: 'pa-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
      fakeTurn({
        sessionId: 's-B',
        messageId: 'pa-3',
        ts: '2026-04-20T00:02:00.000Z',
        model: 'claude-haiku-4-5',
        project: '/tmp/other',
      }),
    ]);
    await stamp({ sessionId: 's-A' }, { workflowId: 'wf-parity' });

    // Default path: builds the archive, then queries SQL.
    const archiveOut = await captureSummary({ json: true });
    assert.equal(archiveOut.code, 0);

    // Fallback path: streams the ledger.
    const ledgerOut = await captureSummary({ json: true, 'no-archive': true });
    assert.equal(ledgerOut.code, 0);

    interface SummaryPayload {
      turns: number;
      totalCost: { total: number };
      byModel: Array<{ model: string; turns: number; usage: Record<string, number>; cost: { total: number } }>;
      fidelity: unknown;
    }
    const archive = JSON.parse(archiveOut.stdout) as SummaryPayload;
    const ledger = JSON.parse(ledgerOut.stdout) as SummaryPayload;
    assert.equal(archive.turns, ledger.turns);
    assert.equal(archive.turns, 3);
    assert.deepEqual(
      archive.byModel.map((r) => ({ model: r.model, turns: r.turns, usage: r.usage, cost: r.cost })),
      ledger.byModel.map((r) => ({ model: r.model, turns: r.turns, usage: r.usage, cost: r.cost })),
    );
    assert.deepEqual(archive.totalCost, ledger.totalCost);
    assert.deepEqual(archive.fidelity, ledger.fidelity);
  });

  it('default path auto-builds archive.sqlite on first run', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-AB', messageId: 'ab-1' })]);
    // Pre-condition: no archive on disk.
    await assert.rejects(stat(archivePath()), /ENOENT/);

    const out = await captureSummary({ json: true });
    assert.equal(out.code, 0);

    // Post-condition: `loadTurns` ran `buildArchive()` and the file exists.
    const st = await stat(archivePath());
    assert.equal(st.isFile(), true);
  });

  it('--no-archive flag does NOT build the archive (fallback path)', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-NA', messageId: 'na-1' })]);
    await assert.rejects(stat(archivePath()), /ENOENT/);

    const out = await captureSummary({ json: true, 'no-archive': true });
    assert.equal(out.code, 0);

    // The archive should still be missing — we hit the legacy `queryAll` path.
    await assert.rejects(stat(archivePath()), /ENOENT/);
  });

  it('RELAYBURN_ARCHIVE=0 env disables the archive path (fallback)', async () => {
    await appendTurns([fakeTurn({ sessionId: 's-ENV', messageId: 'env-1' })]);
    await assert.rejects(stat(archivePath()), /ENOENT/);

    process.env['RELAYBURN_ARCHIVE'] = '0';
    try {
      const out = await captureSummary({ json: true });
      assert.equal(out.code, 0);
    } finally {
      delete process.env['RELAYBURN_ARCHIVE'];
    }
    // Same fallback behavior — no archive built.
    await assert.rejects(stat(archivePath()), /ENOENT/);
  });

  it('text output matches between archive and ledger paths (parity)', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 's-T', messageId: 'tx-1' }),
      fakeTurn({
        sessionId: 's-T',
        messageId: 'tx-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);

    const archiveOut = await captureSummary({});
    assert.equal(archiveOut.code, 0);
    const ledgerOut = await captureSummary({ 'no-archive': true });
    assert.equal(ledgerOut.code, 0);

    // The "ingested N new sessions (+M turns)" preamble depends on the live
    // ingest pass which is a no-op here (no ~/.claude or ~/.codex sessions in
    // the temp HOME), but stripping the preamble keeps the test resilient if
    // that contract ever changes. Compare the body — model table + total
    // cost.
    const stripPreamble = (s: string): string => {
      const idx = s.indexOf('turns analyzed:');
      return idx >= 0 ? s.slice(idx) : s;
    };
    assert.equal(stripPreamble(archiveOut.stdout), stripPreamble(ledgerOut.stdout));
  });
});

describe('burn summary per-cell fidelity (#136)', () => {
  let tmpHome: string;
  let tmpRelay: string;
  const originalHome = process.env['HOME'];
  const originalRelay = process.env['RELAYBURN_HOME'];
  const originalArchive = process.env['RELAYBURN_ARCHIVE'];

  beforeEach(async () => {
    tmpHome = await mkdtemp(path.join(tmpdir(), 'burn-summary-cellfid-home-'));
    tmpRelay = await mkdtemp(path.join(tmpdir(), 'burn-summary-cellfid-relay-'));
    process.env['HOME'] = tmpHome;
    process.env['RELAYBURN_HOME'] = tmpRelay;
    // Force the legacy ledger-walk path so the per-cell counters reflect the
    // exact turns we appended; archive-backed pricing/coverage is exercised
    // independently in the parity test above.
    process.env['RELAYBURN_ARCHIVE'] = '0';
    // The ledger's index-sidecar cache is module-level. Earlier suites
    // populate it against their own tmpRelay; without resetting it we'd
    // dedup against stale content fingerprints (same default ts + usage as
    // the parity test → silent skip in `appendTurns`).
    __resetIndexCacheForTesting();
  });

  after(async () => {
    if (originalHome !== undefined) process.env['HOME'] = originalHome;
    else delete process.env['HOME'];
    if (originalRelay !== undefined) process.env['RELAYBURN_HOME'] = originalRelay;
    else delete process.env['RELAYBURN_HOME'];
    if (originalArchive !== undefined) process.env['RELAYBURN_ARCHIVE'] = originalArchive;
    else delete process.env['RELAYBURN_ARCHIVE'];
    await rm(tmpHome, { recursive: true, force: true });
    await rm(tmpRelay, { recursive: true, force: true });
  });

  function fullFidelity(): Fidelity {
    return {
      granularity: 'per-turn',
      coverage: {
        hasInputTokens: true,
        hasOutputTokens: true,
        hasReasoningTokens: true,
        hasCacheReadTokens: true,
        hasCacheCreateTokens: true,
        hasToolCalls: true,
        hasToolResultEvents: true,
        hasSessionRelationships: true,
        hasRawContent: true,
      },
      class: 'full',
    };
  }

  function partialMissingOutput(): Fidelity {
    const f = fullFidelity();
    return {
      ...f,
      coverage: { ...f.coverage, hasOutputTokens: false },
      class: 'partial',
    };
  }

  function aggregateNoOutputOrReasoning(): Fidelity {
    const f = fullFidelity();
    return {
      ...f,
      granularity: 'per-session-aggregate',
      coverage: {
        ...f.coverage,
        hasOutputTokens: false,
        hasReasoningTokens: false,
      },
      class: 'aggregate-only',
    };
  }

  it('renders no marker and no footer when every turn is full fidelity', async () => {
    await appendTurns([
      fakeTurn({ sessionId: 'fc-1', messageId: 'fc-1', fidelity: fullFidelity() }),
      fakeTurn({
        sessionId: 'fc-1',
        messageId: 'fc-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        fidelity: fullFidelity(),
      }),
    ]);
    const out = await captureSummary({});
    assert.equal(out.code, 0);
    // No `*` partial marker on any cell, no footer line.
    assert.equal(out.stdout.includes('*'), false, 'no partial marker should appear');
    assert.equal(
      out.stdout.includes('partial coverage:'),
      false,
      'no partial-coverage footer for all-full slice',
    );
  });

  it('renders `—` (never `0`) for a field every turn omitted', async () => {
    // Two turns, both with output omitted from upstream. Pricing layer would
    // happily report `output: 0`, but the per-cell counter says "knew about
    // 0 of 2 turns" → render the dash sentinel instead of `0`.
    await appendTurns([
      fakeTurn({
        sessionId: 'mz-1',
        messageId: 'mz-1',
        usage: {
          input: 1000,
          output: 0,
          reasoning: 0,
          cacheRead: 1000,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
        fidelity: {
          granularity: 'per-turn',
          coverage: {
            hasInputTokens: true,
            hasOutputTokens: false,
            hasReasoningTokens: false,
            hasCacheReadTokens: true,
            hasCacheCreateTokens: false,
            hasToolCalls: false,
            hasToolResultEvents: false,
            hasSessionRelationships: false,
            hasRawContent: false,
          },
          class: 'partial',
        },
      }),
      fakeTurn({
        sessionId: 'mz-1',
        messageId: 'mz-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        usage: {
          input: 2000,
          output: 0,
          reasoning: 0,
          cacheRead: 1500,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
        fidelity: {
          granularity: 'per-turn',
          coverage: {
            hasInputTokens: true,
            hasOutputTokens: false,
            hasReasoningTokens: false,
            hasCacheReadTokens: true,
            hasCacheCreateTokens: false,
            hasToolCalls: false,
            hasToolResultEvents: false,
            hasSessionRelationships: false,
            hasRawContent: false,
          },
          class: 'partial',
        },
      }),
    ]);
    const out = await captureSummary({});
    assert.equal(out.code, 0);
    // Find the model row and assert the output column rendered as `—`,
    // never literal `0`.
    const modelLine = out.stdout
      .split('\n')
      .find((l) => l.includes('claude-sonnet-4-6'));
    assert.ok(modelLine, 'expected a model row in summary output');
    // Expect a `—` somewhere on the row (output and reasoning + cacheCreate
    // are all fully missing in this fixture).
    assert.ok(modelLine!.includes('—'), `expected a — in row: ${modelLine}`);
  });

  it('marks mixed cells with `*` and prints a single footer note', async () => {
    // One full-fidelity turn + one partial (missing output) for the same
    // model. The output column should carry the value with `*` and the
    // footer should appear exactly once.
    await appendTurns([
      fakeTurn({
        sessionId: 'mx-1',
        messageId: 'mx-1',
        fidelity: fullFidelity(),
      }),
      fakeTurn({
        sessionId: 'mx-1',
        messageId: 'mx-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        fidelity: partialMissingOutput(),
      }),
    ]);
    const out = await captureSummary({});
    assert.equal(out.code, 0);
    assert.ok(
      out.stdout.includes('*'),
      'expected a * partial marker on at least one cell',
    );
    const footerMatches = out.stdout.match(/\* partial coverage:/g) ?? [];
    assert.equal(footerMatches.length, 1, 'expected exactly one partial-coverage footer');
    // Denominator should be 2 (we appended 2 turns).
    assert.match(out.stdout, /partial coverage: \d+ of 2 turns/);
  });

  it('--json emits a fidelity block with summary + perCell', async () => {
    await appendTurns([
      fakeTurn({
        sessionId: 'js-1',
        messageId: 'js-1',
        fidelity: fullFidelity(),
      }),
      fakeTurn({
        sessionId: 'js-1',
        messageId: 'js-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
        fidelity: aggregateNoOutputOrReasoning(),
      }),
    ]);
    const out = await captureSummary({ json: true });
    assert.equal(out.code, 0);
    interface Payload {
      fidelity: {
        summary: { total: number; missingCoverage: Record<string, number> };
        perCell: {
          groupBy: string;
          cells: Array<{
            label: string;
            partial: boolean;
            fields: Record<string, { known: number; missing: number }>;
          }>;
        };
      };
    }
    const payload = JSON.parse(out.stdout) as Payload;
    // Summary shape: 2 turns, 1 missing output, 1 missing reasoning.
    assert.equal(payload.fidelity.summary.total, 2);
    assert.equal(payload.fidelity.summary.missingCoverage['hasOutputTokens'], 1);
    assert.equal(payload.fidelity.summary.missingCoverage['hasReasoningTokens'], 1);
    // perCell shape: one row keyed by model, partial=true, output known=1/missing=1.
    assert.equal(payload.fidelity.perCell.groupBy, 'model');
    assert.equal(payload.fidelity.perCell.cells.length, 1);
    const cell = payload.fidelity.perCell.cells[0]!;
    assert.equal(cell.partial, true);
    assert.deepEqual(cell.fields['output'], { known: 1, missing: 1 });
    assert.deepEqual(cell.fields['input'], { known: 2, missing: 0 });
  });

  it('treats records with no fidelity field as best-effort full (no partial marker)', async () => {
    // Pre-#41 records (no `fidelity` at all). They should be counted as
    // `known` for every field — so no partial marker, no footer.
    await appendTurns([
      fakeTurn({ sessionId: 'pf-1', messageId: 'pf-1' }),
      fakeTurn({
        sessionId: 'pf-1',
        messageId: 'pf-2',
        turnIndex: 1,
        ts: '2026-04-20T00:01:00.000Z',
      }),
    ]);
    const out = await captureSummary({});
    assert.equal(out.code, 0);
    assert.equal(out.stdout.includes('*'), false);
    assert.equal(out.stdout.includes('partial coverage:'), false);
  });
});
