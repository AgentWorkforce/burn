import { strict as assert } from 'node:assert';
import { mkdtemp, rm, stat, utimes, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { after, before, beforeEach, describe, it } from 'node:test';

import {
  __acquireForTesting,
  FAST_RETRIES,
  FAST_RETRY_DELAY_MS,
  SLOW_RETRIES,
  SLOW_RETRY_DELAY_MS,
  STALE_MS,
  withLock,
} from './lock.js';
import { lockPath } from './paths.js';

describe('withLock', () => {
  let tmp: string;
  const originalHome = process.env['RELAYBURN_HOME'];

  before(async () => {
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-lock-test-'));
  });

  beforeEach(async () => {
    await rm(tmp, { recursive: true, force: true });
    tmp = await mkdtemp(path.join(tmpdir(), 'burn-lock-test-'));
    process.env['RELAYBURN_HOME'] = tmp;
  });

  after(async () => {
    if (originalHome !== undefined) process.env['RELAYBURN_HOME'] = originalHome;
    else delete process.env['RELAYBURN_HOME'];
    await rm(tmp, { recursive: true, force: true });
  });

  it('fresh acquire creates and removes the lockfile', async () => {
    let ran = false;
    await withLock('fresh', async () => {
      ran = true;
      // Lock file exists for the duration of the critical section.
      await assert.doesNotReject(() => stat(lockPath('fresh')));
    });
    assert.equal(ran, true);
    // Lock file is unlinked after the critical section completes.
    await assert.rejects(() => stat(lockPath('fresh')), { code: 'ENOENT' });
  });

  it('recovers from an orphan lock with backdated mtime within a single invocation', async () => {
    // Simulate a process that crashed mid-write: a lockfile sitting on disk
    // with an mtime well past the stale threshold. The previous tuning
    // (1s retry budget, 30s stale threshold) made this a hard failure for
    // 29 seconds; the new tuning self-heals on the first retry.
    const lp = lockPath('orphan-test');
    await writeFile(lp, '', 'utf8');
    const longAgo = new Date(Date.now() - (STALE_MS + 5_000));
    await utimes(lp, longAgo, longAgo);

    const start = Date.now();
    let ran = false;
    await withLock('orphan-test', async () => {
      ran = true;
    });
    const elapsed = Date.now() - start;
    assert.equal(ran, true);
    // Should self-heal well inside a single CLI invocation. The first open
    // sees EEXIST, stat reports an old mtime, unlink succeeds, retry opens
    // cleanly — no `delay` calls in the recovery path.
    assert.ok(
      elapsed < 1_000,
      `orphan recovery took ${elapsed}ms; expected near-instant`,
    );
    await assert.rejects(() => stat(lp), { code: 'ENOENT' });
  });

  it('serializes live contention without spurious stale takeover', async () => {
    // Two parallel critical sections on the same lock name. Neither should
    // see the other's lockfile as stale (mtimes are fresh) and both should
    // complete in order.
    let inside = 0;
    let maxInside = 0;
    const log: string[] = [];
    async function critical(tag: string): Promise<void> {
      inside++;
      maxInside = Math.max(maxInside, inside);
      log.push(`enter:${tag}`);
      await new Promise((r) => setTimeout(r, 25));
      log.push(`exit:${tag}`);
      inside--;
    }
    await Promise.all([
      withLock('contend', () => critical('a')),
      withLock('contend', () => critical('b')),
    ]);
    assert.equal(maxInside, 1, 'lock must serialize critical sections');
    // Both critical sections completed.
    assert.equal(log.filter((l) => l.startsWith('exit')).length, 2);
  });

  it('exposes a retry budget that spans STALE_MS plus at least one retry cycle', async () => {
    // Static invariant: future tuning of the constants must not re-introduce
    // the #62 gap where the retry budget was shorter than the stale-detection
    // window. (The lock module also enforces this at load time, but asserting
    // here means the test suite flags it explicitly.)
    const totalBudgetMs =
      FAST_RETRIES * FAST_RETRY_DELAY_MS + SLOW_RETRIES * SLOW_RETRY_DELAY_MS;
    assert.ok(
      totalBudgetMs > STALE_MS + SLOW_RETRY_DELAY_MS,
      `retry budget (${totalBudgetMs}ms) must exceed STALE_MS (${STALE_MS}ms) + one slow retry (${SLOW_RETRY_DELAY_MS}ms)`,
    );
  });

  it('times out with a "held by live process" error when a fresh holder outlasts the budget', async () => {
    // Drive the timeout path with a tiny artificial budget so the test
    // doesn't have to wait the real 11s. Hold the lock with `withLock`,
    // then attempt a second acquire that uses a budget shorter than the
    // outer holder's runtime.
    const lp = lockPath('live-timeout');
    let holderReleased = false;
    const holder = withLock('live-timeout', async () => {
      // Hold for 200ms — longer than the second acquirer's 60ms budget.
      await new Promise((r) => setTimeout(r, 200));
      holderReleased = true;
    });
    // Give the holder a chance to take the lock.
    await new Promise((r) => setTimeout(r, 20));

    const tinyBudget = {
      fastRetries: 3,
      fastRetryDelayMs: 5,
      slowRetries: 3,
      slowRetryDelayMs: 15,
      staleMs: 60_000, // long enough that a fresh mtime is never stale
    };
    let err: Error | null = null;
    try {
      await __acquireForTesting(lp, tinyBudget);
    } catch (e) {
      err = e as Error;
    }
    assert.ok(err, 'expected acquire to throw against a live holder');
    assert.match(err!.message, /held by live process/);
    assert.match(err!.message, /could not acquire lock/);
    assert.match(err!.message, new RegExp(lp.replace(/[.\\/]/g, '\\$&')));

    await holder;
    assert.equal(holderReleased, true);
  });

});
