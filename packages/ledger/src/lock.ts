import { AsyncLocalStorage } from 'node:async_hooks';
import { open, stat, unlink, mkdir } from 'node:fs/promises';
import * as path from 'node:path';

import { lockPath } from './paths.js';

// Two-phase acquire:
//
//   Phase 1 (fast): cheap retries that cover normal in-process contention
//   between concurrent writers. ~1s total.
//
//   Phase 2 (slow): longer waits that outlast STALE_MS so a single invocation
//   can self-heal an orphan lock left by a crashed process. ~10s total —
//   spans the stale-detection window twice.
//
// The previous tuning (RETRY_DELAY_MS=20, MAX_RETRIES=50, STALE_MS=30_000)
// gave a 1s retry budget against a 30s stale threshold, leaving a 29s window
// where any acquirer would hard-fail with "could not acquire lock after 50
// attempts" even though the lock was already orphaned. See #62.
export const FAST_RETRY_DELAY_MS = 20;
export const FAST_RETRIES = 50; // 1s — normal concurrent-writer contention
export const SLOW_RETRY_DELAY_MS = 250;
export const SLOW_RETRIES = 40; // 10s — covers an orphan twice over
export const STALE_MS = 5_000;

const TOTAL_RETRY_BUDGET_MS =
  FAST_RETRY_DELAY_MS * FAST_RETRIES + SLOW_RETRY_DELAY_MS * SLOW_RETRIES;

// Invariant the issue calls out: the retry budget MUST outlast the stale
// threshold (plus at least one retry cycle) so a single invocation can wait
// out an orphan and unlink it. Asserting at module-load time means future
// tuning of either constant can't silently reintroduce the gap.
if (TOTAL_RETRY_BUDGET_MS <= STALE_MS + SLOW_RETRY_DELAY_MS) {
  throw new Error(
    `lock retry budget (${TOTAL_RETRY_BUDGET_MS}ms) must exceed STALE_MS ` +
      `(${STALE_MS}ms) + one slow retry (${SLOW_RETRY_DELAY_MS}ms)`,
  );
}

export interface AcquireOptions {
  fastRetries: number;
  fastRetryDelayMs: number;
  slowRetries: number;
  slowRetryDelayMs: number;
  staleMs: number;
}

const DEFAULT_OPTIONS: AcquireOptions = {
  fastRetries: FAST_RETRIES,
  fastRetryDelayMs: FAST_RETRY_DELAY_MS,
  slowRetries: SLOW_RETRIES,
  slowRetryDelayMs: SLOW_RETRY_DELAY_MS,
  staleMs: STALE_MS,
};

const heldLocks = new AsyncLocalStorage<Set<string>>();

export async function withLock<T>(name: string, fn: () => Promise<T>): Promise<T> {
  const lp = lockPath(name);
  const held = heldLocks.getStore();
  if (held?.has(lp)) return fn();

  await mkdir(path.dirname(lp), { recursive: true });
  await acquire(lp, DEFAULT_OPTIONS);
  const nextHeld = new Set(held ?? []);
  nextHeld.add(lp);
  try {
    return await heldLocks.run(nextHeld, fn);
  } finally {
    try {
      await unlink(lp);
    } catch {
      // ignore; next acquirer will see it as stale
    }
  }
}

// Exported for tests that need to drive the timeout path with a tiny budget
// without holding a real lock for ~11 seconds.
export async function __acquireForTesting(
  lp: string,
  options: AcquireOptions,
): Promise<void> {
  await mkdir(path.dirname(lp), { recursive: true });
  await acquire(lp, options);
}

async function acquire(lp: string, options: AcquireOptions): Promise<void> {
  // Track which failure mode we kept hitting so the timeout error can name
  // the side it timed out on instead of the ambiguous "after N attempts".
  let lastReason: 'live' | 'stale-unlink-failed' = 'live';

  const totalRetries = options.fastRetries + options.slowRetries;
  const budgetMs =
    options.fastRetries * options.fastRetryDelayMs +
    options.slowRetries * options.slowRetryDelayMs;
  for (let attempt = 0; attempt < totalRetries; attempt++) {
    try {
      const handle = await open(lp, 'wx');
      await handle.close();
      return;
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== 'EEXIST') throw err;
      const st = await stat(lp).catch(() => null);
      if (st && Date.now() - st.mtimeMs > options.staleMs) {
        try {
          await unlink(lp);
          // unlink succeeded — loop and retry the open immediately. Don't
          // count this as a stale-unlink-failed attempt.
          continue;
        } catch (unlinkErr) {
          const code = (unlinkErr as NodeJS.ErrnoException).code;
          if (code === 'ENOENT') {
            // Another acquirer already cleaned it up; race the open again.
            continue;
          }
          // Real failure (EPERM, EBUSY, …). Record it and back off so the
          // final error message can distinguish this from "live holder".
          lastReason = 'stale-unlink-failed';
        }
      } else {
        lastReason = 'live';
      }
      const delayMs =
        attempt < options.fastRetries ? options.fastRetryDelayMs : options.slowRetryDelayMs;
      await delay(delayMs);
    }
  }
  const detail =
    lastReason === 'stale-unlink-failed'
      ? 'lock appears stale but unlink kept failing'
      : 'held by live process';
  throw new Error(
    `could not acquire lock after ${totalRetries} attempts ` +
      `(~${budgetMs}ms) — ${detail}: ${lp}`,
  );
}

function delay(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
