import { spawn } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';

import { queryAll, queryTurnsFromArchive } from '@relayburn/ledger';
import type { EnrichedTurn } from '@relayburn/ledger';

import type { ToolDefinition } from '../types.js';

const USAGE_ENDPOINT = 'https://api.anthropic.com/api/oauth/usage';
const ANTHROPIC_OAUTH_BETA = 'oauth-2025-04-20';
const SESSION_DURATION_MS = 5 * 60 * 60 * 1000;

interface UsageWindow {
  percent_used: number;
  reset_at: string;
}

interface UsageResponse {
  five_hour?: UsageWindow;
}

export type CurrentBlockAdvice = 'on-track' | 'at-risk' | 'over-budget' | 'unknown';

export interface CurrentBlockResult {
  percentUsed: number | null;
  burnRateTokensPerMin: number | null;
  projectedBlockTotal: number | null;
  minutesToReset: number | null;
  advice: CurrentBlockAdvice;
  note?: string;
}

export interface CurrentBlockDeps {
  loadOauthToken?: () => Promise<string | null>;
  fetchUsage?: (token: string) => Promise<UsageResponse>;
  queryTurns?: (windowStartMs: number) => Promise<EnrichedTurn[]>;
  now?: () => Date;
  /**
   * Called when the default archive-backed `queryTurns` falls through to the
   * ledger-walking `queryAll` because the archive open / query threw. Defaults
   * to no-op; the CLI server wires this to stderr so failures are visible.
   */
  onLog?: (msg: string) => void;
}

export function createCurrentBlockTool(deps: CurrentBlockDeps = {}): ToolDefinition {
  const loadToken = deps.loadOauthToken ?? loadOauthToken;
  const fetchUsage = deps.fetchUsage ?? fetchUsageFromApi;
  const log = deps.onLog ?? (() => {});
  const queryTurns =
    deps.queryTurns ??
    (async (start: number) => {
      const since = new Date(start).toISOString();
      try {
        return await queryTurnsFromArchive({ since, source: 'claude-code' });
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        log(`currentBlock: archive query failed, falling back to ledger walk: ${msg}`);
        return queryAll({ since, source: 'claude-code' });
      }
    });
  const now = deps.now ?? (() => new Date());

  return {
    name: 'burn__currentBlock',
    description:
      "Return the Claude 5-hour quota window's current percent-used and a " +
      'locally-forecast burn rate + projection. Combines the OAuth-reported ' +
      'window state with ledger-derived token totals so an agent can decide ' +
      'whether to downgrade models mid-session. Read-only.',
    inputSchema: {
      type: 'object',
      properties: {
        sessionId: {
          type: 'string',
          description: 'Accepted but not used — current-block is account-wide, not per-session.',
        },
      },
      required: [],
      additionalProperties: false,
    },
    handler: async () => {
      const nowDate = now();
      const nowMs = nowDate.getTime();

      const token = await loadToken();
      let usage: UsageResponse | null = null;
      let usageError: string | null = null;
      if (token) {
        try {
          usage = await fetchUsage(token);
        } catch (err) {
          usageError = err instanceof Error ? err.message : String(err);
        }
      } else {
        usageError = 'no Claude OAuth token found';
      }

      const windowStartMs = forecastWindowStartMs(usage?.five_hour, nowMs);
      const turns = await queryTurns(windowStartMs);
      const tokensSoFar = sumTokens(turns);
      const elapsedMs = Math.max(0, nowMs - windowStartMs);
      const remainingMs = Math.max(0, windowStartMs + SESSION_DURATION_MS - nowMs);

      const burnRate = elapsedMs > 0 ? tokensSoFar / (elapsedMs / 60_000) : null;
      const projectedBlockTotal =
        burnRate !== null ? Math.round(burnRate * (SESSION_DURATION_MS / 60_000)) : null;
      const minutesToReset = Math.round(remainingMs / 60_000);

      const pct = normalizePercent(usage?.five_hour?.percent_used);
      const projectedPct = projectPercentAtReset(pct, elapsedMs, remainingMs);
      const advice = deriveAdvice(pct, projectedPct);

      const result: CurrentBlockResult = {
        percentUsed: pct,
        burnRateTokensPerMin: burnRate !== null ? Math.round(burnRate) : null,
        projectedBlockTotal,
        minutesToReset,
        advice,
      };
      if (usageError) result.note = `oauth usage unavailable: ${usageError}`;
      return result;
    },
  };
}

function sumTokens(turns: EnrichedTurn[]): number {
  let total = 0;
  for (const t of turns) {
    const u = t.usage;
    total +=
      (u.input ?? 0) +
      (u.output ?? 0) +
      (u.reasoning ?? 0) +
      (u.cacheRead ?? 0) +
      (u.cacheCreate5m ?? 0) +
      (u.cacheCreate1h ?? 0);
  }
  return total;
}

function normalizePercent(raw: number | undefined): number | null {
  if (raw === undefined || !Number.isFinite(raw)) return null;
  // Anthropic's OAuth usage endpoint documents and returns the 0..100 scale
  // (e.g. percent_used: 34 means 34%). Pass it through unchanged. An earlier
  // version tried to auto-detect 0..1 vs 0..100 with a > 1.5 threshold, but
  // that misclassifies legitimately-low values like 1% as 0..1 scale and
  // inflates them 100x — turning "1% used" into "100% used → over-budget"
  // and causing false alarms early in a quota window (Devin review on #67).
  return raw;
}

function projectPercentAtReset(
  currentPercent: number | null,
  elapsedMs: number,
  remainingMs: number,
): number | null {
  if (currentPercent === null) return null;
  if (elapsedMs <= 0) return null;
  const totalMs = elapsedMs + remainingMs;
  if (totalMs <= 0) return null;
  const elapsedFraction = elapsedMs / totalMs;
  if (elapsedFraction <= 0) return null;
  return Math.min(100, currentPercent / elapsedFraction);
}

function deriveAdvice(
  current: number | null,
  projected: number | null,
): CurrentBlockAdvice {
  if (current === null && projected === null) return 'unknown';
  if (current !== null && current >= 100) return 'over-budget';
  if (projected !== null && projected >= 100) return 'over-budget';
  if (projected !== null && projected >= 80) return 'at-risk';
  if (current !== null && current >= 80) return 'at-risk';
  return 'on-track';
}

function forecastWindowStartMs(fiveHour: UsageWindow | undefined, nowMs: number): number {
  if (fiveHour) {
    const reset = Date.parse(fiveHour.reset_at);
    if (Number.isFinite(reset)) return reset - SESSION_DURATION_MS;
  }
  return nowMs - SESSION_DURATION_MS;
}

// --------------------------------------------------------------------------
// OAuth token loading. Copied verbatim from packages/cli/src/commands/limits.ts
// (extracting to a shared location would pull token I/O into @relayburn/ledger,
// which has no transport deps today — punting that refactor to a follow-up).
// --------------------------------------------------------------------------

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
    throw new Error(`usage endpoint ${res.status}`);
  }
  return (await res.json()) as UsageResponse;
}
