import { strict as assert } from 'node:assert';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import { describe, it } from 'node:test';

import { parseClaudeSession } from '@relayburn/reader';
import type {
  CompactionEvent,
  ContentRecord,
  ToolCall,
  ToolResultEventRecord,
  TurnRecord,
  UserTurnRecord,
} from '@relayburn/reader';

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

function event(
  overrides: Partial<ToolResultEventRecord> & {
    sessionId: string;
    toolUseId: string;
    eventIndex: number;
    status: ToolResultEventRecord['status'];
  },
): ToolResultEventRecord {
  return {
    v: 1,
    source: 'claude-code',
    eventSource: 'tool_result',
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

  it('reports the same retry loop from ToolResultEventRecord chronology and annotates eventSource', async () => {
    const pricing = await loadBuiltinPricing();
    const { turns, toolResultEvents } = await parseClaudeSession(
      path.join(FIXTURES, 'retry-loop.jsonl'),
    );
    const legacy = detectPatterns(turns, { pricing });
    const graph = detectPatterns(turns, { pricing, toolResultEvents });
    assert.equal(graph.retryLoops.length, 1);
    assert.equal(graph.failureRuns.length, 0);
    const graphLoop = graph.retryLoops[0]!;
    assert.equal(graphLoop.eventSource, 'tool_result');
    assert.deepEqual(
      { ...graphLoop, eventSource: undefined },
      { ...legacy.retryLoops[0]!, eventSource: undefined },
    );
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

  it('mixes graph-backed sessions with legacy fallback sessions', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({ sessionId: 'graph', messageId: 'g1', turnIndex: 0, toolCalls: [tc('g1', 'Bash', 'same')] }),
      turn({ sessionId: 'graph', messageId: 'g2', turnIndex: 1, toolCalls: [tc('g2', 'Bash', 'same')] }),
      turn({ sessionId: 'graph', messageId: 'g3', turnIndex: 2, toolCalls: [tc('g3', 'Bash', 'same')] }),
      turn({ sessionId: 'fallback', messageId: 'f1', turnIndex: 0, toolCalls: [tc('f1', 'Bash', 'same', { isError: true })] }),
      turn({ sessionId: 'fallback', messageId: 'f2', turnIndex: 1, toolCalls: [tc('f2', 'Bash', 'same', { isError: true })] }),
      turn({ sessionId: 'fallback', messageId: 'f3', turnIndex: 2, toolCalls: [tc('f3', 'Bash', 'same', { isError: true })] }),
    ];
    const toolResultEvents = [
      event({ sessionId: 'graph', toolUseId: 'g1', eventIndex: 0, status: 'errored' }),
      event({ sessionId: 'graph', toolUseId: 'g2', eventIndex: 1, status: 'errored' }),
      event({ sessionId: 'graph', toolUseId: 'g3', eventIndex: 2, status: 'errored' }),
    ];

    const result = detectPatterns(turns, { pricing, toolResultEvents });
    assert.equal(result.retryLoops.length, 2);
    const bySession = new Map(result.retryLoops.map((loop) => [loop.sessionId, loop]));
    assert.equal(bySession.get('graph')!.eventSource, 'tool_result');
    assert.equal(bySession.get('fallback')!.eventSource, undefined);
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

  it('counts chained subagent_notification errors even when toolCalls have no isError flag', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({ sessionId: 's', messageId: 'm1', turnIndex: 0, toolCalls: [tc('a1', 'Agent', 'agent:one')] }),
      turn({ sessionId: 's', messageId: 'm2', turnIndex: 1, toolCalls: [tc('a2', 'Agent', 'agent:two')] }),
      turn({ sessionId: 's', messageId: 'm3', turnIndex: 2, toolCalls: [tc('a3', 'Agent', 'agent:three')] }),
    ];
    const toolResultEvents = [
      event({ sessionId: 's', toolUseId: 'a1', eventIndex: 0, status: 'errored', eventSource: 'subagent_notification' }),
      event({ sessionId: 's', toolUseId: 'a2', eventIndex: 1, status: 'errored', eventSource: 'subagent_notification' }),
      event({ sessionId: 's', toolUseId: 'a3', eventIndex: 2, status: 'errored', eventSource: 'subagent_notification' }),
    ];

    const result = detectPatterns(turns, { pricing, toolResultEvents });
    assert.equal(result.failureRuns.length, 1);
    assert.equal(result.failureRuns[0]!.length, 3);
    assert.equal(result.failureRuns[0]!.eventSource, 'subagent_notification');
  });
});

