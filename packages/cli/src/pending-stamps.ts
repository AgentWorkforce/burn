import { randomUUID } from 'node:crypto';
import {
  mkdir,
  readdir,
  readFile,
  rename,
  stat,
  unlink,
  writeFile,
} from 'node:fs/promises';
import * as path from 'node:path';

import {
  ledgerHome,
  stamp,
  type Enrichment,
} from '@relayburn/ledger';

export type PendingStampHarness = 'codex' | 'opencode';

export interface PendingStamp {
  v: 1;
  harness: PendingStampHarness;
  spawnerPid: number;
  spawnStartTs: string;
  cwd: string;
  enrichment: Enrichment;
  sessionDirHint?: string;
}

export interface PendingStampWriteResult {
  file: string;
  stamp: PendingStamp;
}

export interface PendingStampSessionCandidate {
  harness: PendingStampHarness;
  sessionId: string;
  sessionPath: string;
  sessionMtimeMs?: number;
  cwd?: string;
}

export interface PendingStampResolveResult {
  applied: number;
  enrichment: Enrichment;
}

export interface PendingStampCleanupResult {
  scanned: number;
  deleted: number;
}

const DAY_MS = 24 * 60 * 60 * 1000;
export const PENDING_STAMP_TTL_MS = DAY_MS;
const MTIME_SLOP_MS = 1;

export function pendingStampsDir(): string {
  return path.join(ledgerHome(), 'pending-stamps');
}

export async function writePendingStamp(opts: {
  harness: PendingStampHarness;
  cwd: string;
  enrichment: Enrichment;
  sessionDirHint?: string;
  spawnStartTs?: Date;
  spawnerPid?: number;
}): Promise<PendingStampWriteResult> {
  const spawnStart = opts.spawnStartTs ?? new Date();
  await cleanupStalePendingStamps({ now: spawnStart });

  const record: PendingStamp = {
    v: 1,
    harness: opts.harness,
    spawnerPid: opts.spawnerPid ?? process.pid,
    spawnStartTs: spawnStart.toISOString(),
    cwd: path.resolve(opts.cwd),
    enrichment: { ...opts.enrichment },
  };
  if (opts.sessionDirHint !== undefined) record.sessionDirHint = path.resolve(opts.sessionDirHint);

  const dir = pendingStampsDir();
  await mkdir(dir, { recursive: true });
  const base = [
    opts.harness,
    record.spawnerPid,
    spawnStart.getTime(),
    randomUUID(),
  ].join('-');
  const file = path.join(dir, `${base}.json`);
  const tmp = path.join(dir, `${base}.tmp-${process.pid}-${randomUUID()}`);
  await writeFile(tmp, JSON.stringify(record, null, 2) + '\n', 'utf8');
  await rename(tmp, file);
  return { file, stamp: record };
}

export async function cleanupStalePendingStamps(opts: {
  now?: Date;
  ttlMs?: number;
} = {}): Promise<PendingStampCleanupResult> {
  const nowMs = (opts.now ?? new Date()).getTime();
  const ttlMs = opts.ttlMs ?? PENDING_STAMP_TTL_MS;
  const files = await listPendingStampFiles();
  let deleted = 0;

  for (const file of files) {
    let shouldDelete = false;
    try {
      const raw = await readFile(file, 'utf8');
      const parsed = parsePendingStamp(raw);
      if (parsed) {
        const spawnMs = Date.parse(parsed.spawnStartTs);
        shouldDelete = Number.isFinite(spawnMs) && nowMs - spawnMs > ttlMs;
      } else {
        const s = await stat(file);
        shouldDelete = nowMs - s.mtimeMs > ttlMs;
      }
    } catch {
      shouldDelete = true;
    }

    if (shouldDelete) {
      try {
        await unlink(file);
        deleted++;
      } catch {
        // Best-effort cleanup only.
      }
    }
  }

  return { scanned: files.length, deleted };
}

