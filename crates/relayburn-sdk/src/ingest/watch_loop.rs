//! Watch loop — Rust port of `packages/ingest/src/watch-loop.ts`,
//! upgraded with a `notify`-backed FS-event driver per #250.
//!
//! Drives a periodic `ingest` callable, drains the report through an
//! optional `on_report` sink, and routes errors through `on_error`. Two
//! drivers wake the loop:
//!
//! * **FS events** (preferred): `notify::recommended_watcher` watches the
//!   session-store roots passed in [`StartWatchLoopOptions::watch_paths`]
//!   and wakes the loop on writes. Bursts are coalesced via the debounce
//!   window so 100 inotify events from a single tool dump produce one
//!   ingest tick, not 100. A slow polling backstop
//!   ([`StartWatchLoopOptions::slow_fallback_interval`], default 30s)
//!   covers the platforms where `notify` reports unsupported events
//!   silently (network filesystems, some Docker setups).
//! * **Pure polling** (fallback): when no `watch_paths` are supplied,
//!   when [`StartWatchLoopOptions::disable_fsevents`] is set, or when
//!   `notify` cannot attach to any path, the loop falls back to the
//!   original `tokio::time::interval` cadence at
//!   [`StartWatchLoopOptions::interval`].
//!
//! Concurrency model:
//!
//! * `tick()` acquires the in-flight slot. If a tick is already running, it
//!   no-ops (matching TS, which `return running` joins instead of skipping —
//!   the Rust port skips because tokio doesn't expose a free join handle for
//!   a single-shot future without re-architecting; in practice the skip is
//!   safe because the next interval tick will retry).
//! * `stop()` flips the stopped flag, aborts the periodic task, and awaits
//!   any in-flight tick so callers know all observable side effects have
//!   landed before they tear down state.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::ingest::fs_events::FsBurst;
use crate::ingest::ingest::IngestReport;

/// Ingest callable driven by the loop. The `bool` argument is `force`: the
/// FS-event driver passes `true` so the tick bypasses the no-op fast path
/// (an FS event can fire before the write flushes, leaving the stat-only
/// source fingerprint unchanged), while the polling backstop and the
/// on-demand [`WatchController::tick`] pass `false` and keep the fast path.
pub type IngestFn = Arc<
    dyn Fn(bool) -> Pin<Box<dyn Future<Output = anyhow::Result<IngestReport>> + Send>>
        + Send
        + Sync,
>;

pub type ReportSink = Arc<dyn Fn(&IngestReport) + Send + Sync>;
pub type ErrorSink = Arc<dyn Fn(&anyhow::Error) + Send + Sync>;

/// Default debounce window for the FS-event driver. 200ms is short
/// enough that an interactive Claude / Codex pause feels live and long
/// enough to coalesce the inotify burst from a single tool result
/// dumping a multi-line transcript update. Tuned alongside the burst
/// test in `watch_loop_tests::burst_writes_coalesce_into_one_tick`.
pub const DEFAULT_FS_DEBOUNCE: Duration = Duration::from_millis(200);

/// Default slow polling backstop when the FS-event driver is active.
/// Covers `notify` silently reporting "no events" on filesystems where
/// FSEvents / inotify are unreliable (network mounts, some Docker
/// setups). 30s matches the issue #250 acceptance shape.
pub const DEFAULT_SLOW_FALLBACK: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct StartWatchLoopOptions {
    pub interval: Duration,
    pub immediate: bool,
    pub ingest: IngestFn,
    pub on_report: Option<ReportSink>,
    pub on_error: Option<ErrorSink>,
    /// Session-store roots to monitor with `notify`. Empty disables
    /// the FS-event driver (the loop polls at `interval`).
    pub watch_paths: Vec<PathBuf>,
    /// Coalescing window for bursty FS events. After the first event
    /// wakes the loop, further events landing within this window roll
    /// into the same tick.
    pub debounce: Duration,
    /// Slow polling cadence used as a backstop *while the FS-event
    /// driver is active*. When the driver is inactive (no watch paths,
    /// `disable_fsevents`, or notify couldn't attach), the loop uses
    /// `interval` instead.
    pub slow_fallback_interval: Duration,
    /// Force the polling driver even when `watch_paths` is non-empty.
    /// Surfaced to the CLI as `burn ingest --watch --no-fsevents` so a
    /// user on a filesystem where `notify` misbehaves can opt out.
    pub disable_fsevents: bool,
}