describe('detectPatterns — cancelled graph events', () => {
  it('keeps cancellations out of retry/failure detectors and reports them separately', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({ sessionId: 's', messageId: 'm1', turnIndex: 0, toolCalls: [tc('c1', 'Bash', 'same', { isError: true })] }),
      turn({ sessionId: 's', messageId: 'm2', turnIndex: 1, toolCalls: [tc('c2', 'Bash', 'same', { isError: true })] }),
      turn({ sessionId: 's', messageId: 'm3', turnIndex: 2, toolCalls: [tc('c3', 'Bash', 'same', { isError: true })] }),
    ];
    const toolResultEvents = [
      event({ sessionId: 's', toolUseId: 'c1', eventIndex: 0, status: 'cancelled' }),
      event({ sessionId: 's', toolUseId: 'c2', eventIndex: 1, status: 'cancelled' }),
      event({ sessionId: 's', toolUseId: 'c3', eventIndex: 2, status: 'cancelled' }),
    ];

    const result = detectPatterns(turns, { pricing, toolResultEvents });
    assert.equal(result.retryLoops.length, 0);
    assert.equal(result.failureRuns.length, 0);
    assert.equal(result.cancelledRuns.length, 1);
    assert.equal(result.cancelledRuns[0]!.length, 3);
    assert.equal(result.cancelledRuns[0]!.eventSource, 'tool_result');
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
    assert.deepEqual(result.cancelledRuns, []);
    assert.deepEqual(result.compactions, []);
    assert.deepEqual(result.editReverts, []);
    assert.deepEqual(result.sessionSummaries, []);
  });
});

describe('detectPatterns — OpenCode skill recall duplicates', () => {
  it('detects repeated skill calls with the same name in an OpenCode session', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'opencode' as const,
        toolCalls: [tc('u1', 'skill', 'h1', { skillName: 'react-component' })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        source: 'opencode' as const,
        toolCalls: [tc('u2', 'skill', 'h2', { skillName: 'react-component' })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm3',
        turnIndex: 2,
        source: 'opencode' as const,
        toolCalls: [tc('u3', 'skill', 'h3', { skillName: 'react-component' })],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.skillRecallDups.length, 1);
    const dup = result.skillRecallDups[0]!;
    assert.equal(dup.skillName, 'react-component');
    assert.equal(dup.callCount, 3);
    assert.equal(dup.firstTurnIndex, 0);
    assert.equal(dup.lastTurnIndex, 2);
  });

  it('does not trigger on a single skill call', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'opencode' as const,
        toolCalls: [tc('u1', 'skill', 'h1', { skillName: 'react-component' })],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.skillRecallDups.length, 0);
  });

  it('does not trigger for non-OpenCode sessions (Claude Code)', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'claude-code' as const,
        toolCalls: [tc('u1', 'skill', 'h1', { skillName: 'react-component' })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        source: 'claude-code' as const,
        toolCalls: [tc('u2', 'skill', 'h2', { skillName: 'react-component' })],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.skillRecallDups.length, 0);
  });

  it('groups different skill names separately', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'opencode' as const,
        toolCalls: [tc('u1', 'skill', 'h1', { skillName: 'react-component' })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        source: 'opencode' as const,
        toolCalls: [tc('u2', 'skill', 'h2', { skillName: 'react-component' })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm3',
        turnIndex: 2,
        source: 'opencode' as const,
        toolCalls: [tc('u3', 'skill', 'h3', { skillName: 'testing' })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm4',
        turnIndex: 3,
        source: 'opencode' as const,
        toolCalls: [tc('u4', 'skill', 'h4', { skillName: 'testing' })],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.skillRecallDups.length, 2);
    const react = result.skillRecallDups.find((d) => d.skillName === 'react-component')!;
    const testing = result.skillRecallDups.find((d) => d.skillName === 'testing')!;
    assert.ok(react, 'react-component dup found');
    assert.ok(testing, 'testing dup found');
    assert.equal(react.callCount, 2);
    assert.equal(testing.callCount, 2);
  });
});

