import { readdir } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';

import { loadConfig, pruneContent, retentionMs } from '@relayburn/ledger';

import { formatInt } from '../format.js';
import { walkJsonl, walkOpencodeSessions } from '../walk.js';
import type { ParsedArgs } from '../args.js';

const CONTENT_HELP = `burn content — manage the content sidecar

Usage:
  burn content prune [--days <n>] [--force]

Flags:
  --days <n>   override retention (number of days, or 'forever' to disable)
  --force      delete sidecars even when the source session file still exists.
               Default behavior keeps recoverable sidecars in place because
               'burn rebuild --content' can rederive them from the source.

Examples:
  burn content prune
  burn content prune --days 30
  burn content prune --force
`;

export async function runContent(args: ParsedArgs): Promise<number> {
  const sub = args.positional[0];
  if (!sub || sub === 'help' || sub === '--help' || sub === '-h') {
    process.stdout.write(CONTENT_HELP);
    return 0;
  }
  if (sub === 'prune') {
    return runContentPrune(args);
  }
  process.stderr.write(`unknown content subcommand: ${sub}\n\n${CONTENT_HELP}`);
  return 1;
}

async function runContentPrune(args: ParsedArgs): Promise<number> {
  const cfg = await loadConfig();
  let retention: number | 'forever';
  if (typeof args.flags['days'] === 'string') {
    const parsed = parseRetention(args.flags['days']);
    if (parsed === null) {
      process.stderr.write(
        `burn: invalid --days value: ${JSON.stringify(args.flags['days'])} (expected a number or "forever")\n\n${CONTENT_HELP}`,
      );
      return 2;
    }
    retention = parsed;
  } else {
    retention = cfg.content.retentionDays;
  }
  const ms = retentionMs(retention);
  if (ms === null) {
    process.stdout.write(`content retention=forever — nothing to prune\n`);
    return 0;
  }
  const force = args.flags['force'] === true || isForceEnv();
  const opts: Parameters<typeof pruneContent>[0] = { olderThanMs: ms };
  if (!force) {
    const sources = await loadSourceSessionIds();
    opts.isRecoverable = (sessionId) => sources.has(sessionId);
  }
  const result = await pruneContent(opts);
  process.stdout.write(
    `pruned ${formatInt(result.filesDeleted)} content file${result.filesDeleted === 1 ? '' : 's'} (${formatBytes(result.bytesFreed)})\n`,
  );
  if (!force && result.skippedRecoverable > 0) {
    process.stdout.write(
      `kept ${formatInt(result.skippedRecoverable)} recoverable sidecar${result.skippedRecoverable === 1 ? '' : 's'} whose source files still exist\n` +
        `  (use 'burn content prune --force' to delete them anyway)\n`,
    );
  }
  return 0;
}

function parseRetention(s: string): number | 'forever' | null {
  const trimmed = s.trim().toLowerCase();
  if (trimmed === '') return null;
  if (trimmed === 'forever') return 'forever';
  const n = Number(trimmed);
  if (!Number.isFinite(n)) return null;
  if (n < 0) return 'forever';
  return n;
}

function isForceEnv(): boolean {
  const raw = process.env['RELAYBURN_PRUNE_FORCE'];
  if (typeof raw !== 'string') return false;
  const v = raw.trim().toLowerCase();
  return v === '1' || v === 'true' || v === 'yes' || v === 'on';
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} bytes`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  const fixed = v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2);
  return `${fixed} ${units[i]}`;
}

export async function opportunisticPrune(): Promise<void> {
  try {
    const cfg = await loadConfig();
    if (cfg.content.store === 'off') return;
    const ms = retentionMs(cfg.content.retentionDays);
    if (ms === null) return;
    // Opportunistic prune always applies the recoverable-source check.
    // Reclaiming recoverable disk requires explicit `burn content prune --force`.
    // The exception is RELAYBURN_PRUNE_FORCE=1 for unattended automation that
    // genuinely wants the old behavior.
    const opts: Parameters<typeof pruneContent>[0] = { olderThanMs: ms };
    if (!isForceEnv()) {
      const sources = await loadSourceSessionIds();
      opts.isRecoverable = (sessionId) => sources.has(sessionId);
    }
    await pruneContent(opts);
  } catch (err) {
    // Best-effort — never fail a CLI operation because of prune, but surface
    // the reason on stderr so persistent failures are diagnosable.
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[burn] opportunistic content prune failed: ${msg}\n`);
  }
}

// --- source index ----------------------------------------------------------
//
// Walk the same source roots that `ingest.ts` uses and build an in-memory
// Set<sessionId>. Used to answer "is the upstream agent's session file still
// on disk?" — if yes, the sidecar is recoverable via `burn rebuild --content`
// and prune should skip it.
//
// Cost: one readdir pass per root; ~100ms even on large ledgers. Run
// synchronously at prune time; callers cache the result for the duration of
// a single prune call.

const CLAUDE_PROJECTS = path.join(homedir(), '.claude', 'projects');
const CODEX_SESSIONS = path.join(homedir(), '.codex', 'sessions');
const OPENCODE_STORAGE = path.join(homedir(), '.local', 'share', 'opencode', 'storage');
const OPENCODE_SESSION_ROOT = path.join(OPENCODE_STORAGE, 'session');

// Codex filenames are `rollout-<timestamp>-<uuid>.jsonl` where the trailing
// UUID is the session id used for the sidecar. We extract that suffix.
const CODEX_UUID_SUFFIX =
  /([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$/;

export async function loadSourceSessionIds(): Promise<Set<string>> {
  const out = new Set<string>();
  await Promise.all([
    collectClaudeSessionIds(out),
    collectCodexSessionIds(out),
    collectOpencodeSessionIds(out),
  ]);
  return out;
}

async function collectClaudeSessionIds(out: Set<string>): Promise<void> {
  let projects: string[];
  try {
    const entries = await readdir(CLAUDE_PROJECTS, { withFileTypes: true });
    projects = entries
      .filter((e) => e.isDirectory())
      .map((e) => path.join(CLAUDE_PROJECTS, e.name));
  } catch {
    return;
  }
  for (const dir of projects) {
    let entries;
    try {
      entries = await readdir(dir, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const e of entries) {
      if (!e.isFile()) continue;
      if (!e.name.endsWith('.jsonl')) continue;
      out.add(e.name.slice(0, -'.jsonl'.length));
    }
  }
}

async function collectCodexSessionIds(out: Set<string>): Promise<void> {
  let files: string[];
  try {
    files = await walkJsonl(CODEX_SESSIONS);
  } catch {
    return;
  }
  for (const file of files) {
    const base = path.basename(file, '.jsonl');
    const m = base.match(CODEX_UUID_SUFFIX);
    if (m) out.add(m[1]!);
  }
}

async function collectOpencodeSessionIds(out: Set<string>): Promise<void> {
  let files: string[];
  try {
    files = await walkOpencodeSessions(OPENCODE_SESSION_ROOT);
  } catch {
    return;
  }
  for (const file of files) {
    out.add(path.basename(file, '.json'));
  }
}
