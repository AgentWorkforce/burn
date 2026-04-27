import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { emptyFidelitySummary } from '@relayburn/analyze';

import {
  makeCachingFetcher,
  runLimits,
  type ForecastInput,
  type LimitsDeps,
  type UsageResponse,
} from './limits.js';
import type { ParsedArgs } from '../args.js';

async function captureStdout<T>(
  fn: () => Promise<T>,
): Promise<{ result: T; stdout: string; stderr: string }> {
  let stdout = '';
  let stderr = '';
  const origOut = process.stdout.write.bind(process.stdout);
  const origErr = process.stderr.write.bind(process.stderr);
  process.stdout.write = ((c: string | Uint8Array) => {
    stdout += typeof c === 'string' ? c : Buffer.from(c).toString('utf8');
    return true;
  }) as typeof process.stdout.write;
  process.stderr.write = ((c: string | Uint8Array) => {
    stderr += typeof c === 'string' ? c : Buffer.from(c).toString('utf8');
    return true;
  }) as typeof process.stderr.write;
  try {
    const result = await fn();
    return { result, stdout, stderr };
  } finally {
    process.stdout.write = origOut;
    process.stderr.write = origErr;
  }
}

function args(flags: Record<string, string | true> = {}): ParsedArgs {
  return { flags, tags: {}, positional: [], passthrough: [] };
}

const FIXED_NOW = new Date('2026-04-24T12:00:00.000Z');

function fakeNow(): Date {
  return new Date(FIXED_NOW);
}

function noTokenDeps(): LimitsDeps {
  return {
    loadToken: async () => null,
    fetchUsage: async () => ({}),
    now: fakeNow,
    loadForecast: async () => null,
    loadPlanStatuses: async () => [],
  };
}

function tokenDeps(usage: UsageResponse, forecast: ForecastInput | null = null): LimitsDeps {
  return {
    loadToken: async () => 'fake-token',
    fetchUsage: async () => usage,
    now: fakeNow,
    loadForecast: async () => forecast,
    loadPlanStatuses: async () => [],
  };
}