describe('detectPatterns — OpenCode skill pruning protection', () => {
  it('detects skill content riding in cache after invocation', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'opencode' as const,
        usage: { input: 100, output: 50, reasoning: 0, cacheRead: 0, cacheCreate5m: 200, cacheCreate1h: 0 },
        toolCalls: [tc('u1', 'skill', 'h1', { skillName: 'react-component' })],
      }),
      // Subsequent turns with cacheRead — skill content is riding
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        source: 'opencode' as const,
        usage: { input: 50, output: 30, reasoning: 0, cacheRead: 300, cacheCreate5m: 0, cacheCreate1h: 0 },
        toolCalls: [],
      }),
      turn({
        sessionId: 's',
        messageId: 'm3',
        turnIndex: 2,
        source: 'opencode' as const,
        usage: { input: 50, output: 30, reasoning: 0, cacheRead: 350, cacheCreate5m: 0, cacheCreate1h: 0 },
        toolCalls: [],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.skillPruningProtection.length, 1);
    const ev = result.skillPruningProtection[0]!;
    assert.equal(ev.skillName, 'react-component');
    assert.equal(ev.invokedTurnIndex, 0);
    assert.equal(ev.ridingTurns, 2);
    assert.equal(ev.lastCachedTurnIndex, 2);
  });

  it('does not emit when there are no subsequent cacheRead turns', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'opencode' as const,
        usage: { input: 100, output: 50, reasoning: 0, cacheRead: 0, cacheCreate5m: 200, cacheCreate1h: 0 },
        toolCalls: [tc('u1', 'skill', 'h1', { skillName: 'react-component' })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        source: 'opencode' as const,
        usage: { input: 50, output: 30, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
        toolCalls: [],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.skillPruningProtection.length, 0);
  });

  it('does not trigger for non-OpenCode sessions', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'claude-code' as const,
        usage: { input: 100, output: 50, reasoning: 0, cacheRead: 0, cacheCreate5m: 200, cacheCreate1h: 0 },
        toolCalls: [tc('u1', 'skill', 'h1', { skillName: 'react-component' })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        source: 'claude-code' as const,
        usage: { input: 50, output: 30, reasoning: 0, cacheRead: 300, cacheCreate5m: 0, cacheCreate1h: 0 },
        toolCalls: [],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.skillPruningProtection.length, 0);
  });
});

