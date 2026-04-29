import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type {
  SessionRelationshipRecord,
  Subagent,
  TurnRecord,
} from '@relayburn/reader';

import { loadBuiltinPricing } from './pricing.js';
import { aggregateSubagentTypeStats, buildSubagentTree } from './subagent-tree.js';

function turn(
  overrides: Partial<TurnRecord> & { sessionId: string; messageId: string; model: string },
): TurnRecord {
  const base: TurnRecord = {
    v: 1,
    source: overrides.source ?? 'claude-code',
    sessionId: overrides.sessionId,
    messageId: overrides.messageId,
    turnIndex: overrides.turnIndex ?? 0,
    ts: overrides.ts ?? '2026-04-20T00:00:00.000Z',
    model: overrides.model,
    usage: overrides.usage ?? {
      input: 1000,
      output: 1000,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: overrides.toolCalls ?? [],
  };
  if (overrides.subagent) base.subagent = overrides.subagent;
  return base;
}

function sub(fields: Partial<Subagent>): Subagent {
  return { isSidechain: true, ...fields };
}

function relationship(
  overrides: Partial<SessionRelationshipRecord> & { sessionId: string },
): SessionRelationshipRecord {
  return {
    v: 1,
    source: 'native-claude',
    relationshipType: 'root',
    ...overrides,
  };
}

describe('buildSubagentTree', () => {
  it('folds cumulative cost from nested subagents up to the main root', async () => {
    const pricing = await loadBuiltinPricing();
    const sessionId = 'sess-1';
    const turns: TurnRecord[] = [
      // Main thread: 2 turns
      turn({ sessionId, messageId: 'm1', model: 'claude-sonnet-4-6', turnIndex: 0 }),
      turn({ sessionId, messageId: 'm2', model: 'claude-sonnet-4-6', turnIndex: 1 }),
      // Outer subagent (Explore): 2 turns
      turn({
        sessionId,
        messageId: 'o1',
        model: 'claude-haiku-4-5',
        turnIndex: 2,
        subagent: sub({
          agentId: 'u-outer',
          parentAgentId: sessionId,
          subagentType: 'Explore',
          description: 'Research',
          parentToolUseId: 'toolu_outer',
        }),
      }),
      turn({
        sessionId,
        messageId: 'o2',
        model: 'claude-haiku-4-5',
        turnIndex: 3,
        subagent: sub({
          agentId: 'u-outer',
          parentAgentId: sessionId,
          subagentType: 'Explore',
          parentToolUseId: 'toolu_outer',
        }),
      }),
      // Inner subagent under Explore: 1 turn
      turn({
        sessionId,
        messageId: 'i1',
        model: 'claude-haiku-4-5',
        turnIndex: 4,
        subagent: sub({
          agentId: 'u-inner',
          parentAgentId: 'u-outer',
          subagentType: 'code-reviewer',
          parentToolUseId: 'toolu_inner',
        }),
      }),
    ];

    const trees = buildSubagentTree(turns, { pricing });
    const root = trees.get(sessionId)!;
    assert.ok(root);
    assert.equal(root.label, 'main');
    assert.equal(root.depth, 0);
    assert.equal(root.selfTurns, 2);
    assert.equal(root.cumulativeTurns, 5);
    assert.ok(root.cumulativeCost > root.selfCost, 'cumulative must include subagent costs');

    assert.equal(root.children.length, 1);
    const outer = root.children[0]!;
    assert.equal(outer.label, 'Explore');
    assert.equal(outer.depth, 1);
    assert.equal(outer.selfTurns, 2);
    assert.equal(outer.cumulativeTurns, 3);
    assert.equal(outer.children.length, 1);

    const inner = outer.children[0]!;
    assert.equal(inner.label, 'code-reviewer');
    assert.equal(inner.depth, 2);
    assert.equal(inner.selfTurns, 1);
    assert.equal(inner.cumulativeTurns, 1);
    assert.equal(inner.cumulativeCost, inner.selfCost);

    // Roll-up invariant: outer.cumulativeCost = outer.selfCost + inner.cumulativeCost
    assert.ok(
      Math.abs(outer.cumulativeCost - (outer.selfCost + inner.cumulativeCost)) < 1e-12,
      'outer cumulative is selfCost + inner.cumulativeCost',
    );
  });

  it('buckets sidechain turns without agentId under an (unresolved) node', async () => {
    const pricing = await loadBuiltinPricing();
    const sessionId = 'sess-2';
    const turns: TurnRecord[] = [
      turn({ sessionId, messageId: 'm1', model: 'claude-sonnet-4-6' }),
      // Sidechain with no tree fields (partial/incremental ingest case).
      turn({
        sessionId,
        messageId: 's1',
        model: 'claude-haiku-4-5',
        turnIndex: 1,
        subagent: { isSidechain: true },
      }),
    ];
    const trees = buildSubagentTree(turns, { pricing });
    const root = trees.get(sessionId)!;
    assert.equal(root.children.length, 1);
    assert.equal(root.children[0]!.label, '(unresolved)');
    assert.equal(root.children[0]!.selfTurns, 1);
  });

  it('builds the same Claude tree from SessionRelationshipRecord rows', async () => {
    const pricing = await loadBuiltinPricing();
    const sessionId = 'sess-graph';
    const turns: TurnRecord[] = [
      turn({ sessionId, messageId: 'm1', model: 'claude-sonnet-4-6', turnIndex: 0 }),
      turn({
        sessionId,
        messageId: 'o1',
        model: 'claude-haiku-4-5',
        turnIndex: 1,
        subagent: sub({
          agentId: 'u-outer',
          parentAgentId: sessionId,
          subagentType: 'Explore',
          description: 'Research',
          parentToolUseId: 'toolu_outer',
        }),
      }),
      turn({
        sessionId,
        messageId: 'i1',
        model: 'claude-haiku-4-5',
        turnIndex: 2,
        subagent: sub({
          agentId: 'u-inner',
          parentAgentId: 'u-outer',
          subagentType: 'code-reviewer',
          parentToolUseId: 'toolu_inner',
        }),
      }),
    ];
    const relationships: SessionRelationshipRecord[] = [
      relationship({ sessionId, source: 'claude-code', relationshipType: 'root' }),
      relationship({
        sessionId,
        relationshipType: 'subagent',
        relatedSessionId: sessionId,
        agentId: 'u-outer',
        subagentType: 'Explore',
        description: 'Research',
        parentToolUseId: 'toolu_outer',
      }),
      relationship({
        sessionId,
        relationshipType: 'subagent',
        relatedSessionId: 'u-outer',
        agentId: 'u-inner',
        subagentType: 'code-reviewer',
        parentToolUseId: 'toolu_inner',
      }),
    ];

    const legacy = buildSubagentTree(turns, { pricing }).get(sessionId)!;
    const graph = buildSubagentTree(turns, { pricing, relationships }).get(sessionId)!;
    assert.deepEqual(graph, legacy);
    assert.equal(graph.relationshipType, 'root');
    assert.equal(graph.children[0]!.relationshipType, 'subagent');
  });

  it('joins child-session relationship rows to turns without per-turn subagent metadata', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [
      turn({
        source: 'codex',
        sessionId: 'parent-session',
        messageId: 'parent-1',
        model: 'gpt-5.1-codex',
      }),
      turn({
        source: 'codex',
        sessionId: 'child-session',
        messageId: 'child-1',
        model: 'gpt-5.1-codex',
      }),
    ];
    const relationships: SessionRelationshipRecord[] = [
      relationship({
        source: 'codex',
        sessionId: 'parent-session',
        relationshipType: 'root',
      }),
      relationship({
        source: 'codex',
        sessionId: 'child-session',
        relationshipType: 'subagent',
        relatedSessionId: 'parent-session',
        agentId: 'agent-child',
        subagentType: 'worker',
      }),
    ];

    const root = buildSubagentTree(turns, { pricing, relationships }).get('parent-session')!;
    assert.equal(root.selfTurns, 1);
    assert.equal(root.cumulativeTurns, 2);
    assert.equal(root.children.length, 1);
    assert.equal(root.children[0]!.label, 'worker');
    assert.equal(root.children[0]!.nodeId, 'child-session');
    assert.equal(root.children[0]!.relationshipType, 'subagent');
    assert.equal(root.children[0]!.selfTurns, 1);
  });

  it('does not alias native sidechain session roots onto agent ids when turns lack subagent fields', async () => {
    const pricing = await loadBuiltinPricing();
    const sessionId = 'partial-claude';
    const turns: TurnRecord[] = [
      turn({ sessionId, messageId: 'main-1', model: 'claude-sonnet-4-6' }),
    ];
    const relationships: SessionRelationshipRecord[] = [
      relationship({ sessionId, source: 'claude-code', relationshipType: 'root' }),
      relationship({
        sessionId,
        relationshipType: 'subagent',
        relatedSessionId: sessionId,
        agentId: 'u-outer',
        subagentType: 'Explore',
      }),
    ];

    const root = buildSubagentTree(turns, { pricing, relationships }).get(sessionId)!;
    assert.equal(root.nodeId, sessionId);
    assert.equal(root.label, 'main');
    assert.equal(root.selfTurns, 1);
    assert.equal(root.children.length, 1);
    assert.equal(root.children[0]!.nodeId, 'u-outer');
    assert.equal(root.children[0]!.selfTurns, 0);
  });
});