describe('burn limits', () => {
  it('exits 2 with one-line guidance on stderr when token missing', async () => {
    const { result, stdout, stderr } = await captureStdout(() =>
      runLimits(args(), noTokenDeps()),
    );
    assert.equal(result, 2);
    assert.equal(stdout, '');
    assert.equal(stderr.split('\n').filter(Boolean).length, 1);
    assert.match(stderr, /no Claude OAuth token found/);
    assert.match(stderr, /CLAUDE_CODE_OAUTH_TOKEN/);
  });

  it('renders five_hour, seven_day, seven_day_opus, extra_usage with reset countdowns', async () => {
    const usage: UsageResponse = {
      five_hour: { percent_used: 34, reset_at: '2026-04-24T14:14:00.000Z' },
      seven_day: { percent_used: 12, reset_at: '2026-04-28T18:00:00.000Z' },
      seven_day_opus: { percent_used: 8, reset_at: '2026-04-28T18:00:00.000Z' },
      extra_usage: { percent_used: 0, reset_at: '2026-04-28T18:00:00.000Z' },
    };
    const { result, stdout } = await captureStdout(() => runLimits(args(), tokenDeps(usage)));
    assert.equal(result, 0);
    // 5-hour: reset 2h 14m from FIXED_NOW
    assert.match(stdout, /5-hour\s+34% used\s+resets in 2h 14m/);
    assert.match(stdout, /7-day\s+12% used\s+resets in 4d 6h/);
    assert.match(stdout, /7-day Opus\s+8% used\s+resets in 4d 6h/);
    assert.match(stdout, /extra\s+0% used/);
  });

  it('handles fractional percent_used (0..1) by scaling to 0..100', async () => {
    const usage: UsageResponse = {
      five_hour: { percent_used: 0.42, reset_at: '2026-04-24T13:00:00.000Z' },
    };
    const { stdout } = await captureStdout(() => runLimits(args(), tokenDeps(usage)));
    assert.match(stdout, /5-hour\s+42% used/);
  });

  it('emits JSON with --json including usage and forecast', async () => {
    const usage: UsageResponse = {
      five_hour: { percent_used: 40, reset_at: '2026-04-24T14:00:00.000Z' },
    };
    const forecast: ForecastInput = {
      tokensSoFar: 600_000,
      elapsedMs: 2 * 60 * 60 * 1000, // 2h
      remainingMs: 2 * 60 * 60 * 1000, // 2h
    };
    const { result, stdout } = await captureStdout(() =>
      runLimits(args({ json: true }), tokenDeps(usage, forecast)),
    );
    assert.equal(result, 0);
    const parsed = JSON.parse(stdout);
    assert.equal(parsed.usage.five_hour.percent_used, 40);
    assert.equal(parsed.forecast.tokensSoFar, 600_000);
    // 600k tokens / 120 minutes = 5000 tok/min
    assert.equal(parsed.forecast.burnRateTokensPerMinute, 5000);
    // 40% at 2h elapsed of 4h total → projected 80% at reset
    assert.equal(parsed.forecast.projectedPercentAtReset, 80);
  });

  it('reports api errors without crashing and exits non-zero', async () => {
    const deps: LimitsDeps = {
      loadToken: async () => 'tok',
      fetchUsage: async () => {
        throw new Error('usage endpoint 401: unauthorized');
      },
      now: fakeNow,
      loadForecast: async () => null,
    };
    const { result, stdout } = await captureStdout(() => runLimits(args(), deps));
    assert.equal(result, 1);
    assert.match(stdout, /api error: usage endpoint 401/);
  });

  it('--no-api skips OAuth and renders local-only forecast', async () => {
    const forecast: ForecastInput = {
      tokensSoFar: 60_000,
      elapsedMs: 60 * 60 * 1000, // 1h
      remainingMs: 4 * 60 * 60 * 1000, // 4h
    };
    const deps: LimitsDeps = {
      loadToken: async () => {
        throw new Error('should not be called when --no-api');
      },
      fetchUsage: async () => {
        throw new Error('should not be called when --no-api');
      },
      now: fakeNow,
      loadForecast: async () => forecast,
    };
    const { result, stdout } = await captureStdout(() =>
      runLimits(args({ 'no-api': true }), deps),
    );
    assert.equal(result, 0);
    // 60k / 60min = 1000 tok/min
    assert.match(stdout, /burn rate 1\.0k tok\/min/);
    // No projected % without OAuth baseline
    assert.doesNotMatch(stdout, /projected/);
  });

  it('--no-forecast skips ledger read', async () => {
    const usage: UsageResponse = {
      five_hour: { percent_used: 50, reset_at: '2026-04-24T13:00:00.000Z' },
    };
    let forecastCalled = false;
    const deps: LimitsDeps = {
      loadToken: async () => 'tok',
      fetchUsage: async () => usage,
      now: fakeNow,
      loadForecast: async () => {
        forecastCalled = true;
        return null;
      },
    };
    const { result, stdout } = await captureStdout(() =>
      runLimits(args({ 'no-forecast': true }), deps),
    );
    assert.equal(result, 0);
    assert.equal(forecastCalled, false);
    assert.doesNotMatch(stdout, /Forecast/);
  });

  it('--watch with non-numeric value exits 2', async () => {
    const { result, stderr } = await captureStdout(() =>
      runLimits(args({ watch: 'abc' }), tokenDeps({})),
    );
    assert.equal(result, 2);
    assert.match(stderr, /invalid --watch value/);
  });

  it('renders Monthly plan block when a plan status is provided', async () => {
    const usage: UsageResponse = {
      five_hour: { percent_used: 30, reset_at: '2026-04-24T13:00:00.000Z' },
    };
    const deps: LimitsDeps = {
      loadToken: async () => 'tok',
      fetchUsage: async () => usage,
      now: fakeNow,
      loadForecast: async () => null,
      loadPlanStatuses: async () => [
        {
          usage: {
            plan: {
              id: 'claude-max',
              provider: 'claude',
              name: 'Claude Max',
              budgetUsd: 200,
              resetDay: 1,
            },
            cycleStart: new Date('2026-04-01T00:00:00.000Z'),
            cycleEnd: new Date('2026-05-01T00:00:00.000Z'),
            spentUsd: 87.42,
            daysElapsed: 13,
            daysInCycle: 30,
            projectedEndOfCycleUsd: 201.73,
            overBudget: true,
            runwayDays: 29,
            resetAt: '2026-05-01T00:00:00.000Z',
            limitedData: false,
            fidelity: { confidence: 'high', summary: emptyFidelitySummary() },
          },
        },
      ],
    };
    const { stdout } = await captureStdout(() => runLimits(args(), deps));
    assert.match(stdout, /Monthly plan \(Claude Max\):/);
    // 87.42 / 200 = 43.71% → rounds to 44%
    assert.match(stdout, /Spent:\s+\$87\.42 \/ \$200\.00\s+\(44%\)/);
    assert.match(stdout, /Elapsed:\s+13 \/ 30 days/);
    assert.match(stdout, /Projected: \$201.73 end-of-cycle \(\$1\.73 over\)/);
    assert.match(stdout, /Runway:\s+29 more days/);
  });

  it('annotates plan projection as "(limited data)" when daysElapsed < 7', async () => {
    const deps: LimitsDeps = {
      loadToken: async () => 'tok',
      fetchUsage: async () => ({}),
      now: fakeNow,
      loadForecast: async () => null,
      loadPlanStatuses: async () => [
        {
          usage: {
            plan: {
              id: 'claude-pro',
              provider: 'claude',
              name: 'Claude Pro',
              budgetUsd: 20,
              resetDay: 1,
            },
            cycleStart: new Date('2026-04-22T00:00:00.000Z'),
            cycleEnd: new Date('2026-05-22T00:00:00.000Z'),
            spentUsd: 1,
            daysElapsed: 2,
            daysInCycle: 30,
            projectedEndOfCycleUsd: 15,
            overBudget: false,
            runwayDays: null,
            resetAt: '2026-05-22T00:00:00.000Z',
            limitedData: true,
            fidelity: { confidence: 'high', summary: emptyFidelitySummary() },
          },
        },
      ],
    };
    const { stdout } = await captureStdout(() => runLimits(args(), deps));
    assert.match(stdout, /\(limited data\)/);
  });

  it('survives loadPlanStatuses throwing (malformed plans.json) and warns on stderr', async () => {
    // Regression for the unprotected loadPlanStatuses call: a malformed
    // user-editable plans.json should warn and degrade to an empty list,
    // not crash the command (especially under --watch).
    const usage: UsageResponse = {
      five_hour: { percent_used: 30, reset_at: '2026-04-24T13:00:00.000Z' },
    };
    const deps: LimitsDeps = {
      loadToken: async () => 'tok',
      fetchUsage: async () => usage,
      now: fakeNow,
      loadForecast: async () => null,
      loadPlanStatuses: async () => {
        throw new Error('invalid JSON in /home/u/.relayburn/plans.json: Unexpected token');
      },
    };
    const { result, stdout, stderr } = await captureStdout(() => runLimits(args(), deps));
    assert.equal(result, 0);
    assert.match(stderr, /could not load plans.*invalid JSON/);
    // 5-hour quota block must still render — the OAuth fetch isn't gated on plans.
    assert.match(stdout, /5-hour\s+30% used/);
    assert.doesNotMatch(stdout, /Monthly plan/);
  });

  it('emits plan statuses in --json output', async () => {
    const deps: LimitsDeps = {
      loadToken: async () => 'tok',
      fetchUsage: async () => ({}),
      now: fakeNow,
      loadForecast: async () => null,
      loadPlanStatuses: async () => [
        {
          usage: {
            plan: {
              id: 'claude-pro',
              provider: 'claude',
              name: 'Claude Pro',
              budgetUsd: 20,
              resetDay: 1,
            },
            cycleStart: new Date('2026-04-01T00:00:00.000Z'),
            cycleEnd: new Date('2026-05-01T00:00:00.000Z'),
            spentUsd: 5.5,
            daysElapsed: 23,
            daysInCycle: 30,
            projectedEndOfCycleUsd: 7.17,
            overBudget: false,
            runwayDays: null,
            resetAt: '2026-05-01T00:00:00.000Z',
            limitedData: false,
            fidelity: { confidence: 'high', summary: emptyFidelitySummary() },
          },
        },
      ],
    };
    const { stdout } = await captureStdout(() => runLimits(args({ json: true }), deps));
    const parsed = JSON.parse(stdout);
    assert.equal(parsed.plans.length, 1);
    assert.equal(parsed.plans[0].id, 'claude-pro');
    assert.equal(parsed.plans[0].budgetUsd, 20);
    assert.equal(parsed.plans[0].limitedData, false);
  });

  it('renders very-low projected % without double-normalizing back to 0..1', async () => {
    // Regression: projectFromOauth returns a value already on the 0..100 scale
    // (and capped at 100). If the renderer pipes that through the same
    // formatter that auto-detects 0..1 fractions, a 1.01% projection becomes
    // "101%". Pin down the rendered string here.
    const usage: UsageResponse = {
      // 0.01% used after ~99% of the window elapsed → projected ~1%.
      five_hour: { percent_used: 0.01, reset_at: '2026-04-24T12:01:48.000Z' },
    };
    const forecast: ForecastInput = {
      tokensSoFar: 1,
      elapsedMs: (5 * 60 * 60 - 108) * 1000, // window minus the 108s till reset
      remainingMs: 108 * 1000,
    };
    const { stdout } = await captureStdout(() =>
      runLimits(args(), tokenDeps(usage, forecast)),
    );
    assert.match(stdout, /projected 1% at reset/);
    assert.doesNotMatch(stdout, /projected (10|100|101)% at reset/);
  });
});

