//! Cross-process file lock with the same semantics as
//! `packages/ledger/src/adapters/file-lock.ts`.
//!
//! Why not `flock(2)` / `fs2::FileExt::lock_exclusive`? cross-process
//! semantics differ on macOS / Windows; the TS adapter relies on
//! exclusive-create-then-stat instead, so the Rust port must follow.
//!
//! Acquire protocol (must stay byte-compatible with TS so a Rust holder and
//! a TS holder block each other):
//!   1. `OpenOptions::write(true).create_new(true).open(lockfile)` — succeed
//!      on first creator.
//!   2. On `AlreadyExists`, stat the lockfile. If `mtime + STALE_MS < now`
//!      try to `unlink` it (orphan recovery) and immediately retry the open.
//!   3. Otherwise wait `FAST_RETRY_DELAY_MS` for the first `FAST_RETRIES`
//!      attempts, then `SLOW_RETRY_DELAY_MS` for the next `SLOW_RETRIES`.
//!   4. After `FAST_RETRIES + SLOW_RETRIES` failed attempts, return
//!      `LockTimeout` naming whether the holder was live or the unlink kept
//!      failing.
//!
//! Re-entrancy: `with_lock(name, fn)` is no-op-recursive when the same
//! task already holds `name`. We track the held set in a tokio task-local —
//! the equivalent of TS `AsyncLocalStorage`.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::fs;
use tokio::time::sleep;

use crate::errors::{LedgerError, Result};
use crate::paths::lock_path;

pub const FAST_RETRY_DELAY_MS: u64 = 20;
pub const FAST_RETRIES: u32 = 50; // 1s: normal in-process contention
pub const SLOW_RETRY_DELAY_MS: u64 = 250;
pub const SLOW_RETRIES: u32 = 40; // 10s: covers an orphan twice over
pub const STALE_MS: u64 = 5_000;

// Compile-time invariant: the retry budget must outlast STALE_MS so a single
// invocation can wait an orphan out and unlink it. Mirrors the runtime
// assertion in the TS adapter.
const _: () = {
    let total = FAST_RETRY_DELAY_MS as u128 * FAST_RETRIES as u128
        + SLOW_RETRY_DELAY_MS as u128 * SLOW_RETRIES as u128;
    assert!(total > STALE_MS as u128 + SLOW_RETRY_DELAY_MS as u128);
};

#[derive(Debug, Clone, Copy)]
pub struct AcquireOptions {
    pub fast_retries: u32,
    pub fast_retry_delay_ms: u64,
    pub slow_retries: u32,
    pub slow_retry_delay_ms: u64,
    pub stale_ms: u64,
}

impl Default for AcquireOptions {
    fn default() -> Self {
        Self {
            fast_retries: FAST_RETRIES,
            fast_retry_delay_ms: FAST_RETRY_DELAY_MS,
            slow_retries: SLOW_RETRIES,
            slow_retry_delay_ms: SLOW_RETRY_DELAY_MS,
            stale_ms: STALE_MS,
        }
    }
}

tokio::task_local! {
    static HELD_LOCKS: std::cell::RefCell<HashSet<PathBuf>>;
}

/// Run `fut` while holding the named lock. Re-entrant: if the calling task
/// already holds `name`, the inner future runs without re-acquiring (matches
/// TS `AsyncLocalStorage` re-entrancy in `FileLockManager`).
pub async fn with_lock<F, Fut, T>(name: &str, fut: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let lp = lock_path(name);
    if currently_held(&lp) {
        return fut().await;
    }

    if let Some(parent) = lp.parent() {
        fs::create_dir_all(parent).await?;
    }
    acquire(&lp, AcquireOptions::default()).await?;

    let result = run_with_held(lp.clone(), fut()).await;

    // Release: best-effort unlink. A failure here just means the next
    // acquirer will see it as stale and recover.
    let _ = fs::remove_file(&lp).await;

    result
}

/// Whether the currently-running task already holds `lp`. Returns false when
/// the task-local hasn't been initialized (i.e. nobody has called
/// `with_lock` yet on this task).
fn currently_held(lp: &Path) -> bool {
    HELD_LOCKS
        .try_with(|cell| cell.borrow().contains(lp))
        .unwrap_or(false)
}

async fn run_with_held<Fut, T>(lp: PathBuf, fut: Fut) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    // Two paths: either the task-local is already initialized (nested
    // with_lock under a different name) or it's not (top-level call). Either
    // way the inner future needs to see `lp` as held.
    if HELD_LOCKS.try_with(|_| ()).is_ok() {
        HELD_LOCKS.with(|cell| cell.borrow_mut().insert(lp.clone()));
        let result = fut.await;
        HELD_LOCKS.with(|cell| {
            cell.borrow_mut().remove(&lp);
        });
        result
    } else {
        let mut set = HashSet::new();
        set.insert(lp);
        HELD_LOCKS.scope(std::cell::RefCell::new(set), fut).await
    }
}

/// Public acquire entry point used by tests that need to drive the timeout
/// path with a tiny budget rather than waiting ~11s for the real lock.
pub async fn acquire_for_testing(lp: &Path, options: AcquireOptions) -> Result<()> {
    if let Some(parent) = lp.parent() {
        fs::create_dir_all(parent).await?;
    }
    acquire(lp, options).await
}

