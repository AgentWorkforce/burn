import { createReadStream } from 'node:fs';
import { readdir, readFile, rm, stat, unlink } from 'node:fs/promises';
import { homedir } from 'node:os';
import * as path from 'node:path';
import { createInterface } from 'node:readline';

import { formatInt } from '../format.js';
import type { ParsedArgs } from '../args.js';

type LedgerModule = typeof import('@relayburn/ledger');
type ArchiveStatus = Awaited<ReturnType<LedgerModule['getArchiveStatus']>>;
type FormatArchiveStatusLines = (status: ArchiveStatus) => string[];

const STATE_HELP = `burn state - inspect and maintain derived ledger state

Usage:
  burn state [status] [--json]
  burn state rebuild <target>
  burn state prune [--days <n>] [--force]
  burn state reset [--force] [--reingest] [--json]

Subcommands:
  status    print derived artifact status for index, content, classifier, archive
  rebuild   rebuild index, classify, content, archive, or all derived artifacts
  prune     prune expired content sidecars
  reset     wipe all derived state and (optionally) re-ingest from source logs

Run 'burn state rebuild help' for rebuild targets.
`;

const REBUILD_HELP = `burn state rebuild - rebuild derived ledger artifacts

Usage:
  burn state rebuild index
  burn state rebuild classify [--force]
  burn state rebuild content
  burn state rebuild archive [--full|--vacuum] [--json]
  burn state rebuild archive vacuum [--json]
  burn state rebuild all [--force]

Targets:
  index     rebuild the sidecar id and content-fingerprint indexes
  classify  re-run the activity classifier on ledger turns
  content   re-parse source session files to populate missing content
  archive   apply the ledger tail to archive.sqlite; --full rebuilds from zero;
            --vacuum reclaims unused archive pages
  all       run content, index, classify, then archive
`;

const RESET_HELP = `burn state reset - wipe all derived ledger state

Usage:
  burn state reset [--force] [--reingest] [--json]

Flags:
  --force      actually delete. Without this flag, reset is a dry-run that
               prints the paths and sizes that would be removed.
  --reingest   after a successful --force wipe, re-parse all source harness
               logs from offset 0 by calling 'burn ingest'. No-op without
               --force.
  --json       emit a JSON summary instead of human-readable output.

Deletes (under \$RELAYBURN_HOME or ~/.relayburn):
  ledger.jsonl, ledger.idx, ledger.content.idx, cursors.json, hwm.json,
  archive.sqlite (+ -wal/-shm sidecars), and the content/ directory.

Preserves:
  config.json, plans.json, models.dev.json, *.lock files. Source harness
  logs at ~/.claude/projects, ~/.codex/sessions, and the OpenCode storage
  dir are never touched.

Examples:
  burn state reset                 # dry-run; prints what would be deleted
  burn state reset --force         # delete derived state, leave config alone
  burn state reset --force --reingest
`;

const PRUNE_HELP = `burn state prune - prune expired content sidecars

Usage:
  burn state prune [--days <n>] [--force]

Flags:
  --days <n>   override retention (number of days, or 'forever' to disable)
  --force      delete sidecars even when the source session file still exists.
               Default behavior keeps recoverable sidecars in place because
               'burn state rebuild content' can rederive them from the source.

Examples:
  burn state prune
  burn state prune --days 30
  burn state prune --force
`;

interface IndexFileStatus {
  path: string;
  exists: boolean;
  bytes: number;
  entries: number;
}