impl StartWatchLoopOptions {
    /// Build options around `ingest`. Defaults: 1000ms interval, immediate
    /// first tick, stderr error sink, no report sink. Mirrors the TS
    /// defaults so existing CLI wrappers keep their behavior on port.
    /// Defaults: 1000ms polling fallback interval, immediate first tick,
    /// FS-event driver enabled (but inert until `with_watch_paths` is
    /// called), 30s slow polling backstop, 200ms burst debounce.
    /// Mirrors the TS defaults so existing CLI wrappers keep their
    /// behavior on port.
    pub fn new(ingest: IngestFn) -> Self {
        Self {
            interval: Duration::from_millis(1000),
            immediate: true,
            ingest,
            on_report: None,
            on_error: None,
            watch_paths: Vec::new(),
            debounce: DEFAULT_FS_DEBOUNCE,
            slow_fallback_interval: DEFAULT_SLOW_FALLBACK,
            disable_fsevents: false,
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    pub fn with_immediate(mut self, immediate: bool) -> Self {
        self.immediate = immediate;
        self
    }

    pub fn with_on_report(mut self, sink: ReportSink) -> Self {
        self.on_report = Some(sink);
        self
    }

    pub fn with_on_error(mut self, sink: ErrorSink) -> Self {
        self.on_error = Some(sink);
        self
    }

    /// Enable the FS-event driver against the given session-store
    /// roots. Pass the harness-default paths from
    /// [`crate::default_session_roots`] for the CLI wide-scan, or a
    /// single-harness root for adapter watchers.
    pub fn with_watch_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.watch_paths = paths;
        self
    }

    pub fn with_debounce(mut self, debounce: Duration) -> Self {
        self.debounce = debounce;
        self
    }

    pub fn with_slow_fallback_interval(mut self, interval: Duration) -> Self {
        self.slow_fallback_interval = interval;
        self
    }

    pub fn with_disable_fsevents(mut self, disable: bool) -> Self {
        self.disable_fsevents = disable;
        self
    }
}

/// Controller returned by [`start_watch_loop`]. Drop alone won't cancel the
/// loop — callers must call `stop().await` for graceful shutdown.
pub struct WatchController {
    inner: Arc<WatchInner>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

struct WatchInner {
    /// Atomic stop flag. Checked before each tick wait and before each
    /// run_tick — covers the window where `stop_signal.notify_waiters()`
    /// fires while no waiter is parked (between iterations of the loop).
    stopped: AtomicBool,
    in_flight: Mutex<()>,
    stop_signal: Notify,
    /// Notified when an in-flight tick finishes. Public `tick()` callers
    /// arriving while a tick is mid-flight register on this so their
    /// `await` is a real completion barrier (matching the TS adapter,
    /// where overlapping `tick()` calls share the in-flight promise).
    tick_done: Notify,
    ingest: IngestFn,
    on_report: Option<ReportSink>,
    on_error: Option<ErrorSink>,
}

impl WatchInner {
    /// Skip-if-busy variant used by the periodic loop: if a tick is already
    /// running, return immediately rather than queue. Queuing here would
    /// produce zero-gap back-to-back runs after a slow tick — exactly the
    /// CPU/IO spike the `MissedTickBehavior::Delay` setting also defends
    /// against — so the periodic driver is the wrong place to join.
    ///
    /// The runner — whichever path holds the `in_flight` lock — owns the
    /// `tick_done` notify so a public `tick().await` parked via
    /// `run_tick_or_join` wakes regardless of which path drove the run.
    async fn run_tick_skip_if_busy(self: &Arc<Self>, force: bool) {
        let Ok(_guard) = self.in_flight.try_lock() else {
            return;
        };
        self.run_locked(force).await;
    }

    /// Run-or-join variant used by the public `tick()`: if a tick is
    /// already running, await its completion via `tick_done` instead of
    /// silently skipping. Mirrors the TS `if (running) return running;`
    /// branch where overlapping callers share the in-flight promise — a
    /// `tick().await` is a real completion barrier rather than a no-op.
    async fn run_tick_or_join(self: &Arc<Self>, force: bool) {
        // Register interest BEFORE the try_lock peek. `Notified::enable`
        // (added in tokio 1.13) latches the waker on creation so a runner
        // that finishes between our peek and the await still wakes us;
        // without it, a fast in-flight tick could complete and notify
        // before we register, and we'd block until the next `tick_done`.
        let notified = self.tick_done.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if let Ok(_guard) = self.in_flight.try_lock() {
            self.run_locked(force).await;
        } else {
            notified.await;
        }
    }

