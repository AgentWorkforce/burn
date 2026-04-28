import { strict as assert } from 'node:assert';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';
import { describe, it } from 'node:test';

import { parseClaudeSession } from '@relayburn/reader';
import type { CompactionEvent, ToolCall, TurnRecord, UserTurnRecord } from '@relayburn/reader';

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
