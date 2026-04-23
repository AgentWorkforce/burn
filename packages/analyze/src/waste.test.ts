import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { ContentRecord, ToolCall, TurnRecord } from '@relayburn/reader';

import { loadBuiltinPricing } from './pricing.js';
import {
  aggregateByBash,
  aggregateByFile,
  aggregateBySubagent,
  attributeWaste,
} from './waste.js';

function tc(id: string, name: string, target?: string): ToolCall {
  return {
    id,
    name,
    argsHash: `${name}:${target ?? id}`,
    ...(target !== undefined ? { target } : {}),
  };
}

function turn(overrides: Partial<TurnRecord> & { sessionId: string; messageId: string; turnIndex: number }): TurnRecord {
  return {
    v: 1,
    source: 'claude-code',
    model: 'claude-sonnet-4-6',
    ts: '2026-04-20T00:00:00.000Z',
    usage: { input: 0, output: 0, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
    toolCalls: [],
    ...overrides,
  };
}

function toolResultContent(sessionId: string, toolUseId: string, text: string, ts: string): ContentRecord {
  return {
    v: 1,
    source: 'claude-code',
    sessionId,
    messageId: `m-${toolUseId}`,
    ts,
    role: 'tool_result',
    kind: 'tool_result',
    toolResult: { toolUseId, content: text },
  };
}

describe('attributeWaste', () => {
  it('attributes the persistence of an 8k Read across 20 ride-along turns within ±10% of hand truth', async () => {
    const pricing = await loadBuiltinPricing();
    const rate = pricing['claude-sonnet-4-6']!;
    const READ_TOKENS = 8000;
    const READ_TEXT = 'x'.repeat(READ_TOKENS * 4); // 4 chars/token estimate

    const sessionId = 's-waste-1';
    const turns: TurnRecord[] = [];

    // Turn 0: assistant emits Read tool_use
    turns.push(turn({
      sessionId,
      messageId: 'msg-0',
      turnIndex: 0,
      ts: '2026-04-20T00:00:00.000Z',
      toolCalls: [tc('tu_read_1', 'Read', '/src/big.ts')],
      usage: { input: 200, output: 50, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
    }));

    // Turn 1: pays initial cost (8k tokens enter as fresh input)
    turns.push(turn({
      sessionId,
      messageId: 'msg-1',
      turnIndex: 1,
      ts: '2026-04-20T00:00:01.000Z',
      usage: { input: READ_TOKENS, output: 30, reasoning: 0, cacheRead: 250, cacheCreate5m: 0, cacheCreate1h: 0 },
    }));

    // Turns 2..21: 20 ride-along turns each with cacheRead >= 8000
    for (let i = 2; i <= 21; i++) {
      turns.push(turn({
        sessionId,
        messageId: `msg-${i}`,
        turnIndex: i,
        ts: `2026-04-20T00:00:${String(i).padStart(2, '0')}.000Z`,
        usage: {
          input: 50,
          output: 30,
          reasoning: 0,
          // Always >= READ_TOKENS so the tool_result is treated as still cached.
          cacheRead: READ_TOKENS + 2000,
          cacheCreate5m: 0,
          cacheCreate1h: 0,
        },
      }));
    }

    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set(sessionId, [
      toolResultContent(sessionId, 'tu_read_1', READ_TEXT, '2026-04-20T00:00:00.500Z'),
    ]);

    const result = attributeWaste(turns, { pricing, contentBySession });
    assert.equal(result.attributions.length, 1);
    const a = result.attributions[0]!;
    assert.equal(a.toolUseId, 'tu_read_1');

    // Hand-computed expected:
    //   initial: 8000 tokens × input rate (since turn 1's new content was all
    //     fresh input, no cacheCreate)
    //   persistence: 20 ride-alongs × 8000 tokens × cacheRead rate
    const expectedInitial = (READ_TOKENS / 1_000_000) * rate.input;
    const expectedPersistence = 20 * (READ_TOKENS / 1_000_000) * rate.cacheRead;
    const expectedTotal = expectedInitial + expectedPersistence;

    assert.ok(
      Math.abs(a.totalCost - expectedTotal) <= expectedTotal * 0.10,
      `total=${a.totalCost} expected=${expectedTotal} diff>10%`,
    );
    assert.equal(a.ridingTurns, 20);
  });

  it('aggregates by file and ranks the most expensive Read first', async () => {
    const pricing = await loadBuiltinPricing();
    const sessionId = 's-files';
    const READ_TOKENS = 5000;
    const SMALL_TOKENS = 200;
    const turns: TurnRecord[] = [
      turn({
        sessionId,
        messageId: 'msg-0',
        turnIndex: 0,
        toolCalls: [tc('tu_a', 'Read', '/big.ts'), tc('tu_b', 'Read', '/small.ts')],
      }),
      turn({
        sessionId,
        messageId: 'msg-1',
        turnIndex: 1,
        usage: { input: READ_TOKENS + SMALL_TOKENS, output: 5, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
      turn({
        sessionId,
        messageId: 'msg-2',
        turnIndex: 2,
        usage: { input: 100, output: 5, reasoning: 0, cacheRead: READ_TOKENS + SMALL_TOKENS + 500, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
      turn({
        sessionId,
        messageId: 'msg-3',
        turnIndex: 3,
        usage: { input: 100, output: 5, reasoning: 0, cacheRead: READ_TOKENS + SMALL_TOKENS + 500, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set(sessionId, [
      toolResultContent(sessionId, 'tu_a', 'x'.repeat(READ_TOKENS * 4), '2026-04-20T00:00:00.100Z'),
      toolResultContent(sessionId, 'tu_b', 'y'.repeat(SMALL_TOKENS * 4), '2026-04-20T00:00:00.101Z'),
    ]);

    const result = attributeWaste(turns, { pricing, contentBySession });
    const files = aggregateByFile(result.attributions);
    assert.equal(files.length, 2);
    assert.equal(files[0]!.path, '/big.ts');
    assert.equal(files[1]!.path, '/small.ts');
    assert.ok(files[0]!.totalCost > files[1]!.totalCost);
  });

  it('aggregates by Bash argsHash so repeated commands collapse', async () => {
    const pricing = await loadBuiltinPricing();
    const sessionId = 's-bash';
    const turns: TurnRecord[] = [];
    let ts = 0;
    for (let i = 0; i < 3; i++) {
      turns.push(turn({
        sessionId,
        messageId: `msg-emit-${i}`,
        turnIndex: ts++,
        toolCalls: [{ id: `tu_b_${i}`, name: 'Bash', target: 'ls -la', argsHash: 'Bash:ls' }],
      }));
      turns.push(turn({
        sessionId,
        messageId: `msg-pay-${i}`,
        turnIndex: ts++,
        usage: { input: 1000, output: 5, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }));
    }
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set(sessionId, [
      toolResultContent(sessionId, 'tu_b_0', 'x'.repeat(4000), '2026-04-20T00:00:00.100Z'),
      toolResultContent(sessionId, 'tu_b_1', 'x'.repeat(4000), '2026-04-20T00:00:00.200Z'),
      toolResultContent(sessionId, 'tu_b_2', 'x'.repeat(4000), '2026-04-20T00:00:00.300Z'),
    ]);
    const result = attributeWaste(turns, { pricing, contentBySession });
    const bash = aggregateByBash(result.attributions);
    assert.equal(bash.length, 1);
    assert.equal(bash[0]!.callCount, 3);
  });

  it('aggregates subagent calls by subagent_type', async () => {
    const pricing = await loadBuiltinPricing();
    const sessionId = 's-agent';
    const turns: TurnRecord[] = [
      turn({
        sessionId,
        messageId: 'msg-0',
        turnIndex: 0,
        toolCalls: [{ id: 'tu_a1', name: 'Agent', target: 'general-purpose', argsHash: 'Agent:gp' }],
      }),
      turn({
        sessionId,
        messageId: 'msg-1',
        turnIndex: 1,
        usage: { input: 2000, output: 10, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set(sessionId, [
      toolResultContent(sessionId, 'tu_a1', 'z'.repeat(8000), '2026-04-20T00:00:00.100Z'),
    ]);
    const result = attributeWaste(turns, { pricing, contentBySession });
    const subagents = aggregateBySubagent(result.attributions);
    assert.equal(subagents.length, 1);
    assert.equal(subagents[0]!.subagentType, 'general-purpose');
    assert.equal(subagents[0]!.callCount, 1);
    assert.ok(subagents[0]!.totalCost > 0);
  });

  it('falls back to even-split (initial only) when no content is provided', async () => {
    const pricing = await loadBuiltinPricing();
    const sessionId = 's-fallback';
    const turns: TurnRecord[] = [
      turn({
        sessionId,
        messageId: 'msg-0',
        turnIndex: 0,
        toolCalls: [tc('tu_x', 'Read', '/a.ts'), tc('tu_y', 'Read', '/b.ts')],
      }),
      turn({
        sessionId,
        messageId: 'msg-1',
        turnIndex: 1,
        usage: { input: 4000, output: 5, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];
    const result = attributeWaste(turns, { pricing });
    assert.equal(result.attributions.length, 2);
    // Even split: each tool gets half of the next turn's input cost.
    const rate = pricing['claude-sonnet-4-6']!;
    const expected = ((4000 / 1_000_000) * rate.input) / 2;
    for (const a of result.attributions) {
      assert.ok(Math.abs(a.initialCost - expected) < 1e-9);
      // No persistence in even-split mode.
      assert.equal(a.persistenceCost, 0);
    }
    assert.equal(result.sessionTotals[0]!.attributionMethod, 'even-split');
  });

  it('caps sibling initial cost at the next turn\'s actual newContent', async () => {
    // Two large tool_results sized 6000 + 4000 = 10000 tokens enter on the
    // same next turn, but turn N+1 only paid for 5000 newContent. The summed
    // initialTokens across the two siblings must not exceed 5000, and the
    // share must be proportional to size.
    const pricing = await loadBuiltinPricing();
    const sessionId = 's-cap';
    const turns: TurnRecord[] = [
      turn({
        sessionId,
        messageId: 'msg-0',
        turnIndex: 0,
        toolCalls: [tc('tu_big', 'Read', '/big.ts'), tc('tu_med', 'Read', '/med.ts')],
      }),
      turn({
        sessionId,
        messageId: 'msg-1',
        turnIndex: 1,
        usage: { input: 5000, output: 5, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set(sessionId, [
      toolResultContent(sessionId, 'tu_big', 'x'.repeat(6000 * 4), '2026-04-20T00:00:00.100Z'),
      toolResultContent(sessionId, 'tu_med', 'y'.repeat(4000 * 4), '2026-04-20T00:00:00.101Z'),
    ]);
    const result = attributeWaste(turns, { pricing, contentBySession });
    const summed = result.attributions.reduce((s, a) => s + a.initialTokens, 0);
    assert.ok(summed <= 5000 + 1e-6, `summed=${summed} > newContent=5000`);
    const big = result.attributions.find((a) => a.toolUseId === 'tu_big')!;
    const med = result.attributions.find((a) => a.toolUseId === 'tu_med')!;
    // Proportional by size: 6/10 vs 4/10 of the 5000 cap.
    assert.ok(Math.abs(big.initialTokens - 3000) < 1e-6);
    assert.ok(Math.abs(med.initialTokens - 2000) < 1e-6);
  });

  it('caps sibling persistence at the turn\'s actual cacheRead', async () => {
    // Two cached tool_results of 4000 + 4000 ride along on a turn whose
    // cacheRead is only 5000. Their summed persistenceTokens for that turn
    // must not exceed 5000, allocated proportionally by size (so 2500 each).
    const pricing = await loadBuiltinPricing();
    const sessionId = 's-persist-cap';
    const turns: TurnRecord[] = [
      turn({
        sessionId,
        messageId: 'msg-0',
        turnIndex: 0,
        toolCalls: [tc('tu_a', 'Read', '/a.ts'), tc('tu_b', 'Read', '/b.ts')],
      }),
      turn({
        sessionId,
        messageId: 'msg-1',
        turnIndex: 1,
        usage: { input: 8000, output: 5, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
      turn({
        sessionId,
        messageId: 'msg-2',
        turnIndex: 2,
        // Both results pass the per-result eviction test (cacheRead >= 4000)
        // but the proportional allocation should sum to <= 5000, not 8000.
        usage: { input: 50, output: 5, reasoning: 0, cacheRead: 5000, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set(sessionId, [
      toolResultContent(sessionId, 'tu_a', 'x'.repeat(4000 * 4), '2026-04-20T00:00:00.100Z'),
      toolResultContent(sessionId, 'tu_b', 'y'.repeat(4000 * 4), '2026-04-20T00:00:00.101Z'),
    ]);
    const result = attributeWaste(turns, { pricing, contentBySession });
    const summedPersist = result.attributions.reduce((s, a) => s + a.persistenceTokens, 0);
    assert.ok(summedPersist <= 5000 + 1e-6, `summedPersist=${summedPersist} > cacheRead=5000`);
    for (const a of result.attributions) {
      assert.ok(Math.abs(a.persistenceTokens - 2500) < 1e-6);
    }
  });

  it('uses the paying turn\'s model rate, not the emit turn\'s', async () => {
    // Emit on Sonnet, pay (initial + persistence) on Haiku. The attributed
    // cost should reflect Haiku's rates, not Sonnet's.
    const pricing = await loadBuiltinPricing();
    const sonnet = pricing['claude-sonnet-4-6']!;
    const haiku = pricing['claude-haiku-4-5']!;
    assert.ok(haiku.input !== sonnet.input, 'test prerequisite: rates differ');

    const sessionId = 's-cross-model';
    const TOK = 4000;
    const turns: TurnRecord[] = [
      turn({
        sessionId,
        model: 'claude-sonnet-4-6',
        messageId: 'msg-0',
        turnIndex: 0,
        toolCalls: [tc('tu_x', 'Read', '/x.ts')],
      }),
      turn({
        sessionId,
        model: 'claude-haiku-4-5',
        messageId: 'msg-1',
        turnIndex: 1,
        usage: { input: TOK, output: 5, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
      turn({
        sessionId,
        model: 'claude-haiku-4-5',
        messageId: 'msg-2',
        turnIndex: 2,
        usage: { input: 50, output: 5, reasoning: 0, cacheRead: TOK + 100, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set(sessionId, [
      toolResultContent(sessionId, 'tu_x', 'z'.repeat(TOK * 4), '2026-04-20T00:00:00.100Z'),
    ]);
    const result = attributeWaste(turns, { pricing, contentBySession });
    const a = result.attributions[0]!;
    const expectedInitial = (TOK / 1_000_000) * haiku.input;
    const expectedPersistence = (TOK / 1_000_000) * haiku.cacheRead;
    assert.ok(Math.abs(a.initialCost - expectedInitial) < 1e-9, `initialCost=${a.initialCost} expected=${expectedInitial}`);
    assert.ok(Math.abs(a.persistenceCost - expectedPersistence) < 1e-9, `persistenceCost=${a.persistenceCost} expected=${expectedPersistence}`);
  });

  it('grand total + unattributed = session grand total within rounding', async () => {
    const pricing = await loadBuiltinPricing();
    const sessionId = 's-totals';
    const turns: TurnRecord[] = [
      turn({
        sessionId,
        messageId: 'msg-0',
        turnIndex: 0,
        toolCalls: [tc('tu_z', 'Read', '/z.ts')],
        usage: { input: 100, output: 50, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
      turn({
        sessionId,
        messageId: 'msg-1',
        turnIndex: 1,
        usage: { input: 2000, output: 30, reasoning: 0, cacheRead: 0, cacheCreate5m: 0, cacheCreate1h: 0 },
      }),
    ];
    const contentBySession = new Map<string, ContentRecord[]>();
    contentBySession.set(sessionId, [
      toolResultContent(sessionId, 'tu_z', 'q'.repeat(2000 * 4), '2026-04-20T00:00:00.500Z'),
    ]);
    const result = attributeWaste(turns, { pricing, contentBySession });
    assert.ok(Math.abs(result.attributedTotal + result.unattributedTotal - result.grandTotal) < 1e-9);
  });
});
