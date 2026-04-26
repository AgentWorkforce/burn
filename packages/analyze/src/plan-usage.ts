import type { DatabaseSync } from 'node:sqlite';

import type { Plan, PlanProvider } from '@relayburn/ledger';
import type { SourceKind, TurnRecord } from '@relayburn/reader';

import { costForTurn } from './cost.js';
import { emptyFidelitySummary, summarizeFidelity } from './fidelity.js';
import type { FidelitySummary } from './fidelity.js';
import type { PricingTable } from './pricing.js';

// Per-cycle confidence on the spent/projected totals. `high` when every
// contributing turn supplies per-turn input + output token coverage (i.e.
// `full` or `usage-only` with both axes present). Otherwise `low` — the cycle
// includes at least one `partial` / `aggregate-only` / `cost-only` turn, so
// the totals are a lower bound on actual spend. The accompanying `summary`
// is the same `FidelitySummary` shape `summarizeFidelity` emits for any
// other slice — kept here so JSON consumers can render exact counts without
// re-walking turns.
export interface PlanUsageFidelity {
  confidence: 'high' | 'low';
  summary: FidelitySummary;
}

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
  // Token-coverage confidence over the contributing turns this cycle. See
  // `PlanUsageFidelity`. When `confidence === 'low'`, `spentUsd` is a lower
  // bound — at least one turn lacked per-turn input/output token data, so
  // its priced contribution is missing or estimated. Renderers should
  // surface this so a "looks under budget" plan isn't read as authoritative.
  fidelity: PlanUsageFidelity;
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
  // Like `burn limits`, `plans` is allowed to count partial / aggregate-only /
  // cost-only contributions toward the cycle total — under-counting silently is
  // worse than annotating low-confidence. We collect the contributing turns'
  // fidelity blocks here so we can mark the whole cycle low-confidence below
  // when any of them lacks per-turn input/output coverage.
  const contributing: Array<Pick<TurnRecord, 'fidelity'>> = [];
  for (const t of turns) {
    if (!matchesProvider(plan.provider, t)) continue;
    const ts = Date.parse(t.ts);
    if (!Number.isFinite(ts)) continue;
    if (ts < cycleStartMs || ts >= cycleEndMs) continue;
    // exactOptionalPropertyTypes refuses an explicit `undefined` for the
    // optional `fidelity` field — only attach the property when present.
    contributing.push(t.fidelity ? { fidelity: t.fidelity } : {});
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
    fidelity: deriveFidelity(contributing),
  };
}

// `confidence === 'high'` when every contributing turn carries per-turn
// input + output token coverage — that is, `full` or `usage-only` with both
// axes present. A turn with no `fidelity` field at all (older ledger writers,
// pre-#41) is also treated as high; we have no signal to claim otherwise and
// elsewhere the codebase treats unknown as best-effort full. Empty cycles
// (no contributing turns) report high — there's nothing to be uncertain about.
function deriveFidelity(
  contributing: ReadonlyArray<Pick<TurnRecord, 'fidelity'>>,
): PlanUsageFidelity {
  if (contributing.length === 0) {
    return { confidence: 'high', summary: emptyFidelitySummary() };
  }
  const summary = summarizeFidelity(contributing);
  let confidence: 'high' | 'low' = 'high';
  for (const t of contributing) {
    const f = t.fidelity;
    if (!f) continue; // unknown → treat as high, matches summarizeFidelity policy
    if (f.class !== 'full' && f.class !== 'usage-only') {
      confidence = 'low';
      break;
    }
    if (!f.coverage.hasInputTokens || !f.coverage.hasOutputTokens) {
      confidence = 'low';
      break;
    }
  }
  return { confidence, summary };
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
      // Cursor spend is structurally unobservable from a local-first tool:
      // Cursor moved usage tracking server-side around 2026-01 and the
      // `tokenCount` fields in their local SQLite went to zero. See #22 for
      // the full investigation — the issue is closed wontfix. The cursor
      // preset is kept so users on Cursor Pro can still register their
      // monthly budget and reset day, but `spentUsd` will always be 0
      // (and the projection therefore $0) until Cursor reverses course.
      return turn.source === ('cursor' as TurnRecord['source']);
    case 'custom':
      // Custom plans count every turn the ledger has — the user opts in by
      // labelling their plan as `custom`.
      return true;
  }
}

export interface ComputePlanUsageFromArchiveOptions {
  pricing: PricingTable;
  /** Open archive handle. Caller owns the lifecycle. */
  db: DatabaseSync;
  now?: Date;
}

interface BucketRow {
  source: string;
  model: string;
  input: number | bigint;
  output: number | bigint;
  reasoning: number | bigint;
  cache_read: number | bigint;
  cache_5m: number | bigint;
  cache_1h: number | bigint;
}

