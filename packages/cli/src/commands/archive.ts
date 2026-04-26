import {
  buildArchive,
  getArchiveStatus,
  rebuildArchive,
  type BuildResult,
} from '@relayburn/ledger';

import { formatInt } from '../format.js';
import type { ParsedArgs } from '../args.js';

const ARCHIVE_HELP = `burn archive — derived analytics archive (SQLite read model)

Usage:
  burn archive build       Apply any ledger tail not yet materialized.
  burn archive rebuild     Drop the archive and rebuild from the ledger.
  burn archive status      Print schema version, row counts, and sync state.
  burn archive --help      Show this help.

The archive is a disposable read model derived from \`ledger.jsonl\`. Deleting
\`archive.sqlite\` and running \`burn archive rebuild\` always reproduces the
same state — the ledger remains the canonical event log.

See issue #40 for the broader plan (rewiring read commands onto the archive,
content-sidecar bridging, full subagent / tool-result event tables).
`;

export async function runArchive(args: ParsedArgs): Promise<number> {
  if (args.flags['help'] === true) {
    process.stdout.write(ARCHIVE_HELP);
    return 0;
  }
  const sub = args.positional[0];
  switch (sub) {
    case undefined:
    case 'help':
      process.stdout.write(ARCHIVE_HELP);
      return 0;
    case 'build':
      return runBuild(args);
    case 'rebuild':
      return runRebuild(args);
    case 'status':
      return runStatus(args);
    default:
      process.stderr.write(`burn archive: unknown subcommand: ${sub}\n\n${ARCHIVE_HELP}`);
      return 1;
  }
}

async function runBuild(args: ParsedArgs): Promise<number> {
  const result = await buildArchive();
  return printBuildResult(result, args, 'build');
}

async function runRebuild(args: ParsedArgs): Promise<number> {
  const result = await rebuildArchive();
  return printBuildResult(result, args, 'rebuild');
}

function printBuildResult(
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

async function runStatus(args: ParsedArgs): Promise<number> {
  const status = await getArchiveStatus();
  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify(status, null, 2) + '\n');
    return 0;
  }
  const lines: string[] = [];
  lines.push(`archive: ${status.archivePath}`);
  if (!status.exists) {
    lines.push('  status: not built yet — run `burn archive build`');
    process.stdout.write(lines.join('\n') + '\n');
    return 0;
  }
  lines.push(`  schema version: ${status.archiveVersion}`);
  lines.push(
    `  ledger cursor: ${formatInt(status.ledgerOffsetBytes)} / ${formatInt(status.ledgerSizeBytes)} bytes` +
      (status.upToDate ? ' (up to date)' : ' (tail pending)'),
  );
  if (status.lastBuiltAt) lines.push(`  last build: ${status.lastBuiltAt}`);
  if (status.lastRebuildAt) lines.push(`  last rebuild: ${status.lastRebuildAt}`);
  lines.push('  rows:');
  lines.push(`    sessions:           ${formatInt(status.rowCounts.sessions)}`);
  lines.push(`    turns:              ${formatInt(status.rowCounts.turns)}`);
  lines.push(`    tool_calls:         ${formatInt(status.rowCounts.toolCalls)}`);
  lines.push(`    tool_result_events: ${formatInt(status.rowCounts.toolResultEvents)}`);
  lines.push(`    compactions:        ${formatInt(status.rowCounts.compactions)}`);
  process.stdout.write(lines.join('\n') + '\n');
  return 0;
}