describe('detectPatterns — session summary includes skill detectors', () => {
  it('aggregates skill recall dup and pruning protection counts', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'opencode' as const,
        usage: { input: 100, output: 50, reasoning: 0, cacheRead: 0, cacheCreate5m: 200, cacheCreate1h: 0 },
        toolCalls: [tc('u1', 'skill', 'h1', { skillName: 'react-component' })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        source: 'opencode' as const,
        usage: { input: 100, output: 50, reasoning: 0, cacheRead: 0, cacheCreate5m: 200, cacheCreate1h: 0 },
        toolCalls: [tc('u2', 'skill', 'h2', { skillName: 'react-component' })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm3',
        turnIndex: 2,
        source: 'opencode' as const,
        usage: { input: 50, output: 30, reasoning: 0, cacheRead: 300, cacheCreate5m: 0, cacheCreate1h: 0 },
        toolCalls: [],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    const summary = result.sessionSummaries.find((s) => s.sessionId === 's');
    assert.ok(summary, 'summary exists');
    assert.equal(summary!.skillRecallDupCount, 1);
    // Both skill calls have a subsequent cacheRead turn, so each gets a pruning entry.
    assert.equal(summary!.skillPruningProtectionCount, 2);
  });
});

describe('detectPatterns — OpenCode system prompt tax', () => {
  function userTurn(overrides: { sessionId: string; userUuid: string; blocks: Array<{ kind: 'tool_result' | 'text'; toolUseId?: string; byteLen: number; approxTokens: number; isError?: boolean }>; precedingMessageId?: string; followingMessageId?: string }): UserTurnRecord {
    const record: UserTurnRecord = {
      v: 1,
      source: 'opencode',
      sessionId: overrides.sessionId,
      userUuid: overrides.userUuid,
      ts: '2026-04-20T00:00:00.000Z',
      blocks: overrides.blocks,
    };
    if (overrides.precedingMessageId !== undefined) record.precedingMessageId = overrides.precedingMessageId;
    if (overrides.followingMessageId !== undefined) record.followingMessageId = overrides.followingMessageId;
    return record;
  }

  it('estimates system prompt size from first cacheCreate minus first user message', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'opencode' as const,
        usage: { input: 5000, output: 200, reasoning: 0, cacheRead: 0, cacheCreate5m: 5200, cacheCreate1h: 0 },
        toolCalls: [],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        source: 'opencode' as const,
        usage: { input: 100, output: 50, reasoning: 0, cacheRead: 5200, cacheCreate5m: 0, cacheCreate1h: 0 },
        toolCalls: [],
      }),
    ];
    const userTurnsBySession = new Map<string, UserTurnRecord[]>();
    userTurnsBySession.set('s', [
      userTurn({
        sessionId: 's',
        userUuid: 'u1',
        blocks: [{ kind: 'text' as const, byteLen: 800, approxTokens: 200 }],
        followingMessageId: 'm1',
      }),
    ]);

    const result = detectPatterns(turns, { pricing, userTurnsBySession });
    assert.equal(result.systemPromptTaxes.length, 1);
    const tax = result.systemPromptTaxes[0]!;
    assert.equal(tax.firstTurnCacheCreate, 5200);
    assert.equal(tax.firstUserMessageTokens, 200);
    assert.equal(tax.estimatedSystemPromptTokens, 5000);
    assert.equal(tax.ridingTurns, 1);
  });

  it('does not emit when user-turn data is unavailable', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'opencode' as const,
        usage: { input: 5000, output: 200, reasoning: 0, cacheRead: 0, cacheCreate5m: 5200, cacheCreate1h: 0 },
        toolCalls: [],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.systemPromptTaxes.length, 0);
  });

  it('does not trigger for non-OpenCode sessions', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'claude-code' as const,
        usage: { input: 5000, output: 200, reasoning: 0, cacheRead: 0, cacheCreate5m: 5200, cacheCreate1h: 0 },
        toolCalls: [],
      }),
    ];
    const userTurnsBySession = new Map<string, UserTurnRecord[]>();
    userTurnsBySession.set('s', [
      userTurn({
        sessionId: 's',
        userUuid: 'u1',
        blocks: [{ kind: 'text' as const, byteLen: 800, approxTokens: 200 }],
        followingMessageId: 'm1',
      }),
    ]);
    const result = detectPatterns(turns, { pricing, userTurnsBySession });
    assert.equal(result.systemPromptTaxes.length, 0);
  });

  it('excludes the first turn from ridingTurns even when its cacheRead > 0 (resumed session)', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      // First turn carries cacheRead > 0 — e.g., a resumed OpenCode session
      // where the prefix was already populated. This must not be counted as
      // a riding turn; it is the establishing turn.
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'opencode' as const,
        usage: { input: 5000, output: 200, reasoning: 0, cacheRead: 4000, cacheCreate5m: 5200, cacheCreate1h: 0 },
        toolCalls: [],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 1,
        source: 'opencode' as const,
        usage: { input: 100, output: 50, reasoning: 0, cacheRead: 5200, cacheCreate5m: 0, cacheCreate1h: 0 },
        toolCalls: [],
      }),
    ];
    const userTurnsBySession = new Map<string, UserTurnRecord[]>();
    userTurnsBySession.set('s', [
      userTurn({
        sessionId: 's',
        userUuid: 'u1',
        blocks: [{ kind: 'text' as const, byteLen: 800, approxTokens: 200 }],
        followingMessageId: 'm1',
      }),
    ]);

    const result = detectPatterns(turns, { pricing, userTurnsBySession });
    assert.equal(result.systemPromptTaxes.length, 1);
    const tax = result.systemPromptTaxes[0]!;
    assert.equal(tax.ridingTurns, 1);
  });

  it('does not emit when first cacheCreate is zero', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 0,
        source: 'opencode' as const,
        usage: { input: 200, output: 100, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
        toolCalls: [],
      }),
    ];
    const userTurnsBySession = new Map<string, UserTurnRecord[]>();
    userTurnsBySession.set('s', [
      userTurn({
        sessionId: 's',
        userUuid: 'u1',
        blocks: [{ kind: 'text' as const, byteLen: 800, approxTokens: 200 }],
        followingMessageId: 'm1',
      }),
    ]);
    const result = detectPatterns(turns, { pricing, userTurnsBySession });
    assert.equal(result.systemPromptTaxes.length, 0);
  });
});

// One edit-bearing turn per element, six total — above MIN_EDITS=5.
function editHeavyTurns(source: 'claude-code' | 'codex' | 'opencode', editTool: string, sessionId = 's'): TurnRecord[] {
  const out: TurnRecord[] = [];
  for (let i = 0; i < 6; i++) {
    out.push(
      turn({
        sessionId,
        messageId: `m${i}`,
        turnIndex: i,
        source,
        toolCalls: [tc(`u${i}`, editTool, `h${i}`, { target: `/src/file${i}.ts` })],
      }),
    );
  }
  return out;
}