/**
 * Compute `PlanUsage` for `plan` against the archive's `turns` table — one SQL
 * aggregate per call instead of a full ledger scan. Returns the same shape as
 * `computePlanUsage` so callers can treat the two interchangeably.
 *
 * The SQL groups by `(source, model)` because cost derivation needs both: the
 * per-source `reasoningModeForSource` override (Codex bills reasoning inside
 * `output_tokens`) and the per-model pricing rate live at different join
 * points and we want them composed exactly the way `costForTurn` would have.
 *
 * `cycleStart` / `cycleEnd` come from the same `cycleBounds` helper as the
 * in-memory path, so reset-day boundaries match byte-for-byte.
 */
export function planUsageFromArchive(
  plan: Plan,
  opts: ComputePlanUsageFromArchiveOptions,
): PlanUsage {
  const now = opts.now ?? new Date();
  const { cycleStart, cycleEnd } = cycleBounds(plan.resetDay, now);
  const cycleStartIso = cycleStart.toISOString();
  const cycleEndIso = cycleEnd.toISOString();
  const nowMs = now.getTime();
  const cycleStartMs = cycleStart.getTime();
  const cycleEndMs = cycleEnd.getTime();

  const sources = providerSources(plan.provider);
  // Matches `matchesProvider`'s "no rows" outcome: a provider whose source
  // list is empty (e.g. `cursor`, where the synthetic source is not in
  // `SourceKind`) should produce $0 spend without issuing a query whose
  // `IN ()` would be a SQL syntax error in some dialects.
  const rows: BucketRow[] = sources === null
    ? runQuery(opts.db, cycleStartIso, cycleEndIso, undefined)
    : sources.length === 0
      ? []
      : runQuery(opts.db, cycleStartIso, cycleEndIso, sources);

  let spent = 0;
  for (const row of rows) {
    // Reuse `costForTurn`'s source-aware reasoning override by going through
    // `costForUsage` with an explicit override. Keeps Codex `output_tokens`
    // from being double-billed against `usage.reasoning`.
    const synthetic: TurnRecord = {
      v: 1,
      source: row.source as SourceKind,
      sessionId: '',
      messageId: '',
      turnIndex: 0,
      ts: cycleStartIso,
      model: row.model,
      usage: {
        input: Number(row.input),
        output: Number(row.output),
        reasoning: Number(row.reasoning),
        cacheRead: Number(row.cache_read),
        cacheCreate5m: Number(row.cache_5m),
        cacheCreate1h: Number(row.cache_1h),
      },
      toolCalls: [],
    };
    const cost = costForTurn(synthetic, opts.pricing);
    if (cost) spent += cost.total;
  }

  const elapsedMs = Math.max(0, nowMs - cycleStartMs);
  const cycleMs = Math.max(1, cycleEndMs - cycleStartMs);
  const daysElapsed = Math.floor(elapsedMs / MS_PER_DAY);
  const daysInCycle = Math.max(1, Math.round(cycleMs / MS_PER_DAY));

  const fractionElapsed = elapsedMs / cycleMs;
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

/**
 * Translate a plan's provider into the `turns.source` values the SQL query
 * should match. `null` means "every source" (custom plans). An empty array
 * means "no source the ledger can produce" (cursor — see `matchesProvider`).
 */
function providerSources(provider: PlanProvider): SourceKind[] | null {
  switch (provider) {
    case 'claude':
      return ['claude-code', 'anthropic-api'];
    case 'cursor':
      // Same rationale as `matchesProvider`: no `SourceKind` value matches
      // 'cursor' so we never emit a query at all.
      return [];
    case 'custom':
      return null;
  }
}

function runQuery(
  db: DatabaseSync,
  cycleStartIso: string,
  cycleEndIso: string,
  sources: readonly SourceKind[] | undefined,
): BucketRow[] {
  // Use a half-open window `[start, end)` to match the in-memory path's
  // `ts < cycleEndMs` boundary, so a turn timestamped exactly at the next
  // cycle's start lands in the next cycle (not this one).
  const baseSql = `
    SELECT
      source,
      model,
      COALESCE(SUM(input_tokens), 0)             AS input,
      COALESCE(SUM(output_tokens), 0)            AS output,
      COALESCE(SUM(reasoning_tokens), 0)         AS reasoning,
      COALESCE(SUM(cache_read_tokens), 0)        AS cache_read,
      COALESCE(SUM(cache_create_5m_tokens), 0)   AS cache_5m,
      COALESCE(SUM(cache_create_1h_tokens), 0)   AS cache_1h
    FROM turns
    WHERE ts >= ? AND ts < ?`;
  if (sources === undefined) {
    const stmt = db.prepare(`${baseSql} GROUP BY source, model`);
    return stmt.all(cycleStartIso, cycleEndIso) as unknown as BucketRow[];
  }
  // node:sqlite parameter binding doesn't expand arrays, so build the
  // placeholders inline. `sources` is a closed enum (`SourceKind`) so this
  // is not a SQL-injection vector.
  const placeholders = sources.map(() => '?').join(', ');
  const stmt = db.prepare(
    `${baseSql} AND source IN (${placeholders}) GROUP BY source, model`,
  );
  return stmt.all(cycleStartIso, cycleEndIso, ...sources) as unknown as BucketRow[];
}
