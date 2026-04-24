import { spawn } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';

import { loadPlans, queryAll } from '@relayburn/ledger';
import type { Plan } from '@relayburn/ledger';

import type { ParsedArgs } from '../args.js';
import { formatUsd } from '../format.js';
import { ingestAll } from '../ingest.js';
import { statusForPlans, type PlanStatus } from './plans.js';

const USAGE_ENDPOINT = 'https://api.anthropic.com/api/oauth/usage';
const ANTHROPIC_OAUTH_BETA = 'oauth-2025-04-20';
const CACHE_TTL_MS = 30_000;
const DEFAULT_WATCH_INTERVAL_S = 5;
const SESSION_DURATION_MS = 5 * 60 * 60 * 1000;

interface UsageWindow {
  percent_used: number;
  reset_at: string;
}

export interface UsageResponse {
  five_hour?: UsageWindow;
  seven_day?: UsageWindow;
  seven_day_opus?: UsageWindow;
  extra_usage?: UsageWindow;
}

export interface ForecastInput {
  // tokens consumed since the start of the 5-hour window
  tokensSoFar: number;
  // ms elapsed since the window start
  elapsedMs: number;
  // ms remaining until window end (reset)
  remainingMs: number;
}

export interface LimitsDeps {
  loadToken?: () => Promise<string | null>;
  fetchUsage?: (token: string) => Promise<UsageResponse>;
  now?: () => Date;
  loadForecast?: (windowStartMs: number, nowMs: number) => Promise<ForecastInput | null>;
  loadPlanStatuses?: () => Promise<PlanStatus[]>;
}

export async function runLimits(args: ParsedArgs, deps: LimitsDeps = {}): Promise<number> {
  const watchFlag = args.flags['watch'];
  const json = args.flags['json'] === true;
  const noApi = args.flags['no-api'] === true;
  const noForecast = args.flags['no-forecast'] === true;

  const loadToken = deps.loadToken ?? loadOauthToken;
  const fetchUsage = deps.fetchUsage ?? fetchUsageFromApi;
  const now = deps.now ?? (() => new Date());
  const loadForecast = deps.loadForecast ?? loadForecastFromLedger;
  const loadPlanStatuses = deps.loadPlanStatuses ?? defaultLoadPlanStatuses;

  // Resolve the OAuth token once per invocation, not per render. The macOS
  // Keychain path spawns `security`, which is expensive enough that calling
  // it on every --watch tick (every 5s by default) would dominate the loop.
  let token: string | null = null;
  if (!noApi) {
    token = await loadToken();
    if (!token) {
      process.stderr.write(
        'burn limits: no Claude OAuth token found. Run `claude /login` to authenticate, ' +
          'or set CLAUDE_CODE_OAUTH_TOKEN.\n',
      );
      return 2;
    }
  }

  const fetchOnce = makeCachingFetcher(fetchUsage, CACHE_TTL_MS, now);

  const renderOnce = async (): Promise<{ exitCode: number; output: string }> => {
    let usage: UsageResponse | null = null;
    let usageError: string | null = null;
    if (token !== null) {
      try {
        usage = await fetchOnce(token);
      } catch (err) {
        usageError = err instanceof Error ? err.message : String(err);
      }
    }

    const nowDate = now();
    let forecast: { window: ForecastWindow; data: ForecastInput } | null = null;
    if (!noForecast) {
      const windowStartMs = forecastWindowStartMs(usage?.five_hour, nowDate);
      const data = await loadForecast(windowStartMs, nowDate.getTime());
      if (data) {
        forecast = { window: { startMs: windowStartMs }, data };
      }
    }

    const projectedPercent = forecast && usage?.five_hour
      ? projectFromOauth(usage.five_hour.percent_used, forecast.data)
      : null;

    const planStatuses = await loadPlanStatuses();

    if (json) {
      return {
        exitCode: usageError ? 1 : 0,
        output: JSON.stringify(
          {
            fetchedAt: nowDate.toISOString(),
            usage,
            usageError,
            forecast: forecast
              ? {
                  windowStart: new Date(forecast.window.startMs).toISOString(),
                  tokensSoFar: forecast.data.tokensSoFar,
                  elapsedMs: forecast.data.elapsedMs,
                  remainingMs: forecast.data.remainingMs,
                  burnRateTokensPerMinute: burnRatePerMinute(forecast.data),
                  projectedPercentAtReset: projectedPercent,
                }
              : null,
            plans: planStatuses.map((s) => ({
              id: s.usage.plan.id,
              provider: s.usage.plan.provider,
              name: s.usage.plan.name,
              budgetUsd: s.usage.plan.budgetUsd,
              resetDay: s.usage.plan.resetDay,
              spentUsd: s.usage.spentUsd,
              daysElapsed: s.usage.daysElapsed,
              daysInCycle: s.usage.daysInCycle,
              projectedEndOfCycleUsd: s.usage.projectedEndOfCycleUsd,
              overBudget: s.usage.overBudget,
              runwayDays: s.usage.runwayDays,
              resetAt: s.usage.resetAt,
              limitedData: s.usage.limitedData,
            })),
          },
          null,
          2,
        ) + '\n',
      };
    }

    return {
      exitCode: usageError ? 1 : 0,
      output: renderTty({ usage, usageError, forecast, projectedPercent, planStatuses, now: nowDate }),
    };
  };

  if (watchFlag === undefined) {
    const { exitCode, output } = await renderOnce();
    process.stdout.write(output);
    return exitCode;
  }

  const intervalMs = parseWatchInterval(watchFlag);
  if (intervalMs === null) {
    process.stderr.write(
      `burn limits: invalid --watch value: ${JSON.stringify(watchFlag)} (expected seconds, e.g. 5 or 5s)\n`,
    );
    return 2;
  }
  return runWatch(renderOnce, intervalMs);
}

