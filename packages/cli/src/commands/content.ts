import { loadConfig, pruneContent, retentionMs } from '@relayburn/ledger';

import type { ParsedArgs } from '../args.js';

const CONTENT_HELP = `burn content — manage the content sidecar

Usage:
  burn content prune [--days <n>]

Examples:
  burn content prune
  burn content prune --days 30
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
  const result = await pruneContent({ olderThanMs: ms });
  process.stdout.write(
    `pruned ${result.filesDeleted} content file${result.filesDeleted === 1 ? '' : 's'} (${result.bytesFreed} bytes)\n`,
  );
  return 0;
}

function parseRetention(s: string): number | 'forever' | null {
  const trimmed = s.trim().toLowerCase();
  if (trimmed === 'forever') return 'forever';
  const n = Number(trimmed);
  if (!Number.isFinite(n)) return null;
  if (n < 0) return 'forever';
  return n;
}

export async function opportunisticPrune(): Promise<void> {
  try {
    const cfg = await loadConfig();
    if (cfg.content.store === 'off') return;
    const ms = retentionMs(cfg.content.retentionDays);
    if (ms === null) return;
    await pruneContent({ olderThanMs: ms });
  } catch (err) {
    // Best-effort — never fail a CLI operation because of prune, but surface
    // the reason on stderr so persistent failures are diagnosable.
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[burn] opportunistic content prune failed: ${msg}\n`);
  }
}
