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
}

export interface PruneResult {
  filesDeleted: number;
  bytesFreed: number;
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
  const payload = records.map((r) => JSON.stringify(r)).join('\n') + '\n';
  await withLock(`content.${sessionId}`, async () => {
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
      return { filesDeleted: 0, bytesFreed: 0 };
    }
    throw err;
  }
  const cutoff = Date.now() - options.olderThanMs;
  let filesDeleted = 0;
  let bytesFreed = 0;
  for (const name of entries) {
    if (!name.endsWith('.jsonl')) continue;
    const sessionId = name.slice(0, -'.jsonl'.length);
    if (!isValidSessionId(sessionId)) continue;
    const full = path.join(dir, name);
    // Acquire the same per-session lock used by appendSessionContent so a
    // prune cannot race with an in-flight write for this session. We re-stat
    // inside the lock to ensure we're deciding on the post-write mtime.
    const outcome = await withLock(`content.${sessionId}`, async () => {
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
      try {
        await unlink(full);
        return { size: st.size };
      } catch {
        // raced with another deleter or was already gone
        return null;
      }
    });
    if (outcome) {
      filesDeleted++;
      bytesFreed += outcome.size;
    }
  }
  return { filesDeleted, bytesFreed };
}

// Test helper: set the mtime/atime on a session's content file.
export async function __setContentFileMtimeForTesting(
  sessionId: string,
  when: Date,
): Promise<void> {
  await utimes(contentFilePath(sessionId), when, when);
}