interface ForecastWindow {
  startMs: number;
}

function renderTty(opts: {
  usage: UsageResponse | null;
  usageError: string | null;
  forecast: { window: ForecastWindow; data: ForecastInput } | null;
  projectedPercent: number | null;
  planStatuses: PlanStatus[];
  now: Date;
}): string {
  const { usage, usageError, forecast, projectedPercent, planStatuses, now } = opts;
  const lines: string[] = [];
  lines.push('Claude');

  if (usageError) {
    lines.push(`  (api error: ${usageError})`);
  } else if (usage) {
    const rows: { label: string; window: UsageWindow | undefined }[] = [
      { label: '5-hour', window: usage.five_hour },
      { label: '7-day', window: usage.seven_day },
      { label: '7-day Opus', window: usage.seven_day_opus },
      { label: 'extra', window: usage.extra_usage },
    ];
    const visible = rows.filter((r) => r.window !== undefined) as {
      label: string;
      window: UsageWindow;
    }[];
    if (visible.length === 0) {
      lines.push('  (no quota windows reported)');
    } else {
      const labelWidth = Math.max(...visible.map((r) => r.label.length));
      const pctWidth = Math.max(
        ...visible.map((r) => formatPercent(r.window.percent_used).length),
      );
      for (const r of visible) {
        const label = r.label.padEnd(labelWidth);
        const pct = formatPercent(r.window.percent_used).padStart(pctWidth);
        const reset = formatResetIn(r.window.reset_at, now);
        lines.push(`  ${label}  ${pct} used  resets in ${reset}`);
      }
    }
  }

  if (forecast) {
    const rate = burnRatePerMinute(forecast.data);
    lines.push('');
    lines.push('Forecast (5-hour window, local ledger):');
    if (rate === null) {
      lines.push('  insufficient data');
    } else {
      const parts = [`burn rate ${formatTokensPerMinute(rate)}`];
      if (projectedPercent !== null) {
        // projectedPercent is already on the 0..100 scale (projectFromOauth
        // normalizes + caps); skip formatPercent's auto-detect heuristic so
        // small values like 1.01 don't get re-multiplied to 101.
        parts.push(`projected ${projectedPercent.toFixed(0)}% at reset`);
      }
      lines.push(`  ${parts.join(', ')}`);
    }
  }

  for (const status of planStatuses) {
    lines.push('');
    lines.push(`Monthly plan (${status.usage.plan.name}):`);
    const u = status.usage;
    const spentPct = u.plan.budgetUsd > 0 ? (u.spentUsd / u.plan.budgetUsd) * 100 : 0;
    lines.push(
      `  Spent:     ${formatUsd(u.spentUsd)} / ${formatUsd(u.plan.budgetUsd)}   (${spentPct.toFixed(0)}%)`,
    );
    lines.push(`  Elapsed:   ${u.daysElapsed} / ${u.daysInCycle} days`);
    const projected = formatUsd(u.projectedEndOfCycleUsd);
    const overOrUnder = u.overBudget
      ? `${formatUsd(u.projectedEndOfCycleUsd - u.plan.budgetUsd)} over`
      : `${(((u.plan.budgetUsd - u.projectedEndOfCycleUsd) / u.plan.budgetUsd) * 100).toFixed(0)}% under`;
    const limited = u.limitedData ? '  (limited data)' : '';
    lines.push(`  Projected: ${projected} end-of-cycle (${overOrUnder})${limited}`);
    if (u.runwayDays !== null) {
      lines.push(`  Runway:    ${u.runwayDays} more day${u.runwayDays === 1 ? '' : 's'} at current rate`);
    }
  }

  return lines.join('\n') + '\n';
}