export async function resolvePendingStampsForSession(
  candidate: PendingStampSessionCandidate,
  opts: { now?: Date; ttlMs?: number } = {},
): Promise<PendingStampResolveResult> {
  if (candidate.sessionId.length === 0) return { applied: 0, enrichment: {} };

  await cleanupStalePendingStamps(opts);
  const files = await listPendingStampFiles({ activeOnly: true });
  const matches: Array<{ file: string; record: PendingStamp }> = [];
  for (const file of files) {
    let record: PendingStamp | null = null;
    try {
      record = parsePendingStamp(await readFile(file, 'utf8'));
    } catch {
      continue;
    }
    if (!record) continue;
    if (pendingStampMatches(record, candidate)) matches.push({ file, record });
  }

  matches.sort((a, b) => a.record.spawnStartTs.localeCompare(b.record.spawnStartTs));

  const enrichment: Enrichment = {};
  let applied = 0;
  for (const { file, record } of matches) {
    const claimed = await claimPendingStamp(file);
    if (!claimed) continue;
    try {
      await stamp({ sessionId: candidate.sessionId }, record.enrichment);
      Object.assign(enrichment, record.enrichment);
      applied++;
      await unlink(claimed).catch(() => undefined);
    } catch (err) {
      await rename(claimed, file).catch(() => undefined);
      throw err;
    }
  }

  return { applied, enrichment };
}

function pendingStampMatches(
  record: PendingStamp,
  candidate: PendingStampSessionCandidate,
): boolean {
  if (record.harness !== candidate.harness) return false;
  if (record.sessionDirHint !== undefined) {
    const rel = path.relative(record.sessionDirHint, path.resolve(candidate.sessionPath));
    if (rel.startsWith('..') || path.isAbsolute(rel)) return false;
  }

  const spawnMs = Date.parse(record.spawnStartTs);
  if (!Number.isFinite(spawnMs)) return false;
  if (
    candidate.sessionMtimeMs !== undefined &&
    candidate.sessionMtimeMs + MTIME_SLOP_MS < spawnMs
  ) {
    return false;
  }

  if (candidate.cwd !== undefined) {
    return path.resolve(candidate.cwd) === path.resolve(record.cwd);
  }

  // Fallback when the reader cannot recover a session cwd: use the same mtime
  // causality window the old OpenCode wrapper used for post-spawn discovery.
  return candidate.sessionMtimeMs !== undefined && candidate.sessionMtimeMs + MTIME_SLOP_MS >= spawnMs;
}

async function claimPendingStamp(file: string): Promise<string | null> {
  const claimed = `${file}.claimed-${process.pid}-${randomUUID()}`;
  try {
    await rename(file, claimed);
    return claimed;
  } catch {
    return null;
  }
}

async function listPendingStampFiles(opts: { activeOnly?: boolean } = {}): Promise<string[]> {
  try {
    const entries = await readdir(pendingStampsDir(), {
      withFileTypes: true,
      encoding: 'utf8',
    });
    return entries
      .filter((e) => e.isFile() && (!opts.activeOnly || e.name.endsWith('.json')))
      .map((e) => path.join(pendingStampsDir(), e.name));
  } catch {
    return [];
  }
}

function parsePendingStamp(raw: string): PendingStamp | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== 'object') return null;
  const p = parsed as Partial<PendingStamp>;
  if (p.v !== 1) return null;
  if (p.harness !== 'codex' && p.harness !== 'opencode') return null;
  if (typeof p.spawnerPid !== 'number' || !Number.isFinite(p.spawnerPid)) return null;
  if (typeof p.spawnStartTs !== 'string' || Number.isNaN(Date.parse(p.spawnStartTs))) return null;
  if (typeof p.cwd !== 'string' || p.cwd.length === 0) return null;
  if (!p.enrichment || typeof p.enrichment !== 'object' || Array.isArray(p.enrichment)) {
    return null;
  }
  for (const [k, v] of Object.entries(p.enrichment)) {
    if (typeof k !== 'string' || typeof v !== 'string') return null;
  }
  if (p.sessionDirHint !== undefined && typeof p.sessionDirHint !== 'string') return null;
  const out: PendingStamp = {
    v: 1,
    harness: p.harness,
    spawnerPid: p.spawnerPid,
    spawnStartTs: p.spawnStartTs,
    cwd: p.cwd,
    enrichment: { ...(p.enrichment as Enrichment) },
  };
  if (p.sessionDirHint !== undefined) out.sessionDirHint = p.sessionDirHint;
  return out;
}
