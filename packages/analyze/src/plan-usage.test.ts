import { strict as assert } from 'node:assert';
import { DatabaseSync } from 'node:sqlite';
import { describe, it } from 'node:test';

import type { Plan } from '@relayburn/ledger';
import { EMPTY_COVERAGE, makeFidelity } from '@relayburn/reader';
import type { Fidelity, SourceKind, TurnRecord } from '@relayburn/reader';

import { computePlanUsage, cycleBounds, planUsageFromArchive } from './plan-usage.js';
import type { PricingTable } from './pricing.js';

const FULL_FIDELITY: Fidelity = makeFidelity('per-turn', {
  ...EMPTY_COVERAGE,
  hasInputTokens: true,
  hasOutputTokens: true,
  hasCacheReadTokens: true,
  hasToolCalls: true,
  hasToolResultEvents: true,
  hasSessionRelationships: true,
});

const USAGE_ONLY_FIDELITY: Fidelity = makeFidelity('per-turn', {
  ...EMPTY_COVERAGE,
  hasInputTokens: true,
  hasOutputTokens: true,
});

const PARTIAL_FIDELITY: Fidelity = makeFidelity('per-turn', {
  ...EMPTY_COVERAGE,
  hasInputTokens: true,
  // missing output → "partial"
});

const COST_ONLY_FIDELITY: Fidelity = makeFidelity('cost-only', {
  ...EMPTY_COVERAGE,
});

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
  fidelity?: Fidelity;
}): TurnRecord {
  // exactOptionalPropertyTypes refuses an explicit `undefined` for the
  // optional `fidelity` field — only attach when present.
  const base: TurnRecord = {
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
  return opts.fidelity ? { ...base, fidelity: opts.fidelity } : base;
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

  // Issue #108: fidelity-aware totals. The plan view continues to count every
  // turn that lands in the cycle (no fidelity-based filter — `plans`, like
  // `limits`, is permissive), but annotates the cycle as low-confidence when
  // any contributing turn lacks per-turn input/output token coverage.
  it('reports high-confidence fidelity when every cycle turn is full', () => {
    const turns: TurnRecord[] = [
      turn({
        ts: '2026-04-05T00:00:00.000Z',
        inputTokens: 1_000_000,
        fidelity: FULL_FIDELITY,
      }),
      turn({
        ts: '2026-04-10T00:00:00.000Z',
        inputTokens: 1_000_000,
        fidelity: FULL_FIDELITY,
      }),
    ];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    assert.equal(u.spentUsd, 6);
    assert.equal(u.fidelity.confidence, 'high');
    assert.equal(u.fidelity.summary.total, 2);
    assert.equal(u.fidelity.summary.byClass.full, 2);
  });

  it('treats usage-only (per-turn input + output) cycles as high-confidence', () => {
    const turns: TurnRecord[] = [
      turn({
        ts: '2026-04-05T00:00:00.000Z',
        inputTokens: 1_000_000,
        outputTokens: 1_000_000,
        fidelity: USAGE_ONLY_FIDELITY,
      }),
    ];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    assert.equal(u.fidelity.confidence, 'high');
  });

  it('treats turns without fidelity (older ledger writers) as high-confidence', () => {
    // Backward-compat: pre-#41 records have no fidelity field at all and are
    // best-effort full per the codebase convention. Don't demote a cycle to
    // low-confidence purely because the writer was old.
    const turns: TurnRecord[] = [
      turn({ ts: '2026-04-05T00:00:00.000Z', inputTokens: 1_000_000 }),
    ];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    assert.equal(u.fidelity.confidence, 'high');
  });

  it('marks low-confidence when a cycle has any partial-fidelity turn', () => {
    const turns: TurnRecord[] = [
      turn({
        ts: '2026-04-05T00:00:00.000Z',
        inputTokens: 1_000_000,
        outputTokens: 1_000_000,
        fidelity: FULL_FIDELITY,
      }),
      // Partial: input known, output missing — its priced contribution is a
      // lower bound. Cycle total still includes it.
      turn({
        ts: '2026-04-10T00:00:00.000Z',
        inputTokens: 500_000,
        fidelity: PARTIAL_FIDELITY,
      }),
    ];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    // Spend still counts both turns: 1M input + 1M output ($3 + $15) + 500k input ($1.5)
    assert.equal(u.spentUsd, 19.5);
    assert.equal(u.fidelity.confidence, 'low');
    assert.equal(u.fidelity.summary.total, 2);
    assert.equal(u.fidelity.summary.byClass.full, 1);
    assert.equal(u.fidelity.summary.byClass.partial, 1);
    assert.equal(u.fidelity.summary.missingCoverage.hasOutputTokens, 1);
  });

  it('counts cost-only contributions toward spend and marks the cycle low-confidence', () => {
    // A `cost-only` source provides a price (here: via priced tokens on the
    // turn) but no per-turn token coverage. Spend totals include it; the
    // cycle is flagged low-confidence on the token-coverage axis.
    const turns: TurnRecord[] = [
      turn({
        ts: '2026-04-05T00:00:00.000Z',
        inputTokens: 1_000_000,
        outputTokens: 1_000_000,
        fidelity: FULL_FIDELITY,
      }),
      turn({
        ts: '2026-04-10T00:00:00.000Z',
        inputTokens: 1_000_000, // priced contribution, but fidelity says "cost-only"
        fidelity: COST_ONLY_FIDELITY,
      }),
    ];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    // 1M input + 1M output = $3 + $15 = $18; then cost-only 1M input = $3 → $21
    assert.equal(u.spentUsd, 21);
    assert.equal(u.fidelity.confidence, 'low');
    assert.equal(u.fidelity.summary.byClass['cost-only'], 1);
  });

  it('reports an empty cycle as high-confidence (nothing to be uncertain about)', () => {
    const u = computePlanUsage(plan, [], { pricing: PRICING, now });
    assert.equal(u.fidelity.confidence, 'high');
    assert.equal(u.fidelity.summary.total, 0);
  });

  it('ignores fidelity of turns outside the cycle when deciding confidence', () => {
    const turns: TurnRecord[] = [
      // In-cycle, full fidelity:
      turn({
        ts: '2026-04-05T00:00:00.000Z',
        inputTokens: 1_000_000,
        outputTokens: 1_000_000,
        fidelity: FULL_FIDELITY,
      }),
      // Out-of-cycle (previous month), partial: must NOT drag the cycle down.
      turn({
        ts: '2026-03-20T00:00:00.000Z',
        inputTokens: 1_000_000,
        fidelity: PARTIAL_FIDELITY,
      }),
    ];
    const u = computePlanUsage(plan, turns, { pricing: PRICING, now });
    assert.equal(u.fidelity.confidence, 'high');
    assert.equal(u.fidelity.summary.total, 1);
  });
});

