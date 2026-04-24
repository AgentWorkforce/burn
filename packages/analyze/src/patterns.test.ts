import { strict as assert } from 'node:assert';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import { describe, it } from 'node:test';

import { parseClaudeSession } from '@relayburn/reader';
import type { CompactionEvent, ToolCall, TurnRecord } from '@relayburn/reader';

import { detectPatterns } from './patterns.js';
import { loadBuiltinPricing } from './pricing.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.resolve(__dirname, '..', '..', '..', 'tests', 'fixtures', 'claude');

function tc(
  id: string,
  name: string,
  argsHash: string,
  opts: Partial<ToolCall> = {},
): ToolCall {
  return { id, name, argsHash, ...opts };
}

function turn(overrides: Partial<TurnRecord> & { sessionId: string; messageId: string; turnIndex: number }): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    model: 'claude-sonnet-4-6',
    ts: '2026-04-20T00:00:00.000Z',
    usage: { input: 10, output: 5, reasoning: 0, cacheRead: 100, cacheCreate5m: 50, cacheCreate1h: 0 },
    toolCalls: [],
    ...overrides,
  };
}

describe('detectPatterns — retry loops', () => {
  it('reports one retry-loop of length 4 for 4 consecutive identical failing Bash calls', async () => {
    const pricing = await loadBuiltinPricing();
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'retry-loop.jsonl'));
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.retryLoops.length, 1);
    const loop = result.retryLoops[0]!;
    assert.equal(loop.tool, 'Bash');
    assert.equal(loop.attempts, 4);
    assert.equal(loop.startTurnIndex, 0);
    assert.equal(loop.endTurnIndex, 3);
    assert.ok(loop.cost > 0, 'cost should be nonzero (sum of retry-turn costs)');
  });

  it('does not trigger on 2 consecutive failures (below threshold)', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({ sessionId: 's', messageId: 'm1', turnIndex: 0, toolCalls: [tc('u1', 'Bash', 'abc', { isError: true })] }),
      turn({ sessionId: 's', messageId: 'm2', turnIndex: 1, toolCalls: [tc('u2', 'Bash', 'abc', { isError: true })] }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.retryLoops.length, 0);
  });

  it('resets the streak when an intervening non-errored call breaks it', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({ sessionId: 's', messageId: 'm1', turnIndex: 0, toolCalls: [tc('u1', 'Bash', 'abc', { isError: true })] }),
      turn({ sessionId: 's', messageId: 'm2', turnIndex: 1, toolCalls: [tc('u2', 'Bash', 'abc', { isError: true })] }),
      // Success here breaks the streak.
      turn({ sessionId: 's', messageId: 'm3', turnIndex: 2, toolCalls: [tc('u3', 'Bash', 'abc')] }),
      turn({ sessionId: 's', messageId: 'm4', turnIndex: 3, toolCalls: [tc('u4', 'Bash', 'abc', { isError: true })] }),
      turn({ sessionId: 's', messageId: 'm5', turnIndex: 4, toolCalls: [tc('u5', 'Bash', 'abc', { isError: true })] }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.retryLoops.length, 0);
  });
});

describe('detectPatterns — consecutive failure runs', () => {
  it('reports 3 distinct failing tools in sequence as one failure run', async () => {
    const pricing = await loadBuiltinPricing();
    const { turns } = await parseClaudeSession(
      path.join(FIXTURES, 'consecutive-failures.jsonl'),
    );
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.failureRuns.length, 1);
    const run = result.failureRuns[0]!;
    assert.equal(run.length, 3);
    assert.deepEqual(run.toolsInvolved.sort(), ['Bash', 'Grep', 'Read']);
  });

  it('does NOT trigger when a mixed success/failure sequence breaks the streak', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({ sessionId: 's', messageId: 'm1', turnIndex: 0, toolCalls: [tc('u1', 'Bash', 'a', { isError: true })] }),
      turn({ sessionId: 's', messageId: 'm2', turnIndex: 1, toolCalls: [tc('u2', 'Read', 'b')] }),
      turn({ sessionId: 's', messageId: 'm3', turnIndex: 2, toolCalls: [tc('u3', 'Grep', 'c', { isError: true })] }),
      turn({ sessionId: 's', messageId: 'm4', turnIndex: 3, toolCalls: [tc('u4', 'Glob', 'd', { isError: true })] }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.failureRuns.length, 0);
    assert.equal(result.retryLoops.length, 0);
  });

  it('does not double-report a retry loop as a failure run', async () => {
    const pricing = await loadBuiltinPricing();
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'retry-loop.jsonl'));
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.retryLoops.length, 1, 'retry loop reported');
    assert.equal(result.failureRuns.length, 0, 'same-key streak is NOT a failure run');
  });
});

