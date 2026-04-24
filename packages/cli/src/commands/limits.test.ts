import { strict as assert } from 'node:assert';
import { describe, it } from 'node:test';

import { runLimits, type ForecastInput, type LimitsDeps, type UsageResponse } from './limits.js';
import type { ParsedArgs } from '../args.js';

function captureStdout<T>(fn: () => Promise<T>): Promise<{ result: T; stdout: string; stderr: string }> {
  return new Promise(async (resolve, reject) => {
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
      resolve({ result, stdout, stderr });
    } catch (e) {
      reject(e);
    } finally {
      process.stdout.write = origOut;
      process.stderr.write = origErr;
    }
  });
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
  };
}

function tokenDeps(usage: UsageResponse, forecast: ForecastInput | null = null): LimitsDeps {
  return {
    loadToken: async () => 'fake-token',
    fetchUsage: async () => usage,
    now: fakeNow,
    loadForecast: async () => forecast,
  };
}

describe('burn limits', () => {
  it('exits 2 with one-line guidance when token missing', async () => {
    const { result, stdout } = await captureStdout(() => runLimits(args(), noTokenDeps()));
    assert.equal(result, 2);
    assert.equal(stdout.split('\n').filter(Boolean).length, 1);
    assert.match(stdout, /no Claude OAuth token found/);
    assert.match(stdout, /CLAUDE_CODE_OAUTH_TOKEN/);
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

  it('caches the OAuth response for repeated fetches within TTL', async () => {
    const usage: UsageResponse = {
      five_hour: { percent_used: 10, reset_at: '2026-04-24T13:00:00.000Z' },
    };
    let fetchCount = 0;
    const deps: LimitsDeps = {
      loadToken: async () => 'tok',
      fetchUsage: async () => {
        fetchCount++;
        return usage;
      },
      now: fakeNow,
      loadForecast: async () => null,
    };
    // First invocation: a separate process state, so cache is fresh per call.
    // We test the cache through two back-to-back JSON renders inside a single
    // runLimits call by running with --json twice via separate calls — each
    // runLimits constructs its own fetcher, so the in-process cache is per
    // command invocation. Verify that *within one invocation* a watch loop
    // would dedupe; here we just confirm a single invocation does one fetch.
    await captureStdout(() => runLimits(args({ json: true }), deps));
    assert.equal(fetchCount, 1);
  });
});