function formatPercent(fraction: number): string {
  // Endpoint returns 0..100 (per issue example "34"); guard either shape by
  // assuming >1.5 means already in percent units.
  const pct = fraction > 1.5 ? fraction : fraction * 100;
  return `${pct.toFixed(0)}%`;
}

function formatResetIn(resetAt: string, now: Date): string {
  const t = Date.parse(resetAt);
  if (!Number.isFinite(t)) return 'unknown';
  const ms = t - now.getTime();
  if (ms <= 0) return 'now';
  return formatDuration(ms);
}

function formatDuration(ms: number): string {
  const totalMin = Math.round(ms / 60_000);
  const days = Math.floor(totalMin / (60 * 24));
  const hours = Math.floor((totalMin % (60 * 24)) / 60);
  const minutes = totalMin % 60;
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

function formatTokensPerMinute(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k tok/min`;
  return `${n.toFixed(0)} tok/min`;
}

function burnRatePerMinute(f: ForecastInput): number | null {
  if (f.elapsedMs <= 0) return null;
  return f.tokensSoFar / (f.elapsedMs / 60_000);
}

// Linear extrapolation from the OAuth-reported current % to the end of the
// 5-hour window. We can't translate local tokens → % without knowing the
// per-plan cap, so we lean on OAuth's authoritative %-used and assume the
// next remainder of the window keeps the same average rate as the elapsed
// portion. Capped at 100.
function projectFromOauth(currentPercent: number, f: ForecastInput): number | null {
  if (f.elapsedMs <= 0) return null;
  const totalMs = f.elapsedMs + f.remainingMs;
  if (totalMs <= 0) return null;
  const elapsedFraction = f.elapsedMs / totalMs;
  if (elapsedFraction <= 0) return null;
  const pct = currentPercent > 1.5 ? currentPercent : currentPercent * 100;
  return Math.min(100, pct / elapsedFraction);
}

function forecastWindowStartMs(fiveHour: UsageWindow | undefined, now: Date): number {
  if (fiveHour) {
    const reset = Date.parse(fiveHour.reset_at);
    if (Number.isFinite(reset)) return reset - SESSION_DURATION_MS;
  }
  return now.getTime() - SESSION_DURATION_MS;
}

export function makeCachingFetcher(
  fetchUsage: (token: string) => Promise<UsageResponse>,
  ttlMs: number,
  now: () => Date,
): (token: string) => Promise<UsageResponse> {
  let cache: { token: string; at: number; value: UsageResponse } | null = null;
  return async (token: string) => {
    const t = now().getTime();
    if (cache && cache.token === token && t - cache.at < ttlMs) return cache.value;
    const value = await fetchUsage(token);
    cache = { token, at: t, value };
    return value;
  };
}

function parseWatchInterval(flag: string | true): number | null {
  if (flag === true) return DEFAULT_WATCH_INTERVAL_S * 1000;
  const m = /^(\d+)(s)?$/.exec(flag.trim());
  if (!m) return null;
  const n = parseInt(m[1]!, 10);
  if (n <= 0) return null;
  return n * 1000;
}

async function runWatch(
  renderOnce: () => Promise<{ exitCode: number; output: string }>,
  intervalMs: number,
): Promise<number> {
  let stopped = false;
  const stop = () => {
    stopped = true;
  };
  process.on('SIGINT', stop);
  process.on('SIGTERM', stop);

  const isTty = process.stdout.isTTY === true;
  let lastExit = 0;
  while (!stopped) {
    const { exitCode, output } = await renderOnce();
    lastExit = exitCode;
    if (isTty) {
      process.stdout.write('\x1b[H\x1b[2J' + output);
    } else {
      process.stdout.write(output);
    }
    if (stopped) break;
    await sleep(intervalMs, () => stopped);
  }
  process.off('SIGINT', stop);
  process.off('SIGTERM', stop);
  return lastExit;
}

function sleep(ms: number, isCancelled: () => boolean): Promise<void> {
  return new Promise((resolve) => {
    const start = Date.now();
    const tick = () => {
      if (isCancelled() || Date.now() - start >= ms) return resolve();
      setTimeout(tick, Math.min(100, ms));
    };
    tick();
  });
}

export async function loadOauthToken(): Promise<string | null> {
  const env = process.env['CLAUDE_CODE_OAUTH_TOKEN'];
  if (env && env.length > 0) return env;

  const fromFile = await readCredentialsFile();
  if (fromFile) return fromFile;

  if (process.platform === 'darwin') {
    const fromKeychain = await readMacOsKeychain();
    if (fromKeychain) return fromKeychain;
  }
  return null;
}

async function readCredentialsFile(): Promise<string | null> {
  const candidates = [
    path.join(homedir(), '.claude', '.credentials.json'),
    path.join(homedir(), '.claude', 'credentials.json'),
  ];
  for (const p of candidates) {
    try {
      const raw = await readFile(p, 'utf8');
      const parsed = JSON.parse(raw) as unknown;
      const token = extractTokenFromCredentials(parsed);
      if (token) return token;
    } catch {
      // fall through to next candidate
    }
  }
  return null;
}

function extractTokenFromCredentials(parsed: unknown): string | null {
  if (!parsed || typeof parsed !== 'object') return null;
  const obj = parsed as Record<string, unknown>;
  const oauth = obj['claudeAiOauth'];
  if (oauth && typeof oauth === 'object') {
    const access = (oauth as Record<string, unknown>)['accessToken'];
    if (typeof access === 'string' && access.length > 0) return access;
  }
  const direct = obj['accessToken'];
  if (typeof direct === 'string' && direct.length > 0) return direct;
  return null;
}

function readMacOsKeychain(): Promise<string | null> {
  return new Promise((resolve) => {
    const child = spawn(
      'security',
      ['find-generic-password', '-s', 'Claude Code-credentials', '-w'],
      { stdio: ['ignore', 'pipe', 'ignore'] },
    );
    let out = '';
    child.stdout.on('data', (chunk: Buffer) => {
      out += chunk.toString('utf8');
    });
    child.on('error', () => resolve(null));
    child.on('exit', (code) => {
      if (code !== 0) return resolve(null);
      const trimmed = out.trim();
      if (!trimmed) return resolve(null);
      try {
        const parsed = JSON.parse(trimmed) as unknown;
        const fromJson = extractTokenFromCredentials(parsed);
        if (fromJson) return resolve(fromJson);
      } catch {
        // keychain entry is the bare token, not JSON
      }
      resolve(trimmed);
    });
  });
}

async function fetchUsageFromApi(token: string): Promise<UsageResponse> {
  const res = await fetch(USAGE_ENDPOINT, {
    headers: {
      Authorization: `Bearer ${token}`,
      'anthropic-beta': ANTHROPIC_OAUTH_BETA,
      Accept: 'application/json',
    },
  });
  if (!res.ok) {
    throw new Error(`usage endpoint ${res.status}: ${await safeBody(res)}`);
  }
  return (await res.json()) as UsageResponse;
}

async function safeBody(res: Response): Promise<string> {
  try {
    const t = await res.text();
    return t.slice(0, 200);
  } catch {
    return '';
  }
}

async function defaultLoadPlanStatuses(): Promise<PlanStatus[]> {
  const plans: Plan[] = await loadPlans();
  if (plans.length === 0) return [];
  return statusForPlans(plans);
}

async function loadForecastFromLedger(
  windowStartMs: number,
  nowMs: number,
): Promise<ForecastInput | null> {
  // Match the convention used by every other read-only command (summary,
  // by-tool, diagnose, …): sweep new session logs into the ledger before
  // querying, so the forecast reflects what just happened in the active
  // claude session rather than whatever the last burn invocation captured.
  // ingestAll is incremental via cursors, so the watch loop calling this
  // every ~5s stays cheap on a steady-state ledger.
  await ingestAll();
  const since = new Date(windowStartMs).toISOString();
  const turns = await queryAll({ since, source: 'claude-code' });
  if (turns.length === 0) return null;
  let tokens = 0;
  for (const t of turns) {
    const u = t.usage;
    tokens +=
      (u.input ?? 0) +
      (u.output ?? 0) +
      (u.reasoning ?? 0) +
      (u.cacheRead ?? 0) +
      (u.cacheCreate5m ?? 0) +
      (u.cacheCreate1h ?? 0);
  }
  const elapsedMs = Math.max(0, nowMs - windowStartMs);
  const remainingMs = Math.max(0, windowStartMs + SESSION_DURATION_MS - nowMs);
  return { tokensSoFar: tokens, elapsedMs, remainingMs };
}