describe('detectPatterns — compaction losses', () => {
  it('prices the compaction against the preceding turn cacheRead', async () => {
    const pricing = await loadBuiltinPricing();
    const { turns, events } = await parseClaudeSession(
      path.join(FIXTURES, 'compact-boundary.jsonl'),
    );
    const result = detectPatterns(turns, { pricing, compactions: events });
    assert.equal(result.compactions.length, 1);
    const c = result.compactions[0]!;
    assert.equal(c.tokensBeforeCompact, 9000);
    assert.equal(c.precedingMessageId, 'msg_c_1');
    assert.ok(c.cacheLostCost > 0, 'cost must be nonzero when tokens > 0 and pricing known');
  });
});

describe('detectPatterns — edit reverts', () => {
  it('detects a two-edit cycle where edit B reverts edit A on the same file', async () => {
    const pricing = await loadBuiltinPricing();
    const { turns } = await parseClaudeSession(path.join(FIXTURES, 'edit-revert.jsonl'));
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editReverts.length, 1);
    const c = result.editReverts[0]!;
    assert.equal(c.filePath, '/src/foo.ts');
    assert.equal(c.firstEditTurnIndex, 0);
    assert.ok(c.revertTurnIndex > c.firstEditTurnIndex);
    assert.equal(c.spanTurns, c.revertTurnIndex - c.firstEditTurnIndex);
  });

  it('does not trigger when the two edits are on DIFFERENT files', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        toolCalls: [
          tc('u1', 'Edit', 'h1', {
            target: '/a.ts',
            editPreHash: 'hashA',
            editPostHash: 'hashB',
          }),
        ],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        toolCalls: [
          tc('u2', 'Edit', 'h2', {
            target: '/b.ts',
            editPreHash: 'hashB',
            editPostHash: 'hashA',
          }),
        ],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editReverts.length, 0);
  });

  it('on A→B→C→A chain, detects the A↔C reversion (post of final matches pre of first)', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        toolCalls: [
          tc('u1', 'Edit', 'h1', {
            target: '/f.ts',
            editPreHash: 'A',
            editPostHash: 'B',
          }),
        ],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        toolCalls: [
          tc('u2', 'Edit', 'h2', {
            target: '/f.ts',
            editPreHash: 'B',
            editPostHash: 'C',
          }),
        ],
      }),
      turn({
        sessionId: 's',
        messageId: 'm3',
        turnIndex: 2,
        toolCalls: [
          tc('u3', 'Edit', 'h3', {
            target: '/f.ts',
            editPreHash: 'C',
            editPostHash: 'A',
          }),
        ],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editReverts.length, 1);
    const c = result.editReverts[0]!;
    // Anchors: first edit (preHash=A) at turn 0, revert at turn 2 (postHash=A).
    assert.equal(c.firstEditTurnIndex, 0);
    assert.equal(c.revertTurnIndex, 2);
  });
});

describe('detectPatterns — session summary rollup', () => {
  it('aggregates retry/failure/compaction/revert counts per session', async () => {
    const pricing = await loadBuiltinPricing();
    const { turns: retryTurns } = await parseClaudeSession(path.join(FIXTURES, 'retry-loop.jsonl'));
    const { turns: revertTurns } = await parseClaudeSession(
      path.join(FIXTURES, 'edit-revert.jsonl'),
    );
    const { events: compactEvents } = await parseClaudeSession(
      path.join(FIXTURES, 'compact-boundary.jsonl'),
    );

    const allTurns = [...retryTurns, ...revertTurns];
    const result = detectPatterns(allTurns, { pricing, compactions: compactEvents });

    const retrySummary = result.sessionSummaries.find((s) => s.sessionId === 'retry-session')!;
    assert.equal(retrySummary.retryLoopCount, 1);
    assert.equal(retrySummary.totalRetries, 4);

    const revertSummary = result.sessionSummaries.find((s) => s.sessionId === 'revert-session')!;
    assert.equal(revertSummary.editRevertCount, 1);

    const compactSummary = result.sessionSummaries.find(
      (s) => s.sessionId === 'compact-session',
    );
    assert.ok(compactSummary, 'compaction-only session still appears in summary');
    assert.equal(compactSummary!.compactionCount, 1);
  });
});

describe('detectPatterns — defensive', () => {
  it('returns empty results when no turns and no events', async () => {
    const pricing = await loadBuiltinPricing();
    const result = detectPatterns([], { pricing, compactions: [] as CompactionEvent[] });
    assert.deepEqual(result.retryLoops, []);
    assert.deepEqual(result.failureRuns, []);
    assert.deepEqual(result.compactions, []);
    assert.deepEqual(result.editReverts, []);
    assert.deepEqual(result.sessionSummaries, []);
  });
});