// Minimal subset of the real `archive.sqlite` `turns` schema — just the
// columns `planUsageFromArchive` reads. Built per-test in :memory: so we
// don't need a real archive build / RELAYBURN_HOME shuffle.
const ARCHIVE_TURNS_DDL = `
  CREATE TABLE turns (
    source                  TEXT NOT NULL,
    session_id              TEXT NOT NULL,
    message_id              TEXT NOT NULL,
    ts                      TEXT NOT NULL,
    model                   TEXT NOT NULL,
    input_tokens            INTEGER NOT NULL DEFAULT 0,
    output_tokens           INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens        INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens       INTEGER NOT NULL DEFAULT 0,
    cache_create_5m_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_create_1h_tokens  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (source, session_id, message_id)
  );
  CREATE INDEX idx_turns_ts ON turns(ts);
`;

interface ArchiveTurnRow {
  source: SourceKind;
  ts: string;
  model: string;
  inputTokens?: number;
  outputTokens?: number;
  reasoningTokens?: number;
  cacheReadTokens?: number;
  cacheCreate5mTokens?: number;
  cacheCreate1hTokens?: number;
}

function makeArchive(rows: ArchiveTurnRow[]): DatabaseSync {
  const db = new DatabaseSync(':memory:');
  db.exec(ARCHIVE_TURNS_DDL);
  const insert = db.prepare(`
    INSERT INTO turns (
      source, session_id, message_id, ts, model,
      input_tokens, output_tokens, reasoning_tokens,
      cache_read_tokens, cache_create_5m_tokens, cache_create_1h_tokens
    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
  `);
  for (let i = 0; i < rows.length; i++) {
    const r = rows[i]!;
    insert.run(
      r.source,
      `s-${i}`,
      `m-${i}`,
      r.ts,
      r.model,
      r.inputTokens ?? 0,
      r.outputTokens ?? 0,
      r.reasoningTokens ?? 0,
      r.cacheReadTokens ?? 0,
      r.cacheCreate5mTokens ?? 0,
      r.cacheCreate1hTokens ?? 0,
    );
  }
  return db;
}

