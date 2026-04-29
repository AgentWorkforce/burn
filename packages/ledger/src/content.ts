import {
  appendFile,
  mkdir,
  readFile,
  readdir,
  stat,
  unlink,
  utimes,
} from 'node:fs/promises';
import * as path from 'node:path';

import type { ContentRecord } from '@relayburn/reader';

import { withLock } from './lock.js';
import { contentDir, contentFilePath, isValidSessionId } from './paths.js';

export interface PruneOptions {
  olderThanMs: number;
  // When provided, prune skips sessions for which the callback returns true —
  // the source session file still exists upstream and `burn state rebuild content`
  // can rederive the sidecar at any time, so deleting it would be silently
  // lossy. The ledger package stays decoupled from adapter-specific paths;
  // the caller (CLI) builds the source index and supplies the predicate.
  isRecoverable?: (sessionId: string) => boolean | Promise<boolean>;
}

export interface PruneResult {
  filesDeleted: number;
  bytesFreed: number;
  // Sidecars left in place because `isRecoverable(sessionId)` returned true.
  // Counted separately from `filesDeleted` so callers can surface a "kept N
  // recoverable sidecars" line and point users at `--force` if they really
  // want to reclaim that disk.
  skippedRecoverable: number;
}

export interface ReadContentSelector {
  sessionId: string;
  messageId?: string;
}

export async function appendContent(records: ContentRecord[]): Promise<void> {
  if (records.length === 0) return;
  const grouped = new Map<string, ContentRecord[]>();
  for (const r of records) {
    const key = r.sessionId;
    if (!key) continue;
    if (!isValidSessionId(key)) {
      process.stderr.write(
        `[burn] skipping content record with unsafe sessionId: ${JSON.stringify(key)}\n`,
      );
      continue;
    }
    let bucket = grouped.get(key);
    if (!bucket) {
      bucket = [];
      grouped.set(key, bucket);
    }
    bucket.push(r);
  }
  if (grouped.size === 0) return;
  await mkdir(contentDir(), { recursive: true });
  for (const [sessionId, items] of grouped) {
    await appendSessionContent(sessionId, items);
  }
}

async function appendSessionContent(
  sessionId: string,
  records: ContentRecord[],
): Promise<void> {
  const file = contentFilePath(sessionId);
  await withLock(`content.${sessionId}`, async () => {
    const lines = records.map((r) => JSON.stringify(r));
    let existing = new Set<string>();
    try {
      const raw = await readFile(file, 'utf8');
      existing = new Set(raw.split('\n').filter((line) => line.length > 0));
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== 'ENOENT') throw err;
    }
    const fresh = lines.filter((line) => {
      if (existing.has(line)) return false;
      existing.add(line);
      return true;
    });
    if (fresh.length === 0) return;
    const payload = fresh.join('\n') + '\n';
    await appendFile(file, payload, { encoding: 'utf8' });
  });
}

export async function readContent(
  selector: ReadContentSelector,
): Promise<ContentRecord[]> {
  const file = contentFilePath(selector.sessionId);
  let raw: string;
  try {
    raw = await readFile(file, 'utf8');
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === 'ENOENT') return [];
    throw err;
  }
  const out: ContentRecord[] = [];
  for (const line of raw.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(trimmed);
    } catch {
      continue;
    }
    if (!parsed || typeof parsed !== 'object') continue;
    const rec = parsed as ContentRecord;
    if (selector.messageId !== undefined && rec.messageId !== selector.messageId) continue;
    out.push(rec);
  }
  return out;
}

export async function pruneContent(options: PruneOptions): Promise<PruneResult> {
  const dir = contentDir();
  let entries: string[];
  try {
    entries = await readdir(dir);
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === 'ENOENT') {
      return { filesDeleted: 0, bytesFreed: 0, skippedRecoverable: 0 };
    }
    throw err;
  }
  const cutoff = Date.now() - options.olderThanMs;
  const isRecoverable = options.isRecoverable;
  let filesDeleted = 0;
  let bytesFreed = 0;
  let skippedRecoverable = 0;
  for (const name of entries) {
    if (!name.endsWith('.jsonl')) continue;
    const sessionId = name.slice(0, -'.jsonl'.length);
    if (!isValidSessionId(sessionId)) continue;
    const full = path.join(dir, name);
    // Acquire the same per-session lock used by appendSessionContent so a
    // prune cannot race with an in-flight write for this session. We re-stat
    // inside the lock to ensure we're deciding on the post-write mtime.
    type Outcome =
      | { kind: 'deleted'; size: number }
      | { kind: 'skippedRecoverable' }
      | null;
    const outcome: Outcome = await withLock(`content.${sessionId}`, async () => {
      let st: Awaited<ReturnType<typeof stat>>;
      try {
        st = await stat(full);
      } catch {
        return null;
      }
      if (!st.isFile()) return null;
      // Inclusive cutoff: files whose mtime equals now - olderThanMs are
      // eligible. This also makes `pruneContent({olderThanMs: 0})` clear the
      // directory reliably.
      if (st.mtimeMs > cutoff) return null;
      // Source-aware protection: if the upstream agent's session file still
      // exists, the sidecar is recoverable via `burn state rebuild content`, so
      // deleting it on retention alone is silently lossy. Callers opt in by
      // supplying `isRecoverable`; the ledger package itself never reaches
      // out to adapter-specific paths.
      if (isRecoverable) {
        try {
          if (await isRecoverable(sessionId)) {
            return { kind: 'skippedRecoverable' };
          }
        } catch {
          // If the predicate throws, fall through to the existing behavior —
          // we'd rather prune than fail-open and accumulate forever on a
          // broken source index.
        }
      }
      try {
        await unlink(full);
        return { kind: 'deleted', size: st.size };
      } catch {
        // raced with another deleter or was already gone
        return null;
      }
    });
    if (outcome?.kind === 'deleted') {
      filesDeleted++;
      bytesFreed += outcome.size;
    } else if (outcome?.kind === 'skippedRecoverable') {
      skippedRecoverable++;
    }
  }
  return { filesDeleted, bytesFreed, skippedRecoverable };
}

export async function listContentSessionIds(): Promise<Set<string>> {
  const dir = contentDir();
  const out = new Set<string>();
  let entries: string[];
  try {
    entries = await readdir(dir);
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === 'ENOENT') return out;
    throw err;
  }
  for (const name of entries) {
    if (!name.endsWith('.jsonl')) continue;
    const sessionId = name.slice(0, -'.jsonl'.length);
    if (!isValidSessionId(sessionId)) continue;
    // An empty sidecar signals "attempted but nothing written" and should be
    // re-parsed rather than treated as already-populated.
    try {
      const st = await stat(path.join(dir, name));
      if (st.size > 0) out.add(sessionId);
    } catch {
      // raced with deletion; ignore
    }
  }
  return out;
}

// Test helper: set the mtime/atime on a session's content file.
export async function __setContentFileMtimeForTesting(
  sessionId: string,
  when: Date,
): Promise<void> {
  await utimes(contentFilePath(sessionId), when, when);
}
