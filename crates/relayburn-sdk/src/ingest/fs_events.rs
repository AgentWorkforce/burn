//! Filesystem-event driver for the ingest watch loop.
//!
//! Wraps `notify::recommended_watcher` so the watch loop can wake on
//! actual session-store writes (FSEvents / inotify / RDCW) instead of a
//! 1s polling tick. The async wakeup surface is a single-permit
//! [`tokio::sync::Notify`]: the notify callback runs on its own OS
//! thread and calls `notify_one` per relevant event, which collapses
//! into a single pending permit regardless of event rate. That keeps
//! memory bounded under noisy roots — N events allocate O(1), not
//! O(N).
//!
//! [`FsBurst::wait_for_burst`] consumes that permit, then sleeps for
//! `debounce` to coalesce further events landing inside the window.
//! Crucially the burst future *always* returns after at most one
//! debounce window: under sustained writes the loop emits a steady
//! ~`debounce` cadence rather than waiting for a quiet period that
//! never arrives. The slow polling backstop in the watch loop only
//! kicks in when the FS-event channel goes silent.
//!
//! The watcher is best-effort: paths that don't exist are skipped, and
//! a complete failure to attach to any path returns `Err`. Callers fall
//! back to the polling driver in that case (network filesystems, Docker
//! mounts without inotify, `--no-fsevents`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::event::EventKind;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::Notify;

/// Active filesystem watcher backed by `notify`. The held
/// [`RecommendedWatcher`] keeps the OS-level watch alive; dropping the
/// struct stops the watcher. The notify callback retains an
/// `Arc<Notify>`, so events posted while no consumer is awaiting still
/// store a single pending permit that the next `wait_for_burst` call
/// observes.
pub(crate) struct FsBurst {
    _watcher: RecommendedWatcher,
    pending: Arc<Notify>,
}

impl FsBurst {
    /// Attach to every existing path in `paths` and return a burst
    /// receiver. Returns `Err` when no path could be watched — callers
    /// should fall back to the polling driver in that case.
    ///
    /// `Recursive` mode is required: ingest cares about new files
    /// landing inside `~/.claude/projects/<project>/` etc., not about
    /// the project root itself.
    pub fn new(paths: &[PathBuf]) -> anyhow::Result<Self> {
        let pending = Arc::new(Notify::new());
        let mut watcher = build_watcher(pending.clone())?;
        let mut watched_any = false;
        for p in paths {
            if !p.exists() {
                continue;
            }
            if watcher.watch(p, RecursiveMode::Recursive).is_ok() {
                watched_any = true;
            }
        }
        if !watched_any {
            anyhow::bail!("no watchable session-store paths");
        }
        Ok(Self {
            _watcher: watcher,
            pending,
        })
    }

    /// Wait for the next FS event, then sleep `debounce` to coalesce
    /// further events that land inside the window. Always returns
    /// `Some(())` once the window elapses.
    ///
    /// Cadence: under bursty writes, N events fired during the debounce
    /// window collapse into a single tick (the goal). Under *sustained*
    /// writes, this returns once per `debounce` because we deliberately
    /// don't extend the window past its first interval — extending
    /// would let a continuous write stream starve the ingest loop and
    /// demote it to the 30s slow polling backstop.
    ///
    /// The notify slot is single-bit, so memory is O(1) regardless of
    /// event rate. Lost cancellation is also fine: if the future is
    /// dropped between the wake and the sleep, any subsequent event
    /// re-stores the permit and the next call observes it.
    pub async fn wait_for_burst(&mut self, debounce: Duration) -> Option<()> {
        // `Notified::enable` (tokio 1.13+) latches the waker on
        // creation so a permit posted between this future being
        // constructed and being polled isn't lost. Without enable, a
        // notify_one between the previous return and this re-park
        // could fall on the floor.
        let notified = self.pending.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        notified.await;
        // Coalescing window. Events landing inside this sleep set the
        // pending permit (single-bit) and the next call to
        // `wait_for_burst` observes it immediately — that's how
        // sustained writes get a steady ~debounce-cadence tick stream
        // instead of waiting for a quiet period.
        tokio::time::sleep(debounce).await;
        Some(())
    }
}

fn build_watcher(pending: Arc<Notify>) -> anyhow::Result<RecommendedWatcher> {
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else {
            return;
        };
        // Pure metadata churn (atime updates from a `cat`, attribute
        // changes) doesn't change the JSONL content the ingest reads.
        // Filtering at the source keeps wakeups honest under backups
        // / antivirus scans.
        if matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            // `notify_one` collapses multiple calls into a single
            // pending permit — bounded memory, bounded wakeups even
            // under noisy roots that fire thousands of events per
            // second.
            pending.notify_one();
        }
    })?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Best-effort smoke test: write a file inside a watched temp dir
    /// and confirm the burst receiver wakes.
    ///
    /// Marked `#[ignore]` because FS-event delivery latency varies by
    /// platform and CI sandbox; the test is informational under
    /// `cargo test -- --ignored`. The protocol-level guarantees the
    /// watch loop relies on are exercised by the polling-fallback path
    /// in `watch_loop_tests.rs`.
    #[tokio::test]
    #[ignore]
    async fn fs_burst_wakes_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut burst = FsBurst::new(&[dir.path().to_path_buf()]).unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            fs::write(&path, b"{}\n").unwrap();
        });
        let woke = tokio::time::timeout(
            Duration::from_secs(2),
            burst.wait_for_burst(Duration::from_millis(50)),
        )
        .await;
        writer.join().unwrap();
        assert!(matches!(woke, Ok(Some(()))));
    }

    /// `FsBurst::new` returns `Err` when none of the supplied paths
    /// exist. Watch loop relies on this to fall back to polling.
    #[tokio::test]
    async fn fs_burst_errors_when_no_paths_exist() {
        let result = FsBurst::new(&[PathBuf::from("/nonexistent/relayburn/test/path")]);
        assert!(result.is_err());
    }

    /// Sustained writes must produce a steady tick cadence rather
    /// than starve the loop. Verifies the fix for the Codex review
    /// comment on #410: an earlier "wait for quiet" implementation
    /// would never return under continuous events, demoting watch
    /// mode to the 30s slow fallback.
    ///
    /// Also marked `#[ignore]` because it depends on real FS-event
    /// delivery — same caveat as `fs_burst_wakes_on_write`.
    #[tokio::test]
    #[ignore]
    async fn fs_burst_emits_under_sustained_writes() {
        let dir = tempfile::tempdir().unwrap();
        let mut burst = FsBurst::new(&[dir.path().to_path_buf()]).unwrap();
        let dir_path = dir.path().to_path_buf();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_for_writer = stop.clone();
        let writer = std::thread::spawn(move || {
            let mut i = 0u64;
            while !stop_for_writer.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = fs::write(dir_path.join(format!("s-{i}.jsonl")), b"{}\n");
                i += 1;
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        // Three back-to-back wake cycles must each return inside
        // ~debounce + slack. If the burst future waited for a quiet
        // period, the second call would hang indefinitely.
        let debounce = Duration::from_millis(50);
        for _ in 0..3 {
            let result =
                tokio::time::timeout(Duration::from_millis(500), burst.wait_for_burst(debounce))
                    .await;
            assert!(matches!(result, Ok(Some(()))));
        }

        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        writer.join().unwrap();
    }
}
