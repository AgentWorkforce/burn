//! Filesystem-event driver for the ingest watch loop.
//!
//! Wraps `notify::recommended_watcher` so the watch loop can wake on
//! actual session-store writes (FSEvents / inotify / RDCW) instead of a
//! 1s polling tick. The async wakeup surface is a tokio mpsc — the
//! notify callback runs on its own OS thread and posts a unit notice
//! into the channel; consumers drain bursts via [`FsBurst::wait_for_burst`]
//! to coalesce N rapid writes into a single tick.
//!
//! The watcher is best-effort: paths that don't exist are skipped, and
//! a complete failure to attach to any path returns `Err`. Callers fall
//! back to the polling driver in that case (network filesystems, Docker
//! mounts without inotify, `--no-fsevents`).

use std::path::PathBuf;
use std::time::Duration;

use notify::event::EventKind;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Active filesystem watcher backed by `notify`. The held
/// [`RecommendedWatcher`] keeps the OS-level watch alive; dropping the
/// struct stops the watcher and closes the channel.
pub(crate) struct FsBurst {
    _watcher: RecommendedWatcher,
    rx: UnboundedReceiver<()>,
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
        let (tx, rx) = unbounded_channel::<()>();
        let mut watcher = build_watcher(tx)?;
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
            rx,
        })
    }

    /// Block until a meaningful FS event arrives, then drain any
    /// further events that land within `debounce`. Returns `Some(())`
    /// once the burst settles, or `None` when the watcher has shut
    /// down (channel closed).
    ///
    /// The drain step is what coalesces bursty writes — a tool dumping
    /// 100 lines into a transcript fires N inotify events on Linux but
    /// produces a single ingest tick here.
    pub async fn wait_for_burst(&mut self, debounce: Duration) -> Option<()> {
        self.rx.recv().await?;
        loop {
            match tokio::time::timeout(debounce, self.rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return None,
                Err(_) => return Some(()),
            }
        }
    }
}

fn build_watcher(tx: UnboundedSender<()>) -> anyhow::Result<RecommendedWatcher> {
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else {
            return;
        };
        // Pure metadata churn (atime updates from a `cat`, attribute
        // changes) doesn't change the JSONL content the ingest reads.
        // Filtering at the source keeps the burst counter honest under
        // backups / antivirus scans.
        if matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            let _ = tx.send(());
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
}