async fn acquire(lp: &Path, options: AcquireOptions) -> Result<()> {
    let mut last_reason: &'static str = "held by live process";

    let total_retries = options.fast_retries + options.slow_retries;
    let budget_ms = options.fast_retries as u64 * options.fast_retry_delay_ms
        + options.slow_retries as u64 * options.slow_retry_delay_ms;

    for attempt in 0..total_retries {
        match try_create(lp).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(age_ms) = lockfile_age_ms(lp).await {
                    if age_ms > options.stale_ms {
                        match fs::remove_file(lp).await {
                            Ok(()) => continue, // race the open again immediately
                            Err(unlink_err)
                                if unlink_err.kind() == std::io::ErrorKind::NotFound =>
                            {
                                continue;
                            }
                            Err(_) => {
                                last_reason = "lock appears stale but unlink kept failing";
                            }
                        }
                    } else {
                        last_reason = "held by live process";
                    }
                } else {
                    last_reason = "held by live process";
                }
                let delay_ms = if attempt < options.fast_retries {
                    options.fast_retry_delay_ms
                } else {
                    options.slow_retry_delay_ms
                };
                sleep(Duration::from_millis(delay_ms)).await;
            }
            Err(e) => return Err(LedgerError::Io(e)),
        }
    }

    Err(LedgerError::LockTimeout {
        attempts: total_retries,
        budget_ms,
        detail: last_reason,
        path: lp.display().to_string(),
    })
}

async fn try_create(lp: &Path) -> std::io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lp)
        .await
        .map(|_| ())
}

async fn lockfile_age_ms(lp: &Path) -> Option<u64> {
    let meta = fs::metadata(lp).await.ok()?;
    let mtime = meta.modified().ok()?;
    let now = SystemTime::now();
    now.duration_since(mtime).ok().map(|d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tempfile::tempdir;

    use crate::test_support::set_home;

    #[tokio::test]
    async fn serializes_concurrent_callers() {
        // 100 concurrent callers; observe that they fully serialize. We
        // increment-then-read-back a shared counter inside the critical
        // section and assert no two ever observe the same value (a strong
        // signal that the lock granted them in sequence).
        let dir = tempdir().unwrap();
        let _g = set_home(dir.path()).await;

        let counter = Arc::new(AtomicU32::new(0));
        let observed_max = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::new();
        for _ in 0..100 {
            let counter = counter.clone();
            let observed_max = observed_max.clone();
            handles.push(tokio::spawn(async move {
                with_lock("serialize-test", || async {
                    let next = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    // Yield mid-section to give a competing task a chance to
                    // race; if locking is broken `observed_max` will lag the
                    // counter by 1 because two tasks were inside at once.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    let prev_max = observed_max.fetch_max(next, Ordering::SeqCst);
                    let after_release = counter.load(Ordering::SeqCst);
                    assert_eq!(
                        next, after_release,
                        "another task entered the critical section: prev_max={prev_max}"
                    );
                    Ok(())
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 100);
    }

    #[tokio::test]
    async fn recovers_from_orphan_lockfile() {
        let dir = tempdir().unwrap();
        let _g = set_home(dir.path()).await;

        let lp = lock_path("orphan-test");
        std::fs::create_dir_all(lp.parent().unwrap()).unwrap();

        // Simulate a stale orphan: create the lockfile, then sleep so its
        // mtime ages past `stale_ms`. Real production uses STALE_MS=5s; we
        // run this test with a tiny stale_ms so the sleep stays small.
        std::fs::File::create(&lp).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let opts = AcquireOptions {
            stale_ms: 5,
            ..AcquireOptions::default()
        };
        acquire_for_testing(&lp, opts).await.unwrap();
        let _ = std::fs::remove_file(&lp);
    }

    #[tokio::test]
    async fn timeout_when_lock_is_held_indefinitely() {
        let dir = tempdir().unwrap();
        let _g = set_home(dir.path()).await;
        let lp = lock_path("never-released");
        std::fs::create_dir_all(lp.parent().unwrap()).unwrap();
        std::fs::File::create(&lp).unwrap();

        let opts = AcquireOptions {
            fast_retries: 2,
            fast_retry_delay_ms: 1,
            slow_retries: 2,
            slow_retry_delay_ms: 1,
            stale_ms: 60_000, // not stale within the budget
        };
        let err = acquire_for_testing(&lp, opts).await.unwrap_err();
        match err {
            LedgerError::LockTimeout { detail, .. } => {
                assert_eq!(detail, "held by live process");
            }
            other => panic!("expected LockTimeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reentrant_with_same_name() {
        // Re-entering the same lock name must NOT block; the inner block
        // should run inline. If re-entrancy were broken this test would
        // deadlock until the outer acquire timed out.
        let dir = tempdir().unwrap();
        let _g = set_home(dir.path()).await;

        with_lock("reentrant", || async {
            with_lock("reentrant", || async { Ok(()) }).await
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn distinct_names_do_not_block_each_other() {
        let dir = tempdir().unwrap();
        let _g = set_home(dir.path()).await;

        let h1 = tokio::spawn(async {
            with_lock("name-a", || async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(())
            })
            .await
        });
        let h2 = tokio::spawn(async {
            with_lock("name-b", || async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(())
            })
            .await
        });
        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();
    }
}