describe('planUsageFromArchive', () => {
  const now = new Date('2026-04-15T00:00:00.000Z'); // 14 days into a calendar cycle

  it('parity with computePlanUsage on the same fixture', () => {
    const fixtureTurns: TurnRecord[] = [
      turn({ ts: '2026-04-05T00:00:00.000Z', inputTokens: 1_000_000 }),
      turn({ ts: '2026-04-10T00:00:00.000Z', inputTokens: 1_000_000 }),
      // Outside cycle: previous month
      turn({ ts: '2026-03-25T00:00:00.000Z', inputTokens: 5_000_000 }),
    ];
    const memUsage = computePlanUsage(plan, fixtureTurns, { pricing: PRICING, now });

    const db = makeArchive(
      fixtureTurns.map((t) => ({
        source: t.source,
        ts: t.ts,
        model: t.model,
        inputTokens: t.usage.input,
        outputTokens: t.usage.output,
      })),
    );
    try {
      const archiveUsage = planUsageFromArchive(plan, { pricing: PRICING, db, now });
      // Byte-identical PlanUsage shape on the parity fixture.
      assert.deepEqual(archiveUsage, memUsage);
    } finally {
      db.close();
    }
  });

  it('reset-day boundary: turn at cycleEnd lands in the next cycle', () => {
    // resetDay=1 cycle for `now=2026-04-15` is [2026-04-01, 2026-05-01).
    // A turn at exactly 2026-05-01T00:00:00.000Z must NOT count toward this
    // cycle (matches the `< cycleEndMs` half-open in `computePlanUsage`).
    const db = makeArchive([
      // boundary-low: first instant of the cycle → counted
      {
        source: 'claude-code',
        ts: '2026-04-01T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
      // strictly inside
      {
        source: 'claude-code',
        ts: '2026-04-14T23:59:59.999Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
      // boundary-high: cycle end → next cycle, must be excluded
      {
        source: 'claude-code',
        ts: '2026-05-01T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
      // far past
      {
        source: 'claude-code',
        ts: '2026-03-31T23:59:59.999Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
    ]);
    try {
      const u = planUsageFromArchive(plan, { pricing: PRICING, db, now });
      // 2 in-window × $3 = $6
      assert.equal(u.spentUsd, 6);
    } finally {
      db.close();
    }
  });

  it('claude provider only counts claude-code/anthropic-api turns', () => {
    const db = makeArchive([
      {
        source: 'claude-code',
        ts: '2026-04-05T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
      {
        source: 'anthropic-api',
        ts: '2026-04-06T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
      {
        source: 'codex',
        ts: '2026-04-07T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
    ]);
    try {
      const u = planUsageFromArchive(plan, { pricing: PRICING, db, now });
      // 2 claude turns × $3 = $6, codex excluded
      assert.equal(u.spentUsd, 6);
    } finally {
      db.close();
    }
  });

  it('cursor provider returns $0 without issuing a query against unknown sources', () => {
    const cursorPlan: Plan = { ...plan, provider: 'cursor', id: 'cursor-pro' };
    const db = makeArchive([
      // Even if some hypothetical source called 'cursor' lived in the table,
      // the helper short-circuits on the empty source list — see
      // `providerSources('cursor')`.
      {
        source: 'claude-code',
        ts: '2026-04-05T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 5_000_000,
      },
    ]);
    try {
      const u = planUsageFromArchive(cursorPlan, { pricing: PRICING, db, now });
      assert.equal(u.spentUsd, 0);
      assert.equal(u.projectedEndOfCycleUsd, 0);
    } finally {
      db.close();
    }
  });

  it('custom provider counts every source', () => {
    const customPlan: Plan = { ...plan, provider: 'custom' };
    const db = makeArchive([
      {
        source: 'claude-code',
        ts: '2026-04-05T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
      {
        source: 'codex',
        ts: '2026-04-06T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
      {
        source: 'opencode',
        ts: '2026-04-07T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
    ]);
    try {
      const u = planUsageFromArchive(customPlan, { pricing: PRICING, db, now });
      assert.equal(u.spentUsd, 9);
    } finally {
      db.close();
    }
  });

  it('flags limitedData when fewer than 7 days have elapsed', () => {
    const earlyNow = new Date('2026-04-04T00:00:00.000Z');
    const db = makeArchive([]);
    try {
      const u = planUsageFromArchive(plan, { pricing: PRICING, db, now: earlyNow });
      assert.equal(u.daysElapsed, 3);
      assert.equal(u.limitedData, true);
    } finally {
      db.close();
    }
  });

  it('does not double-bill Codex reasoning tokens (uses same source override as costForTurn)', () => {
    const customPlan: Plan = { ...plan, provider: 'custom' };
    // 1M output × $15 = $15. With reasoning double-billed at the output rate
    // we'd see $15 + $15 = $30; the source-aware override keeps it at $15.
    const db = makeArchive([
      {
        source: 'codex',
        ts: '2026-04-05T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        outputTokens: 1_000_000,
        reasoningTokens: 1_000_000,
      },
    ]);
    try {
      const u = planUsageFromArchive(customPlan, { pricing: PRICING, db, now });
      assert.equal(u.spentUsd, 15);
    } finally {
      db.close();
    }
  });

  it('honors a custom resetDay (anniversary cycle)', () => {
    const anniversaryPlan: Plan = { ...plan, resetDay: 15 };
    // now = April 20, cycle started April 15, ends May 15
    const db = makeArchive([
      // inside
      {
        source: 'claude-code',
        ts: '2026-04-16T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 1_000_000,
      },
      // before cycle start — excluded
      {
        source: 'claude-code',
        ts: '2026-04-10T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 5_000_000,
      },
    ]);
    try {
      const u = planUsageFromArchive(anniversaryPlan, {
        pricing: PRICING,
        db,
        now: new Date('2026-04-20T00:00:00.000Z'),
      });
      assert.equal(u.spentUsd, 3);
    } finally {
      db.close();
    }
  });

  it('groups by (source, model) so multi-model spend aggregates correctly', () => {
    const customPlan: Plan = { ...plan, provider: 'custom' };
    const pricing: PricingTable = {
      ...PRICING,
      'gpt-5-mini': {
        input: 1,
        output: 5,
        cacheRead: 0.1,
        cacheWrite: 1.25,
        reasoningMode: 'same_as_output',
      },
    };
    const db = makeArchive([
      {
        source: 'claude-code',
        ts: '2026-04-05T00:00:00.000Z',
        model: 'claude-sonnet-4-6',
        inputTokens: 2_000_000,
      },
      {
        source: 'codex',
        ts: '2026-04-06T00:00:00.000Z',
        model: 'gpt-5-mini',
        outputTokens: 1_000_000,
      },
    ]);
    try {
      const u = planUsageFromArchive(customPlan, { pricing, db, now });
      // 2M input × $3 = $6 (claude) + 1M output × $5 = $5 (gpt-5-mini) = $11
      assert.equal(u.spentUsd, 11);
    } finally {
      db.close();
    }
  });
});
