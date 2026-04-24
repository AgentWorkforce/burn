import type { Plan } from '@relayburn/ledger';
import type { TurnRecord } from '@relayburn/reader';

import { costForTurn } from './cost.js';
import type { PricingTable } from './pricing.js';

export interface PlanUsage {
  plan: Plan;
  cycleStart: Date;
  cycleEnd: Date;
  spentUsd: number;
  daysElapsed: number;
  daysInCycle: number;
  // Linear extrapolation: spent / daysElapsed × daysInCycle. Equals spentUsd
  // when the cycle just started (daysElapsed === 0) so the projection never
  // collapses to zero or NaN.
  projectedEndOfCycleUsd: number;
  overBudget: boolean;
  // Days of budget left at the current daily-spend rate. null when daily
  // spend is zero (no observable rate yet) or when projection is under
  // budget — runway is "until the wall," not "until the cycle ends." When
  // the projection sits under budget, the runway exceeds the cycle length
  // and the cycle resets first, which makes runway uninteresting.
  runwayDays: number | null;
  resetAt: string;
  // True when the cycle has fewer than this many days of observed data.
  // Renderers should mark these projections as "limited data" per #39's
  // acceptance criteria.
  limitedData: boolean;
}

const MS_PER_DAY = 24 * 60 * 60 * 1000;
const LIMITED_DATA_DAYS = 7;

export interface ComputePlanUsageOptions {
  pricing: PricingTable;
  now?: Date;
}

export function computePlanUsage(
  plan: Plan,
  turns: Iterable<TurnRecord>,
  opts: ComputePlanUsageOptions,
): PlanUsage {
  const now = opts.now ?? new Date();
  const { cycleStart, cycleEnd } = cycleBounds(plan.resetDay, now);
  const cycleStartMs = cycleStart.getTime();
  const cycleEndMs = cycleEnd.getTime();
  const nowMs = now.getTime();

  let spent = 0;
  for (const t of turns) {
    if (!matchesProvider(plan.provider, t)) continue;
    const ts = Date.parse(t.ts);
    if (!Number.isFinite(ts)) continue;
    if (ts < cycleStartMs || ts >= cycleEndMs) continue;
    const cost = costForTurn(t, opts.pricing);
    if (cost) spent += cost.total;
  }

  const elapsedMs = Math.max(0, nowMs - cycleStartMs);
  const cycleMs = Math.max(1, cycleEndMs - cycleStartMs);
  const daysElapsed = Math.floor(elapsedMs / MS_PER_DAY);
  const daysInCycle = Math.max(1, Math.round(cycleMs / MS_PER_DAY));

  const fractionElapsed = elapsedMs / cycleMs;
  // Linear projection from observed spend across the elapsed slice. When
  // the cycle just started, fall back to the spent total so the projection
  // never explodes to infinity.
  const projected = fractionElapsed > 0 ? spent / fractionElapsed : spent;

  const overBudget = projected > plan.budgetUsd;

  let runwayDays: number | null = null;
  if (overBudget && elapsedMs > 0) {
    const dailyRate = spent / (elapsedMs / MS_PER_DAY);
    if (dailyRate > 0) {
      const remaining = Math.max(0, plan.budgetUsd - spent);
      runwayDays = Math.floor(remaining / dailyRate);
    }
  }

  return {
    plan,
    cycleStart,
    cycleEnd,
    spentUsd: spent,
    daysElapsed,
    daysInCycle,
    projectedEndOfCycleUsd: projected,
    overBudget,
    runwayDays,
    resetAt: cycleEnd.toISOString(),
    limitedData: daysElapsed < LIMITED_DATA_DAYS,
  };
}

// Returns the [start, end) window for the cycle containing `now`. The
// start is the most recent occurrence of resetDay (clamped to the month's
// last day if resetDay > month length); the end is the next occurrence.
// Both are anchored to UTC midnight so the boundary is unambiguous across
// timezones.
export function cycleBounds(resetDay: number, now: Date): { cycleStart: Date; cycleEnd: Date } {
  const utcYear = now.getUTCFullYear();
  const utcMonth = now.getUTCMonth();

  // Candidate start in the *current* UTC month.
  const thisMonthStart = makeCycleAnchor(utcYear, utcMonth, resetDay);

  let cycleStart: Date;
  if (now.getTime() >= thisMonthStart.getTime()) {
    cycleStart = thisMonthStart;
  } else {
    // We're before this month's anchor → cycle started in the previous month.
    cycleStart = makeCycleAnchor(utcYear, utcMonth - 1, resetDay);
  }
  const cycleEnd = makeCycleAnchor(
    cycleStart.getUTCFullYear(),
    cycleStart.getUTCMonth() + 1,
    resetDay,
  );
  // Defensive: a month boundary that lands the same calendar day (DST/leap
  // shift edge case) shouldn't produce a zero-length cycle.
  if (cycleEnd.getTime() <= cycleStart.getTime()) {
    return { cycleStart, cycleEnd: new Date(cycleStart.getTime() + 28 * MS_PER_DAY) };
  }
  return { cycleStart, cycleEnd };
}

function makeCycleAnchor(year: number, month: number, day: number): Date {
  // Date.UTC handles month over/underflow (month -1 → previous December,
  // month 12 → next January). For day, we manually clamp to the actual
  // month length so resetDay=31 in February resolves to Feb 28/29.
  const normYear = year + Math.floor(month / 12);
  const normMonth = ((month % 12) + 12) % 12;
  const lastOfMonth = new Date(Date.UTC(normYear, normMonth + 1, 0)).getUTCDate();
  const clampedDay = Math.min(day, lastOfMonth);
  return new Date(Date.UTC(normYear, normMonth, clampedDay));
}

function matchesProvider(provider: Plan['provider'], turn: TurnRecord): boolean {
  switch (provider) {
    case 'claude':
      return turn.source === 'claude-code' || turn.source === 'anthropic-api';
    case 'cursor':
      // No reader emits `cursor` turns yet (see SourceKind in @relayburn/reader);
      // this branch will start picking up spend once a Cursor adapter lands.
      return turn.source === ('cursor' as TurnRecord['source']);
    case 'custom':
      // Custom plans count every turn the ledger has — the user opts in by
      // labelling their plan as `custom`.
      return true;
  }
}
