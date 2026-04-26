import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import type { Plan } from '@relayburn/ledger';
import type { TurnRecord } from '@relayburn/reader';

import { computePlanUsage, cycleBounds } from './plan-usage.js';
import type { PricingTable } from './pricing.js';

const PRICING: PricingTable = {
  'claude-sonnet-4-6': {
    input: 3,
    output: 15,
    cacheRead: 0.3,
    cacheWrite: 3.75,
    reasoningMode: 'same_as_output',
  },
};

function turn(opts: {
  ts: string;
  source?: TurnRecord['source'];
  inputTokens?: number;
  outputTokens?: number;
  model?: string;
  sessionId?: string;
}): TurnRecord {
  return {
    v: 1,
    source: opts.source ?? 'claude-code',
    sessionId: opts.sessionId ?? 's1',
    messageId: `m-${opts.ts}`,
    turnIndex: 0,
    ts: opts.ts,
    model: opts.model ?? 'claude-sonnet-4-6',
    usage: {
      input: opts.inputTokens ?? 0,
      output: opts.outputTokens ?? 0,
      reasoning: 0,
      cacheRead: 0,
      cacheCreate5m: 0,
      cacheCreate1h: 0,
    },
    toolCalls: [],
  };
}

const plan: Plan = {
  id: 'claude-pro',
  provider: 'claude',
  name: 'Claude Pro',
  budgetUsd: 20,
  resetDay: 1,
};

describe('cycleBounds', () => {
  it('day 1 → calendar month boundary', () => {
    const now = new Date('2026-04-15T12:00:00.000Z');
    const { cycleStart, cycleEnd } = cycleBounds(1, now);
    assert.equal(cycleStart.toISOString(), '2026-04-01T00:00:00.000Z');
    assert.equal(cycleEnd.toISOString(), '2026-05-01T00:00:00.000Z');
  });

  it('day 15 mid-cycle → cycle started this month', () => {
    const now = new Date('2026-04-20T12:00:00.000Z');
    const { cycleStart, cycleEnd } = cycleBounds(15, now);
    assert.equal(cycleStart.toISOString(), '2026-04-15T00:00:00.000Z');
    assert.equal(cycleEnd.toISOString(), '2026-05-15T00:00:00.000Z');
  });

  it('day 15 before mid-month → cycle started last month', () => {
    const now = new Date('2026-04-10T12:00:00.000Z');
    const { cycleStart, cycleEnd } = cycleBounds(15, now);
    assert.equal(cycleStart.toISOString(), '2026-03-15T00:00:00.000Z');
    assert.equal(cycleEnd.toISOString(), '2026-04-15T00:00:00.000Z');
  });

  it('day 31 in February → clamps to last day of February', () => {
    const now = new Date('2026-02-15T12:00:00.000Z');
    const { cycleStart } = cycleBounds(31, now);
    // 2026 is not a leap year → Feb has 28 days
    assert.equal(cycleStart.toISOString(), '2026-01-31T00:00:00.000Z');
  });

  it('day 31 anchored end-of-month rolls forward correctly', () => {
    // Just after Jan 31 → cycle started Jan 31, ends end-of-Feb (clamped to 28)
    const now = new Date('2026-02-10T12:00:00.000Z');
    const { cycleStart, cycleEnd } = cycleBounds(31, now);
    assert.equal(cycleStart.toISOString(), '2026-01-31T00:00:00.000Z');
    assert.equal(cycleEnd.toISOString(), '2026-02-28T00:00:00.000Z');
  });

  it('crosses year boundary cleanly', () => {
    const now = new Date('2026-01-05T12:00:00.000Z');
    const { cycleStart, cycleEnd } = cycleBounds(15, now);
    assert.equal(cycleStart.toISOString(), '2025-12-15T00:00:00.000Z');
    assert.equal(cycleEnd.toISOString(), '2026-01-15T00:00:00.000Z');
  });
});