describe('detectPatterns — edit-heavy sessions (cross-harness)', () => {
  it('flags Claude session with 6 edits and 0 reads', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = editHeavyTurns('claude-code', 'Edit');
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editHeavySessions.length, 1);
    const r = result.editHeavySessions[0]!;
    assert.equal(r.source, 'claude-code');
    assert.equal(r.editCount, 6);
    assert.equal(r.readCount, 0);
    assert.equal(r.ratio, Number.POSITIVE_INFINITY);
  });

  it('flags OpenCode session using lowercase tool names', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = editHeavyTurns('opencode', 'edit');
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editHeavySessions.length, 1);
    assert.equal(result.editHeavySessions[0]!.source, 'opencode');
    assert.equal(result.editHeavySessions[0]!.editCount, 6);
  });

  it('flags Codex session using apply_patch (normalized to Edit)', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = editHeavyTurns('codex', 'apply_patch');
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editHeavySessions.length, 1);
    assert.equal(result.editHeavySessions[0]!.source, 'codex');
    assert.equal(result.editHeavySessions[0]!.editCount, 6);
  });

  it('does not flag a session with sufficient reads (ratio under threshold)', async () => {
    const pricing = await loadBuiltinPricing();
    // 6 edits, 3 reads → ratio 2.0 ≤ 4. Should not flag.
    const turns = [
      ...editHeavyTurns('claude-code', 'Edit'),
      turn({ sessionId: 's', messageId: 'r1', turnIndex: 6, toolCalls: [tc('r1', 'Read', 'a', { target: '/a.ts' })] }),
      turn({ sessionId: 's', messageId: 'r2', turnIndex: 7, toolCalls: [tc('r2', 'Read', 'b', { target: '/b.ts' })] }),
      turn({ sessionId: 's', messageId: 'r3', turnIndex: 8, toolCalls: [tc('r3', 'Read', 'c', { target: '/c.ts' })] }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editHeavySessions.length, 0);
  });

  it('does not flag below MIN_EDITS (4 edits, 0 reads)', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 4; i++) {
      turns.push(
        turn({
          sessionId: 's',
          messageId: `m${i}`,
          turnIndex: i,
          toolCalls: [tc(`u${i}`, 'Edit', `h${i}`, { target: `/f${i}.ts` })],
        }),
      );
    }
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editHeavySessions.length, 0);
  });

  it('grep / glob / LS / bash do NOT count as reads', async () => {
    const pricing = await loadBuiltinPricing();
    // 6 edits, plus 5 non-Read tools (Grep/Glob/LS/Bash). Ratio should still
    // be infinite — reads stay at 0.
    const turns = [
      ...editHeavyTurns('claude-code', 'Edit'),
      turn({ sessionId: 's', messageId: 'g1', turnIndex: 6, toolCalls: [tc('g1', 'Grep', 'a')] }),
      turn({ sessionId: 's', messageId: 'g2', turnIndex: 7, toolCalls: [tc('g2', 'Glob', 'b')] }),
      turn({ sessionId: 's', messageId: 'g3', turnIndex: 8, toolCalls: [tc('g3', 'LS', 'c')] }),
      turn({ sessionId: 's', messageId: 'g4', turnIndex: 9, toolCalls: [tc('g4', 'Bash', 'cat /etc/hosts')] }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editHeavySessions.length, 1);
    assert.equal(result.editHeavySessions[0]!.readCount, 0);
  });

  it('Codex read_file normalizes to Read and brings ratio under threshold', async () => {
    const pricing = await loadBuiltinPricing();
    // 6 edits + 2 read_file calls → ratio 3.0, ≤ 4, no flag.
    const turns = [
      ...editHeavyTurns('codex', 'apply_patch'),
      turn({ sessionId: 's', messageId: 'r1', turnIndex: 6, source: 'codex' as const, toolCalls: [tc('r1', 'read_file', 'a')] }),
      turn({ sessionId: 's', messageId: 'r2', turnIndex: 7, source: 'codex' as const, toolCalls: [tc('r2', 'read_file', 'b')] }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editHeavySessions.length, 0);
  });

  it('reports likelyRetries from intra-turn edit→bash→edit cycles', async () => {
    const pricing = await loadBuiltinPricing();
    // One turn with [Edit, Bash, Edit] = 1 retry. Repeat across 5 turns to
    // pass MIN_EDITS=5 (each turn contributes 2 edits → 10 edits total).
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 5; i++) {
      turns.push(
        turn({
          sessionId: 's',
          messageId: `m${i}`,
          turnIndex: i,
          toolCalls: [
            tc(`e1-${i}`, 'Edit', `h${i}a`, { target: `/f${i}.ts` }),
            tc(`b-${i}`, 'Bash', `bh${i}`, { target: 'pytest' }),
            tc(`e2-${i}`, 'Edit', `h${i}b`, { target: `/f${i}.ts` }),
          ],
        }),
      );
    }
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editHeavySessions.length, 1);
    const r = result.editHeavySessions[0]!;
    assert.equal(r.editCount, 10);
    assert.equal(r.likelyRetries, 5, 'one retry per turn × 5 turns');
  });

  it('aggregates editHeavyCount into the session summary', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = editHeavyTurns('claude-code', 'Edit');
    const result = detectPatterns(turns, { pricing });
    const summary = result.sessionSummaries.find((s) => s.sessionId === 's');
    assert.ok(summary, 'summary present');
    assert.equal(summary!.editHeavyCount, 1);
  });
});