interface StateStatus {
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
  archive: ArchiveStatus;
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

export async function runState(args: ParsedArgs): Promise<number> {
  if (args.flags['help'] === true) {
    process.stdout.write(STATE_HELP);
    return 0;
  }

  const sub = args.positional[0];
  switch (sub) {
    case undefined:
    case 'status':
      return runStatus(args);
    case 'help':
      process.stdout.write(STATE_HELP);
      return 0;
    case 'rebuild':
      return runStateRebuild(args);
    case 'prune':
      return runStatePrune(args);
    case 'reset':
      return runStateReset(args);
    default:
      process.stderr.write(`burn state: unknown subcommand: ${sub}\n\n${STATE_HELP}`);
      return 1;
  }
}

async function runStateRebuild(args: ParsedArgs): Promise<number> {
  const target = args.positional[1];
  switch (target) {
    case 'help':
    case '--help':
    case '-h':
      process.stdout.write(REBUILD_HELP);
      return 0;
    case undefined:
      process.stderr.write(`burn state rebuild: missing target\n\n${REBUILD_HELP}`);
      return 2;
    case 'index':
      return runIndex();
    case 'classify':
      return runClassify(args);
    case 'content':
      return runContentRebuild();
    case 'archive':
      return runArchiveTarget(args);
    case 'all':
      return runAll(args);
    default:
      process.stderr.write(`burn state rebuild: unknown target: ${target}\n\n${REBUILD_HELP}`);
      return 1;
  }
}

async function runArchiveTarget(args: ParsedArgs): Promise<number> {
  const action = args.positional[2];
  const vacuum = args.flags['vacuum'] === true || action === 'vacuum';
  if (action !== undefined && action !== 'build' && action !== 'vacuum') {
    process.stderr.write(`burn state rebuild archive: unknown action: ${action}\n\n${REBUILD_HELP}`);
    return 1;
  }
  if (vacuum && args.flags['full'] === true) {
    process.stderr.write(`burn state rebuild archive: --full and --vacuum cannot be combined\n`);
    return 1;
  }
  const { runArchiveBuild, runArchiveVacuum } = await import('./archive.js');
  if (vacuum) return runArchiveVacuum(args);
  return runArchiveBuild(args, { full: args.flags['full'] === true });
}

async function runAll(args: ParsedArgs): Promise<number> {
  const lines: string[] = [];
  await rebuildContent(lines);
  await rebuildIndexTarget(lines);
  await rebuildClassify(lines, args.flags['force'] === true);
  const { runArchiveBuild } = await import('./archive.js');
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

async function runContentRebuild(): Promise<number> {
  const lines: string[] = [];
  await rebuildContent(lines);
  process.stdout.write(lines.join('\n') + '\n');
  return 0;
}

async function rebuildClassify(lines: string[], force: boolean): Promise<void> {
  const { reclassifyLedger } = await import('@relayburn/ledger');
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
  const { reingestMissingContent } = await import('../ingest.js');
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
  const { rebuildIndex } = await import('@relayburn/ledger');
  const { ids, content } = await rebuildIndex();
  lines.push(
    `rebuilt ledger index: ${formatInt(ids)} id hashes, ${formatInt(content)} content fingerprints`,
  );
}

async function runStatus(args: ParsedArgs): Promise<number> {
  const status = await collectStateStatus();
  if (args.flags['json'] === true) {
    process.stdout.write(JSON.stringify(status, null, 2) + '\n');
    return 0;
  }
  const { formatArchiveStatusLines } = await import('./archive.js');
  process.stdout.write(formatStateStatusLines(status, formatArchiveStatusLines).join('\n') + '\n');
  return 0;
}

async function collectStateStatus(): Promise<StateStatus> {
  const {
    getArchiveStatus,
    ledgerContentIndexPath,
    ledgerIndexPath,
  } = await import('@relayburn/ledger');
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
  const { contentDir, isValidSessionId } = await import('@relayburn/ledger');
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
  const { isTurnLine, isUserTurnLine, ledgerPath } = await import('@relayburn/ledger');
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

function formatStateStatusLines(
  status: StateStatus,
  formatArchiveStatusLines: FormatArchiveStatusLines,
): string[] {
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

async function runStatePrune(args: ParsedArgs): Promise<number> {
  const { loadConfig, pruneContent, retentionMs } = await import('@relayburn/ledger');
  const cfg = await loadConfig();
  let retention: number | 'forever';
  if (typeof args.flags['days'] === 'string') {
    const parsed = parseRetention(args.flags['days']);
    if (parsed === null) {
      process.stderr.write(
        `burn state prune: invalid --days value: ${JSON.stringify(args.flags['days'])} (expected a number or "forever")\n\n${PRUNE_HELP}`,
      );
      return 2;
    }
    retention = parsed;
  } else {
    retention = cfg.content.retentionDays;
  }
  const ms = retentionMs(retention);
  if (ms === null) {
    process.stdout.write(`content retention=forever - nothing to prune\n`);
    return 0;
  }
  const force = args.flags['force'] === true || isForceEnv();
  const opts: Parameters<LedgerModule['pruneContent']>[0] = { olderThanMs: ms };
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
        `  (use 'burn state prune --force' to delete them anyway)\n`,
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

// --- reset -----------------------------------------------------------------

interface ResetTargetReport {
  path: string;
  kind: 'file' | 'dir';
  existed: boolean;
  bytes: number;
  filesRemoved: number;
}

interface ResetSummary {
  home: string;
  forced: boolean;
  reingestRequested: boolean;
  reingested: { scannedSessions: number; ingestedSessions: number; appendedTurns: number } | null;
  targets: ResetTargetReport[];
  totals: { filesRemoved: number; bytesFreed: number };
}

async function runStateReset(args: ParsedArgs): Promise<number> {
  if (args.flags['help'] === true || args.positional[1] === 'help') {
    process.stdout.write(RESET_HELP);
    return 0;
  }
  const { invalidateIndexCache, ledgerHome, withLock } = await import('@relayburn/ledger');
  const force = args.flags['force'] === true;
  const reingest = args.flags['reingest'] === true;
  const json = args.flags['json'] === true;

  const home = ledgerHome();
  const targets = await collectResetTargets();

  if (!force) {
    // Dry run is read-only — no lock needed.
    const summary = buildResetSummary({ home, targets, forced: false, reingestRequested: reingest, reingested: null });
    if (reingest) {
      process.stderr.write(
        `burn state reset: --reingest requires --force; nothing was deleted.\n`,
      );
    }
    writeResetSummary(summary, { json, dryRun: true });
    return 0;
  }

  // Hold the ledger lock only across the deletion. ingestAll() can run for
  // far longer than STALE_MS (lock.ts), so keeping the lock open across it
  // would let a concurrent burn process treat this lock as stale and unlink
  // it mid-run, defeating the mutex. The reingest path takes the lock per
  // append internally.
  const applied = await withLock('ledger', async () => {
    const out: ResetTargetReport[] = [];
    for (const t of targets) out.push(await applyResetTarget(t, home));
    // The on-disk index was just unlinked; drop the in-memory dedup cache
    // so any subsequent appendTurns / appendContent re-derives from the
    // empty files instead of skipping records as duplicates of hashes
    // loaded before the wipe.
    invalidateIndexCache();
    return out;
  });

  let reingested: ResetSummary['reingested'] = null;
  if (reingest) {
    const { ingestAll } = await import('../ingest.js');
    const r = await ingestAll();
    reingested = {
      scannedSessions: r.scannedSessions,
      ingestedSessions: r.ingestedSessions,
      appendedTurns: r.appendedTurns,
    };
  }

  const summary = buildResetSummary({ home, targets: applied, forced: true, reingestRequested: reingest, reingested });
  writeResetSummary(summary, { json, dryRun: false });
  return 0;
}

async function collectResetTargets(): Promise<ResetTargetReport[]> {
  const {
    archivePath,
    contentDir,
    cursorsPath,
    hwmPath,
    ledgerContentIndexPath,
    ledgerIndexPath,
    ledgerPath,
  } = await import('@relayburn/ledger');
  // Order doesn't matter for correctness, but file sidecars first reads
  // nicer in the dry-run output: small to large, ledger -> archive -> content.
  const archive = archivePath();
  const filePaths: string[] = [
    ledgerPath(),
    ledgerIndexPath(),
    ledgerContentIndexPath(),
    cursorsPath(),
    hwmPath(),
    archive,
    `${archive}-wal`,
    `${archive}-shm`,
  ];
  const reports: ResetTargetReport[] = [];
  for (const p of filePaths) {
    reports.push(await statResetFile(p));
  }
  reports.push(await statResetDir(contentDir()));
  return reports;
}

async function statResetFile(p: string): Promise<ResetTargetReport> {
  try {
    const st = await stat(p);
    return {
      path: p,
      kind: 'file',
      existed: st.isFile(),
      bytes: st.isFile() ? st.size : 0,
      filesRemoved: st.isFile() ? 1 : 0,
    };
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
    return { path: p, kind: 'file', existed: false, bytes: 0, filesRemoved: 0 };
  }
}

async function statResetDir(dir: string): Promise<ResetTargetReport> {
  try {
    const st = await stat(dir);
    if (!st.isDirectory()) {
      return { path: dir, kind: 'dir', existed: false, bytes: 0, filesRemoved: 0 };
    }
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
    return { path: dir, kind: 'dir', existed: false, bytes: 0, filesRemoved: 0 };
  }
  let bytes = 0;
  let files = 0;
  for await (const child of walkDirEntries(dir)) {
    bytes += child.size;
    files += 1;
  }
  return { path: dir, kind: 'dir', existed: true, bytes, filesRemoved: files };
}

async function* walkDirEntries(
  dir: string,
): AsyncGenerator<{ path: string; size: number }> {
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === 'ENOENT') return;
    throw err;
  }
  for (const e of entries) {
    const full = path.join(dir, e.name);
    if (e.isDirectory()) {
      yield* walkDirEntries(full);
    } else if (e.isFile()) {
      try {
        const st = await stat(full);
        yield { path: full, size: st.size };
      } catch (err) {
        if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
      }
    }
  }
}

async function applyResetTarget(
  t: ResetTargetReport,
  home: string,
): Promise<ResetTargetReport> {
  // Belt-and-suspenders: refuse to touch anything that resolves outside
  // ledgerHome(). The constants we feed in already live there, but a
  // misconfigured RELAYBURN_HOME or future refactor shouldn't be able to
  // turn this into an arbitrary-rm.
  assertInsideHome(t.path, home);
  if (!t.existed) return t;
  if (t.kind === 'file') {
    try {
      await unlink(t.path);
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === 'ENOENT') {
        return { ...t, existed: false, bytes: 0, filesRemoved: 0 };
      }
      throw err;
    }
    return t;
  }
  await rm(t.path, { recursive: true, force: true });
  return t;
}

function assertInsideHome(target: string, home: string): void {
  const rel = path.relative(home, target);
  if (rel === '' || rel.startsWith('..') || path.isAbsolute(rel)) {
    throw new Error(
      `burn state reset: refusing to touch path outside ledger home: ${target}`,
    );
  }
}

interface BuildResetSummaryOpts {
  home: string;
  targets: ResetTargetReport[];
  forced: boolean;
  reingestRequested: boolean;
  reingested: ResetSummary['reingested'];
}

function buildResetSummary(opts: BuildResetSummaryOpts): ResetSummary {
  let bytes = 0;
  let files = 0;
  for (const t of opts.targets) {
    bytes += t.bytes;
    files += t.filesRemoved;
  }
  return {
    home: opts.home,
    forced: opts.forced,
    reingestRequested: opts.reingestRequested,
    reingested: opts.reingested,
    targets: opts.targets,
    totals: { filesRemoved: files, bytesFreed: bytes },
  };
}

function writeResetSummary(
  summary: ResetSummary,
  opts: { json: boolean; dryRun: boolean },
): void {
  if (opts.json) {
    process.stdout.write(JSON.stringify({ ...summary, dryRun: opts.dryRun }, null, 2) + '\n');
    return;
  }
  const lines: string[] = [];
  if (opts.dryRun) {
    lines.push(`dry run: would reset derived state under ${summary.home}`);
  } else {
    lines.push(`reset derived state under ${summary.home}`);
  }
  for (const t of summary.targets) {
    const tag = t.existed ? `${formatInt(t.filesRemoved)} file${t.filesRemoved === 1 ? '' : 's'}, ${formatBytes(t.bytes)}` : 'already absent';
    lines.push(`  ${t.kind === 'dir' ? 'dir ' : 'file'} ${t.path}: ${tag}`);
  }
  if (opts.dryRun) {
    lines.push(
      `total: ${formatInt(summary.totals.filesRemoved)} file${summary.totals.filesRemoved === 1 ? '' : 's'}, ${formatBytes(summary.totals.bytesFreed)} would be freed`,
    );
    lines.push(`re-run with --force to apply.`);
  } else {
    lines.push(
      `total: removed ${formatInt(summary.totals.filesRemoved)} file${summary.totals.filesRemoved === 1 ? '' : 's'}, ${formatBytes(summary.totals.bytesFreed)} freed`,
    );
    if (summary.reingested) {
      lines.push(
        `reingested ${formatInt(summary.reingested.ingestedSessions)} of ${formatInt(summary.reingested.scannedSessions)} sessions (${formatInt(summary.reingested.appendedTurns)} turns appended)`,
      );
    }
  }
  process.stdout.write(lines.join('\n') + '\n');
}

export async function opportunisticPrune(): Promise<void> {
  try {
    const { loadConfig, pruneContent, retentionMs } = await import('@relayburn/ledger');
    const cfg = await loadConfig();
    if (cfg.content.store === 'off') return;
    const ms = retentionMs(cfg.content.retentionDays);
    if (ms === null) return;
    // Opportunistic prune always applies the recoverable-source check.
    // Reclaiming recoverable disk requires explicit `burn state prune --force`.
    // The exception is RELAYBURN_PRUNE_FORCE=1 for unattended automation that
    // genuinely wants the old behavior.
    const opts: Parameters<LedgerModule['pruneContent']>[0] = { olderThanMs: ms };
    if (!isForceEnv()) {
      const sources = await loadSourceSessionIds();
      opts.isRecoverable = (sessionId) => sources.has(sessionId);
    }
    await pruneContent(opts);
  } catch (err) {
    // Best-effort - never fail a CLI operation because of prune, but surface
    // the reason on stderr so persistent failures are diagnosable.
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(`[burn] opportunistic content prune failed: ${msg}\n`);
  }
}

// --- source index ----------------------------------------------------------
//
// Walk the same source roots that `ingest.ts` uses and build an in-memory
// Set<sessionId>. Used to answer "is the upstream agent's session file still
// on disk?" - if yes, the sidecar is recoverable via `burn state rebuild content`
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
    const { walkJsonl } = await import('../walk.js');
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
    const { walkOpencodeSessions } = await import('../walk.js');
    files = await walkOpencodeSessions(OPENCODE_SESSION_ROOT);
  } catch {
    return;
  }
  for (const file of files) {
    out.add(path.basename(file, '.json'));
  }
}