    /// Drive one ingest pass under the `in_flight` lock and broadcast
    /// completion. Both `run_tick_skip_if_busy` (periodic loop) and
    /// `run_tick_or_join` (public `tick`) funnel through here so any
    /// caller parked on `tick_done` wakes when the run finishes —
    /// regardless of which path the runner came from.
    async fn run_locked(self: &Arc<Self>, force: bool) {
        let result = (self.ingest)(force).await;
        match result {
            Ok(report) => {
                if let Some(sink) = &self.on_report {
                    sink(&report);
                }
            }
            Err(err) => {
                if let Some(sink) = &self.on_error {
                    sink(&err);
                } else {
                    eprintln!("[burn] ingest: {err}");
                }
            }
        }
        self.tick_done.notify_waiters();
    }
}

impl WatchController {
    /// Run a single tick on demand. If a tick is already in flight, await
    /// it and return when it completes — `tick().await` is a true
    /// completion barrier, matching the TS adapter's shared-promise shape.
    pub async fn tick(&self) {
        // On-demand ticks keep the fast path: a manual `tick()` isn't tied to
        // an in-flight FS write, so the stat fingerprint is trustworthy.
        self.inner.run_tick_or_join(false).await;
    }

    /// Stop the periodic task and await any in-flight tick. Idempotent.
    /// We do NOT `abort()` the spawned task — that would cut a tick off
    /// mid-write. The stop is two-phased: set the atomic flag (covers a
    /// notify lost between loop iterations), then notify the parked waiter.
    /// The trailing `in_flight.lock().await` covers a concurrent `tick()`
    /// call from outside the loop: `tick()` doesn't check `stopped`, so
    /// it can still acquire `in_flight` and run an ingest after the loop
    /// task has exited. Waiting on the guard here guarantees no tick is
    /// mid-write when `stop` returns, so callers can tear down state
    /// safely.
    pub async fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.stop_signal.notify_waiters();
        if let Some(handle) = self.handle.lock().await.take() {
            let _ = handle.await;
        }
        let _ = self.inner.in_flight.lock().await;
    }
}

/// Spawn a background ticker that calls `opts.ingest` whenever the
/// active driver fires, skipping ticks while one is in flight. Returns
/// a [`WatchController`] the caller uses to invoke an extra tick on
/// demand or stop the loop.
///
/// Driver selection (see module docs for the full rationale):
///
/// * If `watch_paths` is non-empty, `disable_fsevents` is false, and
///   `notify` can attach to at least one path → FS-event driver with a
///   slow polling backstop at `slow_fallback_interval`.
/// * Otherwise → polling driver at `interval`, matching the legacy
///   1.x `setInterval` cadence.
pub fn start_watch_loop(opts: StartWatchLoopOptions) -> WatchController {
    let inner = Arc::new(WatchInner {
        stopped: AtomicBool::new(false),
        in_flight: Mutex::new(()),
        stop_signal: Notify::new(),
        tick_done: Notify::new(),
        ingest: opts.ingest,
        on_report: opts.on_report,
        on_error: opts.on_error,
    });
    let interval = opts.interval;
    let immediate = opts.immediate;
    let watch_paths = opts.watch_paths;
    let debounce = opts.debounce;
    let slow_fallback = opts.slow_fallback_interval;
    let disable_fsevents = opts.disable_fsevents;
    let ticker = inner.clone();
    let handle = tokio::spawn(async move {
        // Try to bring up the FS-event driver. Failure (no path exists,
        // notify backend errors) silently demotes us to the polling
        // driver — that's the slow-fallback acceptance criterion from
        // #250.
        let burst = if !disable_fsevents && !watch_paths.is_empty() {
            FsBurst::new(&watch_paths).ok()
        } else {
            None
        };

        if immediate {
            // Startup sweep: a fresh ledger has a blank fingerprint, so this
            // always runs a full scan anyway; no need to force.
            ticker.run_tick_skip_if_busy(false).await;
        }

        match burst {
            Some(mut burst) => {
                run_fs_event_driver(&ticker, &mut burst, debounce, slow_fallback).await;
            }
            None => {
                run_polling_driver(&ticker, interval).await;
            }
        }
    });
    WatchController {
        inner,
        handle: Mutex::new(Some(handle)),
    }
}

/// Pure-polling driver — matches the pre-#250 behaviour exactly. Used
/// when no `watch_paths` are configured, when `disable_fsevents` is
/// set, or when `FsBurst` couldn't attach (network mount, etc.).
async fn run_polling_driver(ticker: &Arc<WatchInner>, interval: Duration) {
    // Schedule the periodic ticker to first fire `interval` from now.
    // `tokio::time::interval` fires immediately on the first `tick()`,
    // so for the immediate path we'd want to skip that first tick;
    // for the non-immediate path we'd want to wait `interval` before
    // the first periodic run. `interval_at(now + interval, …)` covers
    // both: the next tick lands `interval` after start in either case.
    let start_at = tokio::time::Instant::now() + interval;
    let mut iv = tokio::time::interval_at(start_at, interval);
    // Default `MissedTickBehavior::Burst` would fire catch-up ticks
    // back-to-back after a slow ingest pass, which can spike CPU/IO
    // exactly when the system is already under load. `Delay` schedules
    // the next tick `interval` after the previous fires, preserving
    // stable polling cadence — closer to TS `setInterval` pacing under
    // a single-threaded runner.
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if ticker.stopped.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = iv.tick() => {}
            _ = ticker.stop_signal.notified() => break,
        }
        if ticker.stopped.load(Ordering::SeqCst) {
            break;
        }
        // Pure-polling driver: no FS-event signal, so the stat fingerprint is
        // the only change source — keep the fast path (force = false).
        ticker.run_tick_skip_if_busy(false).await;
    }
}

