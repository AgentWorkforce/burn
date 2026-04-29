import { createReadStream } from 'node:fs';
import { readdir, readFile, stat } from 'node:fs/promises';
import * as path from 'node:path';
import { createInterface } from 'node:readline';

import {
  contentDir,
  getArchiveStatus,
  isTurnLine,
  isUserTurnLine,
  isValidSessionId,
  ledgerContentIndexPath,
  ledgerIndexPath,
  ledgerPath,
  rebuildIndex,
  reclassifyLedger,
} from '@relayburn/ledger';

import { reingestMissingContent } from '../ingest.js';
import { formatInt } from '../format.js';
import type { ParsedArgs } from '../args.js';
import { formatArchiveStatusLines, runArchiveBuild, runArchiveVacuum } from './archive.js';

const REBUILD_HELP = `burn rebuild - rebuild derived ledger artifacts

Usage:
  burn rebuild index
  burn rebuild classify [--force]
  burn rebuild content
  burn rebuild archive [--full|--vacuum] [--json]
  burn rebuild archive vacuum [--json]
  burn rebuild all [--force]
  burn rebuild status [--json]

Targets:
  index     rebuild the sidecar id and content-fingerprint indexes
  classify  re-run the activity classifier on ledger turns
  content   re-parse source session files to populate missing content
  archive   apply the ledger tail to archive.sqlite; --full rebuilds from zero;
            --vacuum reclaims unused archive pages
  all       run content, index, classify, then archive
  status    print derived artifact status for index, content, classifier, archive

`;

interface IndexFileStatus {
  path: string;
  exists: boolean;
  bytes: number;
  entries: number;
}

interface RebuildStatus {
  index: {
    ids: IndexFileStatus;
    content: IndexFileStatus;
  };
  content: {
    path: string;
    exists: boolean;
    files: number;
    sessions: number;
    bytes: number;
    userTurns: number;
  };
  classifier: {
    turns: number;
    classified: number;
    missing: number;
  };
  archive: Awaited<ReturnType<typeof getArchiveStatus>>;
}

interface ContentSidecarStatus {
  path: string;
  exists: boolean;
  files: number;
  sessions: number;
  bytes: number;
}

interface LedgerDerivedCounts {
  turns: number;
  classified: number;
  userTurns: number;
}

export async function runRebuild(args: ParsedArgs): Promise<number> {
  if (args.flags['help'] === true) {
    process.stdout.write(REBUILD_HELP);
    return 0;
  }

  const target = args.positional[0];
  switch (target) {
    case undefined:
    case 'help':
      process.stdout.write(REBUILD_HELP);
      return 0;
    case 'index':
      return runIndex();
    case 'classify':
      return runClassify(args);
    case 'content':
      return runContent();
    case 'archive':
      return runArchiveTarget(args);
    case 'all':
      return runAll(args);
    case 'status':
      return runStatus(args);
    default:
      process.stderr.write(`burn rebuild: unknown target: ${target}\n\n${REBUILD_HELP}`);
      return 1;
  }
}

async function runArchiveTarget(args: ParsedArgs): Promise<number> {
  const action = args.positional[1];
  const vacuum = args.flags['vacuum'] === true || action === 'vacuum';
  if (action !== undefined && action !== 'build' && action !== 'vacuum') {
    process.stderr.write(`burn rebuild archive: unknown action: ${action}\n\n${REBUILD_HELP}`);
    return 1;
  }
  if (vacuum && args.flags['full'] === true) {
    process.stderr.write(`burn rebuild archive: --full and --vacuum cannot be combined\n`);
    return 1;
  }
  if (vacuum) return runArchiveVacuum(args);
  return runArchiveBuild(args, { full: args.flags['full'] === true });
}

async function runAll(args: ParsedArgs): Promise<number> {
  const lines: string[] = [];
  await rebuildContent(lines);
  await rebuildIndexTarget(lines);
  await rebuildClassify(lines, args.flags['force'] === true);
  const flags = { ...args.flags };
  delete flags['json'];
  const archiveArgs = { ...args, flags };
  const result = await captureStdout(() => runArchiveBuild(archiveArgs, { full: false }));
  lines.push(result.stdout.trimEnd());
  process.stdout.write(lines.filter(Boolean).join('\n') + '\n');
  return result.code;
}