// ---------------------------------------------------------------------------
// Content-sidecar enrichments (#57)
// ---------------------------------------------------------------------------

function toolResult(
  sessionId: string,
  messageId: string,
  toolUseId: string,
  text: string,
  ts = '2026-04-20T00:00:00.000Z',
): ContentRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId,
    messageId,
    ts,
    role: 'tool_result',
    kind: 'tool_result',
    toolResult: {
      toolUseId,
      content: text,
      isError: true,
    },
  };
}

function toolUseRec(
  sessionId: string,
  messageId: string,
  id: string,
  name: string,
  input: Record<string, unknown>,
  ts = '2026-04-20T00:00:00.000Z',
): ContentRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId,
    messageId,
    ts,
    role: 'assistant',
    kind: 'tool_use',
    toolUse: { id, name, input },
  };
}

describe('detectPatterns — RetryLoop errorSignature enrichment (#57)', () => {
  it('populates errorSignature when all attempts share a leading line', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 4; i++) {
      turns.push(
        turn({
          sessionId: 's',
          messageId: `m${i}`,
          turnIndex: i,
          toolCalls: [tc(`u${i}`, 'Bash', 'abc', { isError: true })],
        }),
      );
    }
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set('s', [
      toolResult('s', 'm0', 'u0', "npm ERR! code ENOENT\n  more details\n  more details"),
      toolResult('s', 'm1', 'u1', "npm ERR! code ENOENT\n  more details"),
      toolResult('s', 'm2', 'u2', "npm ERR! code ENOENT\n  trailing"),
      toolResult('s', 'm3', 'u3', "npm ERR! code ENOENT\n  yet again"),
    ]);
    const result = detectPatterns(turns, { pricing, contentBySession });
    assert.equal(result.retryLoops.length, 1);
    assert.equal(result.retryLoops[0]!.errorSignature, 'npm ERR! code ENOENT');
  });

  it('marks the first signature with "(signatures diverged)" when attempts diverge', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 3; i++) {
      turns.push(
        turn({
          sessionId: 's',
          messageId: `m${i}`,
          turnIndex: i,
          toolCalls: [tc(`u${i}`, 'Bash', 'abc', { isError: true })],
        }),
      );
    }
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set('s', [
      toolResult('s', 'm0', 'u0', 'npm ERR! code ENOENT'),
      toolResult('s', 'm1', 'u1', 'npm ERR! code EACCES'),
      toolResult('s', 'm2', 'u2', 'npm ERR! code ENOENT'),
    ]);
    const result = detectPatterns(turns, { pricing, contentBySession });
    assert.equal(result.retryLoops.length, 1);
    assert.equal(
      result.retryLoops[0]!.errorSignature,
      'npm ERR! code ENOENT (signatures diverged)',
    );
  });

  it('omits errorSignature when no contentBySession is supplied (graceful degradation)', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 3; i++) {
      turns.push(
        turn({
          sessionId: 's',
          messageId: `m${i}`,
          turnIndex: i,
          toolCalls: [tc(`u${i}`, 'Bash', 'abc', { isError: true })],
        }),
      );
    }
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.retryLoops.length, 1);
    assert.equal(result.retryLoops[0]!.errorSignature, undefined);
  });

  it('omits errorSignature when content has no matching tool_results for the loop', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 3; i++) {
      turns.push(
        turn({
          sessionId: 's',
          messageId: `m${i}`,
          turnIndex: i,
          toolCalls: [tc(`u${i}`, 'Bash', 'abc', { isError: true })],
        }),
      );
    }
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set('s', [
      // Content present, but for an unrelated toolUseId.
      toolResult('s', 'm99', 'unrelated', 'something else'),
    ]);
    const result = detectPatterns(turns, { pricing, contentBySession });
    assert.equal(result.retryLoops.length, 1);
    assert.equal(result.retryLoops[0]!.errorSignature, undefined);
  });
});

