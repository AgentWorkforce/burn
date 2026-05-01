import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { ToolCall, TurnRecord } from '@relayburn/reader';

import { loadBuiltinPricing } from './pricing.js';
import {
  detectToolReplacementEligible,
  toolReplacementEligibleToFinding,
} from './tool-replacement-eligible.js';

function tc(id: string, name: string, target?: string, opts: Partial<ToolCall> = {}): ToolCall {
  return {
    id,
    name,
    argsHash: `${name}:${target ?? id}`,
    ...(target !== undefined ? { target } : {}),
    ...opts,
  };
}

function turn(o: Partial<TurnRecord> & { sessionId: string; messageId: string; turnIndex: number }): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    model: 'claude-sonnet-4-6',
    ts: '2026-04-20T00:00:00.000Z',
    usage: { input: 100, output: 50, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
    toolCalls: [],
    ...o,
  };
}

describe('detectToolReplacementEligible — search sequences', () => {
  it('flags ≥3 Glob → Grep → Read sequences in a session', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 4; i++) {
      turns.push(turn({
        sessionId: 's1',
        messageId: `m${i}`,
        turnIndex: i,
        toolCalls: [
          tc(`g${i}`, 'Glob', '*.ts'),
          tc(`r${i}`, 'Grep', 'foo'),
          tc(`d${i}`, 'Read', `/path/${i}.ts`),
        ],
      }));
    }
    const out = detectToolReplacementEligible(turns, { pricing });
    const search = out.find((f) => f.category === 'search-sequence');
    assert.ok(search, 'search-sequence finding should fire');
    assert.equal(search!.occurrenceCount, 4);
    assert.equal(search!.replacementTool, 'relaywash__Search');
    assert.ok(search!.estimatedTokensSaved > 0);
    assert.ok(search!.estimatedUsdSaved > 0);
  });

  it('does not flag below the threshold', async () => {
    const pricing = await loadBuiltinPricing();
    const turns = [
      turn({
        sessionId: 's',
        messageId: 'm0',
        turnIndex: 0,
        toolCalls: [tc('a', 'Glob', '*.ts'), tc('b', 'Grep', 'foo'), tc('c', 'Read', '/x.ts')],
      }),
      turn({
        sessionId: 's',
        messageId: 'm1',
        turnIndex: 1,
        toolCalls: [tc('d', 'Glob', '*.ts'), tc('e', 'Grep', 'bar'), tc('f', 'Read', '/y.ts')],
      }),
    ];
    const out = detectToolReplacementEligible(turns, { pricing });
    assert.equal(out.find((f) => f.category === 'search-sequence'), undefined);
  });

  it('respects ordering — Read-before-Glob does not count', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 4; i++) {
      turns.push(turn({
        sessionId: 's',
        messageId: `m${i}`,
        turnIndex: i,
        toolCalls: [
          tc(`r${i}`, 'Read', `/x${i}.ts`),
          tc(`g${i}`, 'Grep', 'foo'),
          tc(`b${i}`, 'Glob', '*.ts'),
        ],
      }));
    }
    const out = detectToolReplacementEligible(turns, { pricing });
    assert.equal(out.find((f) => f.category === 'search-sequence'), undefined);
  });
});

describe('detectToolReplacementEligible — edit clusters', () => {
  it('flags ≥3 edits to the same file within 5 turns', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 4; i++) {
      turns.push(turn({
        sessionId: 's',
        messageId: `m${i}`,
        turnIndex: i,
        toolCalls: [tc(`e${i}`, 'Edit', '/src/foo.ts')],
      }));
    }
    const out = detectToolReplacementEligible(turns, { pricing });
    const cluster = out.find((f) => f.category === 'edit-cluster');
    assert.ok(cluster, 'edit-cluster finding should fire');
    assert.equal(cluster!.occurrenceCount, 4);
    assert.equal(cluster!.replacementTool, 'relaywash__Edit');
    assert.deepEqual(cluster!.evidence, ['/src/foo.ts']);
  });

  it('does not flag edits spread across more than 5 consecutive turns', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [
      turn({ sessionId: 's', messageId: 'm0', turnIndex: 0, toolCalls: [tc('e0', 'Edit', '/f.ts')] }),
      turn({ sessionId: 's', messageId: 'm10', turnIndex: 10, toolCalls: [tc('e1', 'Edit', '/f.ts')] }),
      turn({ sessionId: 's', messageId: 'm20', turnIndex: 20, toolCalls: [tc('e2', 'Edit', '/f.ts')] }),
    ];
    const out = detectToolReplacementEligible(turns, { pricing });
    assert.equal(out.find((f) => f.category === 'edit-cluster'), undefined);
  });

  it('treats each file independently', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 4; i++) {
      turns.push(turn({
        sessionId: 's',
        messageId: `a${i}`,
        turnIndex: i,
        toolCalls: [tc(`a${i}`, 'Edit', '/a.ts')],
      }));
      turns.push(turn({
        sessionId: 's',
        messageId: `b${i}`,
        turnIndex: i + 10,
        toolCalls: [tc(`b${i}`, 'Edit', '/b.ts')],
      }));
    }
    const out = detectToolReplacementEligible(turns, { pricing });
    const clusters = out.filter((f) => f.category === 'edit-cluster');
    assert.equal(clusters.length, 2);
    const files = clusters.map((c) => c.evidence[0]).sort();
    assert.deepEqual(files, ['/a.ts', '/b.ts']);
  });
});