/// FS-event driver — sleep until either the burst receiver fires (and
/// the burst window settles) or the slow polling backstop ticks.
async fn run_fs_event_driver(
    ticker: &Arc<WatchInner>,
    burst: &mut FsBurst,
    debounce: Duration,
    slow_fallback: Duration,
) {
    let start_at = tokio::time::Instant::now() + slow_fallback;
    let mut slow = tokio::time::interval_at(start_at, slow_fallback);
    slow.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if ticker.stopped.load(Ordering::SeqCst) {
            break;
        }
        // `force` is true only when an FS event woke us: a `notify` event can
        // arrive before the write is flushed, so the stat-only source
        // fingerprint may still match the stored value. Forcing the sweep on an
        // event makes the per-file cursors the source of truth for that tick.
        // The slow backstop leaves `force` false so a quiet period still pays
        // only the no-op fast path.
        let force = tokio::select! {
            // `wait_for_burst` always resolves to `Some(())` once its
            // debounce window elapses; under sustained writes that's
            // every ~debounce, under bursts it coalesces. We don't
            // pattern-match the Option because the Notify-backed
            // channel can't close without dropping `_watcher`, which
            // would have already torn down this task.
            _ = burst.wait_for_burst(debounce) => true,
            _ = slow.tick() => false,
            _ = ticker.stop_signal.notified() => break,
        };
        if ticker.stopped.load(Ordering::SeqCst) {
            break;
        }
        ticker.run_tick_skip_if_busy(force).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ingest_counting(counter: Arc<AtomicUsize>) -> IngestFn {
        Arc::new(move |_force: bool| {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(IngestReport::default())
            })
        })
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watch_loop_runs_immediate_then_periodic() {
        let counter = Arc::new(AtomicUsize::new(0));
        let opts = StartWatchLoopOptions::new(ingest_counting(counter.clone()))
            .with_interval(Duration::from_millis(100));
        let ctrl = start_watch_loop(opts);

        // Allow the spawned task to advance: yield, advance time, repeat.
        for _ in 0..5 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        ctrl.stop().await;

        let runs = counter.load(Ordering::SeqCst);
        // Immediate tick + ≥1 periodic tick.
        assert!(runs >= 2, "expected ≥2 runs, got {runs}");
    }

    /// Non-immediate watch loop must fire its first periodic tick after
    /// `interval`, not `2 * interval`. Regression test for an earlier
    /// shape that called `iv.tick().await` to skip a tick that the
    /// immediate branch hadn't actually produced.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn non_immediate_first_tick_lands_at_interval() {
        let counter = Arc::new(AtomicUsize::new(0));
        let opts = StartWatchLoopOptions::new(ingest_counting(counter.clone()))
            .with_immediate(false)
            .with_interval(Duration::from_millis(100));
        let ctrl = start_watch_loop(opts);

        // Let the spawned task park on its sleep, then advance time
        // *just past* one interval. After this much paused time the
        // loop should have fired exactly once.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(101)).await;
        // Yield again so the spawned task gets a chance to run the tick
        // body before we read the counter.
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        let runs_after_one_interval = counter.load(Ordering::SeqCst);

        ctrl.stop().await;

        assert_eq!(
            runs_after_one_interval, 1,
            "expected exactly 1 run after ~1 interval (was the loop firing at 2*interval?), got {runs_after_one_interval}"
        );
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let counter = Arc::new(AtomicUsize::new(0));
        let opts = StartWatchLoopOptions::new(ingest_counting(counter.clone()))
            .with_immediate(false)
            .with_interval(Duration::from_secs(60));
        let ctrl = start_watch_loop(opts);
        ctrl.stop().await;
        ctrl.stop().await; // must not panic
    }

    #[tokio::test]
    async fn manual_tick_runs_callable() {
        let counter = Arc::new(AtomicUsize::new(0));
        let opts = StartWatchLoopOptions::new(ingest_counting(counter.clone()))
            .with_immediate(false)
            .with_interval(Duration::from_secs(60));
        let ctrl = start_watch_loop(opts);
        ctrl.tick().await;
        ctrl.tick().await;
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        ctrl.stop().await;
    }

    /// Concurrent `tick()` calls must share the in-flight pass: a caller
    /// that arrives while a tick is running awaits its completion rather
    /// than no-opping. Without this barrier, code that pumps `tick()` in
    /// response to "new work" can race the runner and proceed before the
    /// new work was actually scanned.
    #[tokio::test]
    async fn concurrent_ticks_join_in_flight_run() {
        let in_flight_count = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));

        let in_flight_for_ingest = in_flight_count.clone();
        let max_for_ingest = max_concurrent.clone();
        let completed_for_ingest = completed.clone();
        let ingest: IngestFn = Arc::new(move |_force: bool| {
            let in_flight = in_flight_for_ingest.clone();
            let max = max_for_ingest.clone();
            let completed = completed_for_ingest.clone();
            Box::pin(async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                let mut prev = max.load(Ordering::SeqCst);
                while now > prev {
                    match max.compare_exchange(prev, now, Ordering::SeqCst, Ordering::SeqCst) {
                        Ok(_) => break,
                        Err(actual) => prev = actual,
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(IngestReport::default())
            })
        });

        let opts = StartWatchLoopOptions::new(ingest)
            .with_immediate(false)
            .with_interval(Duration::from_secs(60));
        let ctrl = Arc::new(start_watch_loop(opts));

        // Fire three ticks concurrently. With the join semantics, only one
        // ingest body runs; the other two callers await the same in-flight
        // run and observe its completion before returning.
        let c1 = ctrl.clone();
        let c2 = ctrl.clone();
        let c3 = ctrl.clone();
        let h1 = tokio::spawn(async move { c1.tick().await });
        let h2 = tokio::spawn(async move { c2.tick().await });
        let h3 = tokio::spawn(async move { c3.tick().await });
        let _ = tokio::join!(h1, h2, h3);

        ctrl.stop().await;

        // The runner ran exactly one ingest body — overlapping callers
        // joined rather than queuing.
        assert_eq!(
            completed.load(Ordering::SeqCst),
            1,
            "concurrent tick() calls should share one in-flight run"
        );
        // And no two ingest bodies ever ran simultaneously.
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn errors_route_to_on_error_sink() {
        use std::sync::Mutex as StdMutex;
        let captured: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ingest: IngestFn =
            Arc::new(|_force: bool| Box::pin(async move { Err(anyhow::anyhow!("boom")) }));
        let on_error: ErrorSink = Arc::new(move |err| {
            captured_clone.lock().unwrap().push(err.to_string());
        });
        let opts = StartWatchLoopOptions::new(ingest)
            .with_immediate(false)
            .with_interval(Duration::from_secs(60))
            .with_on_error(on_error);
        let ctrl = start_watch_loop(opts);
        ctrl.tick().await;
        ctrl.stop().await;

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(captured[0].contains("boom"));
    }
}