async function runIndex(): Promise<number> {
  const lines: string[] = [];
  await rebuildIndexTarget(lines);
  process.stdout.write(lines.join('\n') + '\n');
  return 0;
}

async function runClassify(args: ParsedArgs): Promise<number> {
  const lines: string[] = [];
  await rebuildClassify(lines, args.flags['force'] === true);
  process.stdout.write(lines.join('\n') + '\n');
  return 0;
}

async function runContent(): Promise<number> {
  const lines: string[] = [];
  await rebuildContent(lines);
  process.stdout.write(lines.join('\n') + '\n');
  return 0;
}

async function rebuildClassify(lines: string[], force: boolean): Promise<void> {
  const report = await reclassifyLedger({ force });
  const unchanged = report.processed - report.changed;
  lines.push(
    `reclassified ${formatInt(report.processed)} of ${formatInt(report.scanned)} turns` +
      ` (${formatInt(report.skipped)} skipped, already classified)`,
  );
  lines.push(
    `  ${formatInt(report.changed)} ended up with a different activity label,` +
      ` ${formatInt(unchanged)} unchanged`,
  );
  if (report.changed > 0) {
    const changes = Object.entries(report.changedByCategory).sort((a, b) => b[1] - a[1]);
    for (const [cat, n] of changes) {
      lines.push(`    -> ${cat}: ${formatInt(n)}`);
    }
  }
}

async function rebuildContent(lines: string[]): Promise<void> {
  const r = await reingestMissingContent();
  lines.push(
    `reingested derived content for ${formatInt(r.reingestedSessions)} sessions` +
      ` (${formatInt(r.scannedFiles)} files scanned,` +
      ` ${formatInt(r.skippedExisting)} already complete,` +
      ` ${formatInt(r.appendedContent)} records appended,` +
      ` ${formatInt(r.appendedUserTurns)} user turns appended,` +
      ` ${formatInt(r.failed)} failed)`,
  );
}

async function rebuildIndexTarget(lines: string[]): Promise<void> {
  const { ids, content } = await rebuildIndex();
  lines.push(
    `rebuilt ledger index: ${formatInt(ids)} id hashes, ${formatInt(content)} content fingerprints`,
  );
}

async function runStatus(args: ParsedArgs): Promise<number> {
  const status = await collectRebuildStatus();
  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify(status, null, 2) + '\n');
    return 0;
  }
  process.stdout.write(formatRebuildStatusLines(status).join('\n') + '\n');
  return 0;
}

async function collectRebuildStatus(): Promise<RebuildStatus> {
  const [ids, content, contentSidecar, ledgerCounts, archive] = await Promise.all([
    indexFileStatus(ledgerIndexPath()),
    indexFileStatus(ledgerContentIndexPath()),
    contentSidecarStatus(),
    ledgerDerivedCounts(),
    getArchiveStatus(),
  ]);
  return {
    index: { ids, content },
    content: { ...contentSidecar, userTurns: ledgerCounts.userTurns },
    classifier: {
      turns: ledgerCounts.turns,
      classified: ledgerCounts.classified,
      missing: ledgerCounts.turns - ledgerCounts.classified,
    },
    archive,
  };
}

async function indexFileStatus(filePath: string): Promise<IndexFileStatus> {
  try {
    const [raw, st] = await Promise.all([readFile(filePath, 'utf8'), stat(filePath)]);
    return {
      path: filePath,
      exists: st.isFile(),
      bytes: st.isFile() ? st.size : 0,
      entries: st.isFile() ? countNonEmptyLines(raw) : 0,
    };
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
    return { path: filePath, exists: false, bytes: 0, entries: 0 };
  }
}