describe('makeCachingFetcher', () => {
  const baseTime = Date.parse('2026-04-24T12:00:00.000Z');

  it('returns the cached value within the TTL without re-invoking the fetcher', async () => {
    let calls = 0;
    let clock = baseTime;
    const fetcher = makeCachingFetcher(
      async () => {
        calls++;
        return { five_hour: { percent_used: 10, reset_at: '2026-04-24T13:00:00.000Z' } };
      },
      30_000,
      () => new Date(clock),
    );
    await fetcher('tok');
    clock += 10_000;
    await fetcher('tok');
    clock += 10_000;
    await fetcher('tok');
    assert.equal(calls, 1);
  });

  it('re-fetches once the TTL elapses', async () => {
    let calls = 0;
    let clock = baseTime;
    const fetcher = makeCachingFetcher(
      async () => {
        calls++;
        return {};
      },
      30_000,
      () => new Date(clock),
    );
    await fetcher('tok');
    clock += 31_000;
    await fetcher('tok');
    assert.equal(calls, 2);
  });

  it('does not serve cache across distinct tokens', async () => {
    let calls = 0;
    const fetcher = makeCachingFetcher(
      async () => {
        calls++;
        return {};
      },
      30_000,
      () => new Date(baseTime),
    );
    await fetcher('tok-a');
    await fetcher('tok-b');
    await fetcher('tok-a'); // cache is single-slot, so token-a now misses too
    assert.equal(calls, 3);
  });
});
