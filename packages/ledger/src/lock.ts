import { getAdapter } from './adapters/factory.js';
import { __acquireFileLockForTesting } from './adapters/file-lock.js';
import type { AcquireOptions } from './adapters/file-lock.js';

export {
  FAST_RETRIES,
  FAST_RETRY_DELAY_MS,
  SLOW_RETRIES,
  SLOW_RETRY_DELAY_MS,
  STALE_MS,
} from './adapters/file-lock.js';
export type { AcquireOptions } from './adapters/file-lock.js';

export async function withLock<T>(name: string, fn: () => Promise<T>): Promise<T> {
  return getAdapter().withLock(name, fn);
}

// Exported for tests that need to drive the timeout path with a tiny budget
// without holding a real lock for ~11 seconds.
export async function __acquireForTesting(
  lp: string,
  options: AcquireOptions,
): Promise<void> {
  return __acquireFileLockForTesting(lp, options);
}