describe('detectPatterns — FailureRun errorSignatures enrichment (#57)', () => {
  it('records one entry per distinct tool, in first-seen order', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        toolCalls: [tc('u0', 'Bash', 'a', { isError: true })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        toolCalls: [tc('u1', 'Read', 'b', { isError: true })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 2,
        toolCalls: [tc('u2', 'Grep', 'c', { isError: true })],
      }),
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set('s', [
      toolResult('s', 'm0', 'u0', 'bash: command not found'),
      toolResult('s', 'm1', 'u1', 'ENOENT: no such file or directory'),
      toolResult('s', 'm2', 'u2', 'no matches found'),
    ]);
    const result = detectPatterns(turns, { pricing, contentBySession });
    assert.equal(result.failureRuns.length, 1);
    const sigs = result.failureRuns[0]!.errorSignatures!;
    assert.deepEqual(sigs, [
      { tool: 'Bash', firstLine: 'bash: command not found' },
      { tool: 'Read', firstLine: 'ENOENT: no such file or directory' },
      { tool: 'Grep', firstLine: 'no matches found' },
    ]);
  });

  it('omits errorSignatures field when content is not supplied', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        toolCalls: [tc('u0', 'Bash', 'a', { isError: true })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        toolCalls: [tc('u1', 'Read', 'b', { isError: true })],
      }),
      turn({
        sessionId: 's',
        messageId: 'm2',
        turnIndex: 2,
        toolCalls: [tc('u2', 'Grep', 'c', { isError: true })],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.failureRuns.length, 1);
    assert.equal(result.failureRuns[0]!.errorSignatures, undefined);
  });
});

describe('detectPatterns — CompactionLoss lostWork enrichment (#57)', () => {
  it('aggregates files and tool counts in the compacted window', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        ts: '2026-04-20T00:00:00.000Z',
        toolCalls: [
          tc('u0', 'Edit', 'h0', { target: '/src/foo.ts', editPreHash: 'a', editPostHash: 'b' }),
          tc('u1', 'Bash', 'h1'),
        ],
      }),
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        ts: '2026-04-20T00:00:01.000Z',
        toolCalls: [
          tc('u2', 'Edit', 'h2', { target: '/src/bar.ts', editPreHash: 'c', editPostHash: 'd' }),
          tc('u3', 'Read', 'h3'),
          tc('u4', 'Bash', 'h4'),
        ],
      }),
    ];
    const events: CompactionEvent[] = [
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's',
        ts: '2026-04-20T00:00:02.000Z',
        precedingMessageId: 'm1',
        tokensBeforeCompact: 9000,
      },
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set('s', [toolResult('s', 'm0', 'u0', 'present so the index is non-empty')]);
    const result = detectPatterns(turns, { pricing, compactions: events, contentBySession });
    assert.equal(result.compactions.length, 1);
    const lostWork = result.compactions[0]!.lostWork!;
    assert.deepEqual(lostWork.files, ['/src/bar.ts', '/src/foo.ts']);
    assert.equal(lostWork.editCount, 2);
    assert.equal(lostWork.bashCount, 2);
    assert.equal(lostWork.readCount, 1);
  });

  it('windows successive boundaries — second event excludes pre-first-boundary work', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        ts: '2026-04-20T00:00:00.000Z',
        toolCalls: [tc('u0', 'Edit', 'h0', { target: '/a.ts', editPreHash: 'a', editPostHash: 'b' })],
      }),
      // After first boundary
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        ts: '2026-04-20T00:00:02.000Z',
        toolCalls: [tc('u1', 'Edit', 'h1', { target: '/b.ts', editPreHash: 'c', editPostHash: 'd' })],
      }),
    ];
    const events: CompactionEvent[] = [
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's',
        ts: '2026-04-20T00:00:01.000Z',
        precedingMessageId: 'm0',
        tokensBeforeCompact: 5000,
      },
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's',
        ts: '2026-04-20T00:00:03.000Z',
        precedingMessageId: 'm1',
        tokensBeforeCompact: 7000,
      },
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set('s', [toolResult('s', 'm0', 'u0', 'x')]);
    const result = detectPatterns(turns, { pricing, compactions: events, contentBySession });
    assert.equal(result.compactions.length, 2);
    assert.deepEqual(result.compactions[0]!.lostWork!.files, ['/a.ts']);
    assert.deepEqual(result.compactions[1]!.lostWork!.files, ['/b.ts']);
  });

  it('omits lostWork when contentBySession is not supplied', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        ts: '2026-04-20T00:00:00.000Z',
        toolCalls: [tc('u0', 'Edit', 'h0', { target: '/src/foo.ts', editPreHash: 'a', editPostHash: 'b' })],
      }),
    ];
    const events: CompactionEvent[] = [
      {
        v: 1,
        source: 'claude-code',
        sessionId: 's',
        ts: '2026-04-20T00:00:01.000Z',
        precedingMessageId: 'm0',
        tokensBeforeCompact: 1000,
      },
    ];
    const result = detectPatterns(turns, { pricing, compactions: events });
    assert.equal(result.compactions.length, 1);
    assert.equal(result.compactions[0]!.lostWork, undefined);
  });
});