describe('detectToolReplacementEligible — bash sub-verb matches', () => {
  it('flags git status / git diff / git log calls', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [
      turn({ sessionId: 's', messageId: 'm0', turnIndex: 0, toolCalls: [tc('a', 'Bash', 'git status')] }),
      turn({ sessionId: 's', messageId: 'm1', turnIndex: 1, toolCalls: [tc('b', 'Bash', 'git diff HEAD~1')] }),
      turn({ sessionId: 's', messageId: 'm2', turnIndex: 2, toolCalls: [tc('c', 'Bash', 'git log --oneline -n 5')] }),
    ];
    const out = detectToolReplacementEligible(turns, { pricing });
    const git = out.find((f) => f.category === 'bash-git-state');
    assert.ok(git);
    assert.equal(git!.occurrenceCount, 3);
    assert.equal(git!.replacementTool, 'relaywash__GitState');
    assert.ok(git!.evidence.includes('git status'));
    assert.ok(git!.evidence.includes('git diff'));
    assert.ok(git!.evidence.includes('git log'));
  });

  it('flags pnpm test / pytest / jest calls', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [
      turn({ sessionId: 's', messageId: 'm0', turnIndex: 0, toolCalls: [tc('a', 'Bash', 'pnpm test')] }),
      turn({ sessionId: 's', messageId: 'm1', turnIndex: 1, toolCalls: [tc('b', 'Bash', 'pytest -k foo')] }),
      turn({ sessionId: 's', messageId: 'm2', turnIndex: 2, toolCalls: [tc('c', 'Bash', 'jest --watch')] }),
    ];
    const out = detectToolReplacementEligible(turns, { pricing });
    const test = out.find((f) => f.category === 'bash-test-run');
    assert.ok(test);
    assert.equal(test!.occurrenceCount, 3);
    assert.equal(test!.replacementTool, 'relaywash__TestRun');
  });

  it('flags gh pr view / gh api calls', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [
      turn({ sessionId: 's', messageId: 'm0', turnIndex: 0, toolCalls: [tc('a', 'Bash', 'gh pr view 123')] }),
      turn({ sessionId: 's', messageId: 'm1', turnIndex: 1, toolCalls: [tc('b', 'Bash', 'gh api repos/foo/bar/pulls/1/comments')] }),
    ];
    const out = detectToolReplacementEligible(turns, { pricing });
    const gh = out.find((f) => f.category === 'bash-gh-pr');
    assert.ok(gh);
    assert.equal(gh!.occurrenceCount, 2);
    assert.equal(gh!.replacementTool, 'relaywash__GhPR');
  });

  it('does not match unrelated bash commands', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [
      turn({ sessionId: 's', messageId: 'm0', turnIndex: 0, toolCalls: [tc('a', 'Bash', 'ls -la')] }),
      turn({ sessionId: 's', messageId: 'm1', turnIndex: 1, toolCalls: [tc('b', 'Bash', 'cat README.md')] }),
    ];
    const out = detectToolReplacementEligible(turns, { pricing });
    assert.equal(out.length, 0);
  });
});

describe('detectToolReplacementEligible — cross-harness', () => {
  it('normalizes OpenCode lowercase tool names for the search sequence', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 3; i++) {
      turns.push(turn({
        sessionId: 's',
        messageId: `m${i}`,
        turnIndex: i,
        source: 'opencode',
        toolCalls: [
          tc(`g${i}`, 'glob', '*.ts'),
          tc(`r${i}`, 'grep', 'foo'),
          tc(`d${i}`, 'read', `/x${i}.ts`),
        ],
      }));
    }
    const out = detectToolReplacementEligible(turns, { pricing });
    const search = out.find((f) => f.category === 'search-sequence');
    assert.ok(search, 'opencode lowercase tools should still trigger search-sequence');
    assert.equal(search!.source, 'opencode');
  });

  it('matches Codex apply_patch as an Edit for clustering', async () => {
    const pricing = await loadBuiltinPricing();
    const turns: TurnRecord[] = [];
    for (let i = 0; i < 4; i++) {
      turns.push(turn({
        sessionId: 's',
        messageId: `m${i}`,
        turnIndex: i,
        source: 'codex',
        toolCalls: [tc(`e${i}`, 'apply_patch', '/src/x.ts')],
      }));
    }
    const out = detectToolReplacementEligible(turns, { pricing });
    const cluster = out.find((f) => f.category === 'edit-cluster');
    assert.ok(cluster);
    assert.equal(cluster!.source, 'codex');
  });
});

describe('toolReplacementEligibleToFinding', () => {
  it('emits a WasteFinding with kind=tool-replacement-eligible and a relaywash-pointing action', () => {
    const finding = toolReplacementEligibleToFinding({
      source: 'claude-code',
      sessionId: 's-abcd1234',
      category: 'search-sequence',
      replacementTool: 'relaywash__Search',
      occurrenceCount: 5,
      estimatedTokensSaved: 12500,
      estimatedUsdSaved: 0.075,
      sampleTurnIndexes: [0, 1, 2, 3, 4],
      evidence: [],
    });
    assert.equal(finding.kind, 'tool-replacement-eligible');
    assert.equal(finding.sessionId, 's-abcd1234');
    assert.equal(finding.severity, 'warn');
    assert.match(finding.title, /relaywash__Search/);
    assert.match(finding.detail, /github\.com\/AgentWorkforce\/wash/);
    assert.equal(finding.estimatedSavings.tokensPerSession, 12500);
    assert.ok(Math.abs((finding.estimatedSavings.usdPerSession ?? 0) - 0.075) < 1e-9);
    assert.equal(finding.actions.length, 1);
    assert.match(finding.actions[0]!.label, /relaywash/);
  });
});