async function contentSidecarStatus(): Promise<ContentSidecarStatus> {
  const dir = contentDir();
  let entries: string[];
  try {
    entries = await readdir(dir);
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
    return {
      path: dir,
      exists: false,
      files: 0,
      sessions: 0,
      bytes: 0,
    };
  }

  let files = 0;
  let sessions = 0;
  let bytes = 0;
  for (const name of entries) {
    if (!name.endsWith('.jsonl')) continue;
    const sessionId = name.slice(0, -'.jsonl'.length);
    if (!isValidSessionId(sessionId)) continue;
    const full = path.join(dir, name);
    try {
      const st = await stat(full);
      if (!st.isFile()) continue;
      files++;
      bytes += st.size;
      if (st.size > 0) sessions++;
    } catch {
      // Raced with prune; ignore the vanished file.
    }
  }
  return {
    path: dir,
    exists: true,
    files,
    sessions,
    bytes,
  };
}

async function ledgerDerivedCounts(): Promise<LedgerDerivedCounts> {
  const filePath = ledgerPath();
  const exists = await stat(filePath)
    .then((s) => s.isFile())
    .catch(() => false);
  if (!exists) return { turns: 0, classified: 0, userTurns: 0 };

  const counts: LedgerDerivedCounts = { turns: 0, classified: 0, userTurns: 0 };
  const rl = createInterface({
    input: createReadStream(filePath, { encoding: 'utf8' }),
    crlfDelay: Infinity,
  });
  try {
    for await (const line of rl) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      let parsed: unknown;
      try {
        parsed = JSON.parse(trimmed);
      } catch {
        continue;
      }
      if (isTurnLine(parsed)) {
        counts.turns++;
        if (typeof parsed.record.activity === 'string') counts.classified++;
      } else if (isUserTurnLine(parsed)) {
        counts.userTurns++;
      }
    }
  } finally {
    rl.close();
  }
  return counts;
}

function formatRebuildStatusLines(status: RebuildStatus): string[] {
  const lines: string[] = [];
  lines.push('derived state:');
  lines.push('index:');
  lines.push(`  id index: ${formatIndexFileStatus(status.index.ids, 'hashes')}`);
  lines.push(`  content index: ${formatIndexFileStatus(status.index.content, 'fingerprints')}`);
  lines.push('content:');
  if (!status.content.exists) {
    lines.push(`  status: not built yet at ${status.content.path}`);
  } else {
    lines.push(`  path: ${status.content.path}`);
  }
  lines.push(
    `  sidecars: ${formatInt(status.content.files)} files,` +
      ` ${formatInt(status.content.sessions)} non-empty sessions,` +
      ` ${formatInt(status.content.bytes)} bytes`,
  );
  lines.push(`  user turns: ${formatInt(status.content.userTurns)} ledger rows`);
  lines.push('classifier:');
  lines.push(
    `  turns: ${formatInt(status.classifier.classified)} classified /` +
      ` ${formatInt(status.classifier.turns)} total` +
      (status.classifier.missing > 0
        ? ` (${formatInt(status.classifier.missing)} missing)`
        : ' (complete)'),
  );
  lines.push(...formatArchiveStatusLines(status.archive));
  return lines;
}

function formatIndexFileStatus(status: IndexFileStatus, noun: string): string {
  if (!status.exists) return `missing at ${status.path}`;
  return `${formatInt(status.entries)} ${noun}, ${formatInt(status.bytes)} bytes at ${status.path}`;
}

function countNonEmptyLines(raw: string): number {
  let count = 0;
  for (const line of raw.split('\n')) {
    if (line.trim().length > 0) count++;
  }
  return count;
}

async function captureStdout(fn: () => Promise<number>): Promise<{ code: number; stdout: string }> {
  const origStdout = process.stdout.write.bind(process.stdout);
  let stdout = '';
  process.stdout.write = ((chunk: string | Uint8Array): boolean => {
    stdout += typeof chunk === 'string' ? chunk : chunk.toString();
    return true;
  }) as typeof process.stdout.write;
  try {
    const code = await fn();
    return { code, stdout };
  } finally {
    process.stdout.write = origStdout;
  }
}
