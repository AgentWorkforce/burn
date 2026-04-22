import { open, stat, unlink, mkdir } from 'node:fs/promises';
import * as path from 'node:path';

import { lockPath } from './paths.js';

const STALE_MS = 30_000;
const RETRY_DELAY_MS = 20;
const MAX_RETRIES = 50;

export async function withLock<T>(name: string, fn: () => Promise<T>): Promise<T> {
  const lp = lockPath(name);
  await mkdir(path.dirname(lp), { recursive: true });
  await acquire(lp);
  try {
    return await fn();
  } finally {
    try {
      await unlink(lp);
    } catch {
      // ignore; next acquirer will see it as stale
    }
  }
}

async function acquire(lp: string): Promise<void> {
  for (let attempt = 0; attempt < MAX_RETRIES; attempt++) {
    try {
      const handle = await open(lp, 'wx');
      await handle.close();
      return;
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== 'EEXIST') throw err;
      const st = await stat(lp).catch(() => null);
      if (st && Date.now() - st.mtimeMs > STALE_MS) {
        try {
          await unlink(lp);
        } catch {
          // raced with another process; loop and retry
        }
        continue;
      }
      await delay(RETRY_DELAY_MS);
    }
  }
  throw new Error(`could not acquire lock after ${MAX_RETRIES} attempts: ${lp}`);
}

function delay(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