describe('aggregateSubagentTypeStats', () => {
  it('reports median/p95/mean/total per subagent type across invocations', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    // Three Explore invocations with different costs (1, 2, 3 turns each).
    for (let i = 0; i < 3; i++) {
      const agentId = `u-exp-${i}`;
      for (let j = 0; j <= i; j++) {
        turns.push(
          turn({
            sessionId: `sess-${i}`,
            messageId: `m-${i}-${j}`,
            model: 'claude-haiku-4-5',
            turnIndex: j,
            subagent: sub({ agentId, subagentType: 'Explore' }),
          }),
        );
      }
    }
    // One code-reviewer invocation (1 turn).
    turns.push(
      turn({
        sessionId: 'sess-rev',
        messageId: 'mr',
        model: 'claude-haiku-4-5',
        subagent: sub({ agentId: 'u-rev', subagentType: 'code-reviewer' }),
      }),
    );

    const stats = aggregateSubagentTypeStats(turns, { pricing });
    const explore = stats.find((s) => s.subagentType === 'Explore')!;
    assert.ok(explore);
    assert.equal(explore.invocations, 3);
    assert.equal(explore.turns, 6);
    assert.ok(explore.medianCost > 0);
    assert.ok(explore.p95Cost >= explore.medianCost);
    assert.ok(Math.abs(explore.meanCost - explore.totalCost / 3) < 1e-12);

    const rev = stats.find((s) => s.subagentType === 'code-reviewer')!;
    assert.equal(rev.invocations, 1);
    assert.equal(rev.turns, 1);
    assert.equal(rev.medianCost, rev.totalCost);
    assert.equal(rev.p95Cost, rev.totalCost);
  });
});
