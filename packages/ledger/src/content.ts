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
import { contentDir, contentFilePath } from './paths.js';

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
    const full = path.join(dir, name);
    let st: Awaited<ReturnType<typeof stat>>;
    try {
      st = await stat(full);
    } catch {
      continue;
    }
    if (!st.isFile()) continue;
    if (st.mtimeMs >= cutoff) continue;
    try {
      await unlink(full);
      filesDeleted++;
      bytesFreed += st.size;
    } catch {
      // ignore — may have been removed concurrently
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
