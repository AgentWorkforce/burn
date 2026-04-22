import { homedir } from 'node:os';
import * as path from 'node:path';
import { readFile } from 'node:fs/promises';

import type { ParsedArgs } from '../args.js';

interface UsageWindow {
  percent_used: number;
  reset_at: string;
}

interface ClaudeUsageResponse {
  five_hour?: UsageWindow;
  seven_day?: UsageWindow;
  seven_day_opus?: UsageWindow;
  extra_usage?: UsageWindow;
}

const HELP = `burn limits — Claude quota window tracking

Usage:
  burn limits [--watch [5s]] [--json]

Options:
  --watch [interval]  Refresh loop (default: 5s)
  --json             Programmatic JSON output

Examples:
  burn limits
  burn limits --watch
  burn limits --watch 10s
  burn limits --json
`;

function formatDuration(ms: number): string {
  const totalSeconds = Math.floor(ms / 1000);
  const days = Math.floor(totalSeconds / 86400);
  const hours = Math.floor((totalSeconds % 86400) / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  
  if (days > 0) {
    return `${days}d ${hours}h`;
  }
  if (hours > 0) {
    return `${hours}h ${minutes}m`;
  }
  return `${minutes}m`;
}

function getResetTime(isoString: string): string {
  const reset = new Date(isoString);
  const now = new Date();
  const diff = reset.getTime() - now.getTime();
  
  if (diff <= 0) return 'now';
  return `in ${formatDuration(diff)}`;
}

async function getClaudeOAuthToken(): Promise<string | null> {
  const claudeStateDir = path.join(homedir(), '.claude');
  const stateFile = path.join(claudeStateDir, 'state.json');
  
  try {
    const content = await readFile(stateFile, 'utf-8');
    const state = JSON.parse(content);
    return state.oauth_token || state.token || null;
  } catch {
    return null;
  }
}

interface CachedUsage {
  data: ClaudeUsageResponse;
  fetchedAt: number;
}

let usageCache: CachedUsage | null = null;
const CACHE_TTL_MS = 60000; // 60 seconds cache

async function fetchClaudeUsage(token: string): Promise<ClaudeUsageResponse | null> {
  // Check cache first
  if (usageCache && Date.now() - usageCache.fetchedAt < CACHE_TTL_MS) {
    return usageCache.data;
  }

  try {
    const response = await fetch('https://api.anthropic.com/api/oauth/usage', {
      headers: {
        'Authorization': `Bearer ${token}`,
        'Accept': 'application/json',
      },
    });
    
    if (!response.ok) {
      if (response.status === 401) {
        process.stderr.write('Error: Invalid or expired OAuth token\n');
        return null;
      }
      process.stderr.write(`Error: API returned ${response.status}\n`);
      return null;
    }
    
    const data = await response.json() as unknown;
    if (typeof data !== 'object' || data === null) {
      return null;
    }
    
    const usage = data as ClaudeUsageResponse;
    // Update cache
    usageCache = { data: usage, fetchedAt: Date.now() };
    return usage;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`Error fetching usage: ${msg}\n`);
    return null;
  }
}

function formatUsageTable(usage: ClaudeUsageResponse): string {
  const lines: string[] = [];
  lines.push('Claude');
  
  if (usage.five_hour) {
    const w = usage.five_hour;
    lines.push(`  5-hour     ${w.percent_used.toFixed(0)}% used  resets ${getResetTime(w.reset_at)}`);
  }
  
  if (usage.seven_day) {
    const w = usage.seven_day;
    lines.push(`  7-day      ${w.percent_used.toFixed(0)}% used  resets ${getResetTime(w.reset_at)}`);
  }
  
  if (usage.seven_day_opus) {
    const w = usage.seven_day_opus;
    lines.push(`  7-day Opus ${w.percent_used.toFixed(0)}% used  resets ${getResetTime(w.reset_at)}`);
  }
  
  if (usage.extra_usage) {
    const w = usage.extra_usage;
    lines.push(`  Extra      ${w.percent_used.toFixed(0)}% used  resets ${getResetTime(w.reset_at)}`);
  }
  
  return lines.join('\n');
}

function formatUsageJson(usage: ClaudeUsageResponse): string {
  return JSON.stringify(usage, null, 2);
}

export async function runLimits(args: ParsedArgs): Promise<number> {
  if (args.flags['help']) {
    process.stdout.write(HELP);
    return 0;
  }
  
  const isJson = args.flags['json'] === true;
  const watchMode = args.flags['watch'] !== undefined;
  
  // Reject --watch --json combination as it breaks JSON parsing with ANSI codes
  if (watchMode && isJson) {
    process.stderr.write('Error: --watch and --json cannot be used together\n');
    process.stderr.write('Use --json for single-shot programmatic output\n');
    return 2;
  }
  
  const watchInterval = typeof args.flags['watch'] === 'string'
    ? parseInterval(args.flags['watch'])
    : 5000;
  
  const token = await getClaudeOAuthToken();
  if (!token) {
    process.stderr.write('Error: Could not find Claude OAuth token — run "claude auth login" or use Claude Code to refresh\n');
    return 2;
  }
  
  let hasError = false;
  
  const runOnce = async (): Promise<boolean> => {
    const usage = await fetchClaudeUsage(token);
    if (!usage) {
      hasError = true;
      return false;
    }
    
    if (isJson) {
      process.stdout.write(formatUsageJson(usage) + '\n');
    } else {
      process.stdout.write(formatUsageTable(usage) + '\n');
    }
    return true;
  };
  
  if (watchMode) {
    // Initial run
    const success = await runOnce();
    if (!success) {
      return hasError ? 2 : 0;
    }
    
    // Set up watch loop
    while (true) {
      await sleep(watchInterval);
      // Clear screen and move cursor to top (only in TTY mode, not JSON)
      process.stdout.write('\x1B[2J\x1B[0;0H');
      const success = await runOnce();
      if (!success) {
        return hasError ? 2 : 0;
      }
    }
  } else {
    const success = await runOnce();
    return hasError ? 2 : 0;
  }
}

function parseInterval(s: string): number {
  const match = /^(\d+)([smh])?$/.exec(s);
  if (!match) return 5000;
  
  const value = parseInt(match[1]!, 10);
  const unit = match[2] || 's';
  
  switch (unit) {
    case 's': return value * 1000;
    case 'm': return value * 60 * 1000;
    case 'h': return value * 60 * 60 * 1000;
    default: return 5000;
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms));
}
