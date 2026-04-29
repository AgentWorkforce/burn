import {
  buildArchive,
  getArchiveStatus,
  rebuildArchive,
  vacuumArchive,
  type ArchiveStatus,
  type BuildResult,
} from '@relayburn/ledger';

import { formatInt } from '../format.js';
import type { ParsedArgs } from '../args.js';
import { withProgress } from '../progress.js';

export async function runArchiveBuild(
  args: ParsedArgs,
  opts: { full?: boolean } = {},
): Promise<number> {
  const mode = opts.full ? 'rebuilding archive from ledger' : 'building archive tail';
  const result = await withProgress(mode, async (task) => {
    const r = opts.full ? await rebuildArchive() : await buildArchive();
    task.succeed(
      `${opts.full ? 'rebuilt' : 'built'} archive: ` +
        `${formatInt(r.turnsApplied)} turn${r.turnsApplied === 1 ? '' : 's'} applied`,
    );
    return r;
  });
  return printArchiveBuildResult(result, args, opts.full ? 'rebuild' : 'build');
}

export async function runArchiveStatus(args: ParsedArgs): Promise<number> {
  const status = await withProgress('checking archive status', async (task) => {
    const s = await getArchiveStatus();
    task.succeed('checked archive status');
    return s;
  });
  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify(status, null, 2) + '\n');
    return 0;
  }
  process.stdout.write(formatArchiveStatusLines(status).join('\n') + '\n');
  return 0;
}

export async function runArchiveVacuum(args: ParsedArgs): Promise<number> {
  const result = await withProgress('vacuuming archive', async (task) => {
    const r = await vacuumArchive();
    task.succeed('vacuumed archive');
    return r;
  });
  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify(result, null, 2) + '\n');
    return 0;
  }
  if (!result.existed) {
    process.stdout.write(
      `archive: no archive at ${result.archivePath} - run \`burn state rebuild archive\` first\n`,
    );
    return 0;
  }
  process.stdout.write(
    `archive: vacuumed ${formatBytes(result.beforeBytes)} -> ${formatBytes(result.afterBytes)}` +
      ` (reclaimed ${formatBytes(result.reclaimedBytes)})\n`,
  );
  return 0;
}

export function formatArchiveStatusLines(status: ArchiveStatus): string[] {
  const lines: string[] = [];
  lines.push(`archive: ${status.archivePath}`);
  if (!status.exists) {
    lines.push('  status: not built yet - run `burn state rebuild archive`');
    return lines;
  }
  lines.push(`  schema version: ${status.archiveVersion}`);
  lines.push(
    `  ledger cursor: ${formatInt(status.ledgerOffsetBytes)} / ${formatInt(status.ledgerSizeBytes)} bytes` +
      (status.upToDate ? ' (up to date)' : ' (tail pending)'),
  );
  if (status.lastBuiltAt) lines.push(`  last build: ${status.lastBuiltAt}`);
  if (status.lastRebuildAt) lines.push(`  last full rebuild: ${status.lastRebuildAt}`);
  lines.push('  rows:');
  lines.push(`    sessions:           ${formatInt(status.rowCounts.sessions)}`);
  lines.push(`    turns:              ${formatInt(status.rowCounts.turns)}`);
  lines.push(`    tool_calls:         ${formatInt(status.rowCounts.toolCalls)}`);
  lines.push(`    tool_result_events: ${formatInt(status.rowCounts.toolResultEvents)}`);
  lines.push(`    compactions:        ${formatInt(status.rowCounts.compactions)}`);
  return lines;
}

function printArchiveBuildResult(
  result: BuildResult,
  args: ParsedArgs,
  mode: 'build' | 'rebuild',
): number {
  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify(result, null, 2) + '\n');
    return 0;
  }
  const lines: string[] = [];
  if (mode === 'rebuild') {
    lines.push('rebuilt archive from ledger');
  } else {
    lines.push('archive build complete');
  }
  lines.push(
    `  ${formatInt(result.turnsApplied)} turn${result.turnsApplied === 1 ? '' : 's'} applied,` +
      ` ${formatInt(result.sessionsTouched)} session${result.sessionsTouched === 1 ? '' : 's'} touched,` +
      ` ${formatInt(result.stampsApplied)} stamp${result.stampsApplied === 1 ? '' : 's'},` +
      ` ${formatInt(result.compactionsApplied)} compaction${result.compactionsApplied === 1 ? '' : 's'},` +
      ` ${formatInt(result.toolResultEventsApplied)} tool-result event${result.toolResultEventsApplied === 1 ? '' : 's'}`,
  );
  lines.push(`  ${formatInt(result.scannedBytes)} bytes scanned from ledger tail`);
  process.stdout.write(lines.join('\n') + '\n');
  return 0;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
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