describe('detectPatterns — EditRevertCycle samplePreview enrichment (#57)', () => {
  it('populates samplePreview with truncated old/new strings from both anchors', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        toolCalls: [
          tc('u0', 'Edit', 'h0', {
            target: '/f.ts',
            editPreHash: 'A',
            editPostHash: 'B',
          }),
        ],
      }),
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        toolCalls: [
          tc('u1', 'Edit', 'h1', {
            target: '/f.ts',
            editPreHash: 'B',
            editPostHash: 'A',
          }),
        ],
      }),
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set('s', [
      toolUseRec('s', 'm0', 'u0', 'Edit', {
        old_string: 'foo',
        new_string: 'bar',
        file_path: '/f.ts',
      }),
      toolUseRec('s', 'm1', 'u1', 'Edit', {
        old_string: 'bar',
        new_string: 'foo',
        file_path: '/f.ts',
      }),
    ]);
    const result = detectPatterns(turns, { pricing, contentBySession });
    assert.equal(result.editReverts.length, 1);
    const preview = result.editReverts[0]!.samplePreview!;
    assert.equal(preview.firstEdit.old, 'foo');
    assert.equal(preview.firstEdit.new, 'bar');
    assert.equal(preview.revert.old, 'bar');
    assert.equal(preview.revert.new, 'foo');
  });

  it('truncates each preview field at ~200 chars with an ellipsis', async () => {
    const pricing = await loadBuiltinPricing();
    const long = 'x'.repeat(500);
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        toolCalls: [
          tc('u0', 'Edit', 'h0', {
            target: '/f.ts',
            editPreHash: 'A',
            editPostHash: 'B',
          }),
        ],
      }),
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        toolCalls: [
          tc('u1', 'Edit', 'h1', {
            target: '/f.ts',
            editPreHash: 'B',
            editPostHash: 'A',
          }),
        ],
      }),
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set('s', [
      toolUseRec('s', 'm0', 'u0', 'Edit', { old_string: long, new_string: long }),
      toolUseRec('s', 'm1', 'u1', 'Edit', { old_string: long, new_string: long }),
    ]);
    const result = detectPatterns(turns, { pricing, contentBySession });
    const preview = result.editReverts[0]!.samplePreview!;
    assert.ok(preview.firstEdit.old.length <= 200, 'old <= 200');
    assert.ok(preview.firstEdit.new.length <= 200, 'new <= 200');
    assert.ok(preview.firstEdit.old.endsWith('…'), 'truncated with ellipsis');
  });

  it('omits samplePreview when contentBySession is not supplied', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        toolCalls: [
          tc('u0', 'Edit', 'h0', { target: '/f.ts', editPreHash: 'A', editPostHash: 'B' }),
        ],
      }),
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        toolCalls: [
          tc('u1', 'Edit', 'h1', { target: '/f.ts', editPreHash: 'B', editPostHash: 'A' }),
        ],
      }),
    ];
    const result = detectPatterns(turns, { pricing });
    assert.equal(result.editReverts.length, 1);
    assert.equal(result.editReverts[0]!.samplePreview, undefined);
  });

  it('omits samplePreview when tool_use entries are missing from content', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        toolCalls: [
          tc('u0', 'Edit', 'h0', { target: '/f.ts', editPreHash: 'A', editPostHash: 'B' }),
        ],
      }),
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        toolCalls: [
          tc('u1', 'Edit', 'h1', { target: '/f.ts', editPreHash: 'B', editPostHash: 'A' }),
        ],
      }),
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set('s', [
      // Only tool_results, no tool_use records.
      toolResult('s', 'm0', 'u0', 'irrelevant'),
    ]);
    const result = detectPatterns(turns, { pricing, contentBySession });
    assert.equal(result.editReverts.length, 1);
    assert.equal(result.editReverts[0]!.samplePreview, undefined);
  });
});