describe('computePlanUsage', () => {
  const now = new Date('2026-04-15T00:00:00.000Z'); // 14 days into a calendar cycle

  it('sums spend within the cycle and ignores turns outside it', () => {
    // 1M input tokens at $3/MM = $3 per turn
    const turns: TurnRecord[] = [
      turn({ ts: '2026-04-05T00:00:00.000Z', inputTokens: 1_000_000 }),
      turn({ ts: '2026-04-10T00:00:00.000Z', inputTokens: 1_000_000 }),
      // Outside cycle: previous month
      turn({ ts: '2026-03-25T00:00:00.000Z', inputTokens: 5_000_000 }),
    ];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    assert.equal(u.spentUsd, 6);
    assert.equal(u.daysElapsed, 14);
    assert.equal(u.daysInCycle, 30);
  });

  it('linearly projects end-of-cycle spend from observed rate', () => {
    // 1M input tokens × $3/MM = $3 per turn; 14 turns = $42 over 14 days.
    // Cycle = 30 days, so projected = 42 × 30/14 = $90.
    const turns: TurnRecord[] = Array.from({ length: 14 }, (_, i) =>
      turn({ ts: `2026-04-${String(i + 1).padStart(2, '0')}T00:00:00.000Z`, inputTokens: 1_000_000 }),
    );
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    assert.equal(u.spentUsd, 42);
    assert.equal(u.projectedEndOfCycleUsd, 90);
  });

  it('flags overBudget and computes runwayDays when projection exceeds budget', () => {
    // $30 spent over 14 days on a $20 plan → projected $64, over budget
    const turns: TurnRecord[] = [
      turn({ ts: '2026-04-08T00:00:00.000Z', inputTokens: 10_000_000 }),
    ];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    assert.equal(u.spentUsd, 30);
    assert.ok(u.projectedEndOfCycleUsd > plan.budgetUsd);
    assert.equal(u.overBudget, true);
    // remaining = max(0, 20-30) = 0 → runway = 0 days (already past)
    assert.equal(u.runwayDays, 0);
  });

  it('runwayDays is null when projection is under budget', () => {
    const turns: TurnRecord[] = [turn({ ts: '2026-04-05T00:00:00.000Z', inputTokens: 1_000_000 })];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    assert.equal(u.overBudget, false);
    assert.equal(u.runwayDays, null);
  });

  it('flags limitedData when fewer than 7 days have elapsed', () => {
    const earlyNow = new Date('2026-04-04T00:00:00.000Z'); // 3 days into cycle
    const u = computePlanUsage(plan, [], { pricing: PRICING, now: earlyNow });
    assert.equal(u.daysElapsed, 3);
    assert.equal(u.limitedData, true);
  });

  it('does not collapse projection to zero when cycle just started', () => {
    const cycleStart = new Date('2026-04-01T00:00:00.000Z');
    const u = computePlanUsage(plan, [], { pricing: PRICING, now: cycleStart });
    assert.equal(u.spentUsd, 0);
    assert.equal(u.projectedEndOfCycleUsd, 0); // 0 spend → 0 projection, but no NaN
    assert.equal(Number.isFinite(u.projectedEndOfCycleUsd), true);
  });

  it('claude provider only counts claude-code/anthropic-api turns', () => {
    const turns: TurnRecord[] = [
      turn({ ts: '2026-04-05T00:00:00.000Z', source: 'claude-code', inputTokens: 1_000_000 }),
      turn({ ts: '2026-04-06T00:00:00.000Z', source: 'codex', inputTokens: 1_000_000 }),
      turn({ ts: '2026-04-07T00:00:00.000Z', source: 'opencode', inputTokens: 1_000_000 }),
    ];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    assert.equal(u.spentUsd, 3); // only the claude-code turn
  });

  it('custom provider counts every turn', () => {
    const customPlan: Plan = { ...plan, provider: 'custom' };
    const turns: TurnRecord[] = [
      turn({ ts: '2026-04-05T00:00:00.000Z', source: 'claude-code', inputTokens: 1_000_000 }),
      turn({ ts: '2026-04-06T00:00:00.000Z', source: 'codex', inputTokens: 1_000_000 }),
      turn({ ts: '2026-04-07T00:00:00.000Z', source: 'opencode', inputTokens: 1_000_000 }),
    ];
    const u = computePlanUsage(customPlan, turns, { pricing: PRICING, now });
    assert.equal(u.spentUsd, 9); // all three
  });

  it('honors a custom resetDay (anniversary cycle)', () => {
    const anniversaryPlan: Plan = { ...plan, resetDay: 15 };
    // now = April 20, cycle started April 15, ends May 15
    const turns: TurnRecord[] = [
      turn({ ts: '2026-04-16T00:00:00.000Z', inputTokens: 1_000_000 }), // inside
      turn({ ts: '2026-04-10T00:00:00.000Z', inputTokens: 5_000_000 }), // before cycle start
    ];
    const u = computePlanUsage(anniversaryPlan, turns, {
      pricing: PRICING,
      now: new Date('2026-04-20T00:00:00.000Z'),
    });
    assert.equal(u.spentUsd, 3);
  });

  it('skips turns with unparseable timestamps without throwing', () => {
    const turns: TurnRecord[] = [
      turn({ ts: 'garbage', inputTokens: 1_000_000 }),
      turn({ ts: '2026-04-05T00:00:00.000Z', inputTokens: 1_000_000 }),
    ];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    assert.equal(u.spentUsd, 3);
  });
});
