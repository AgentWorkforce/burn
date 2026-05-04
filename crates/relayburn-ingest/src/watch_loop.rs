//! Watch loop — Rust port of `packages/ingest/src/watch-loop.ts`.
//!
//! Drives a periodic `ingest` callable, drains the report through an
//! optional `on_report` sink, and routes errors through `on_error`. The TS
//! adapter uses `setInterval` + a `running` guard to prevent overlapping
//! ticks; the Rust port uses `tokio::time::interval` and a `Mutex` over an
//! in-flight future, with the same skip-if-running invariant.
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
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::ingest::IngestReport;

pub type IngestFn = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<IngestReport>> + Send>> + Send + Sync,
>;

pub type ReportSink = Arc<dyn Fn(&IngestReport) + Send + Sync>;
pub type ErrorSink = Arc<dyn Fn(&anyhow::Error) + Send + Sync>;

#[derive(Clone)]
pub struct StartWatchLoopOptions {
    pub interval: Duration,
    pub immediate: bool,
    pub ingest: IngestFn,
    pub on_report: Option<ReportSink>,
    pub on_error: Option<ErrorSink>,
}

impl StartWatchLoopOptions {
    /// Build options around `ingest`. Defaults: 1000ms interval, immediate
    /// first tick, stderr error sink, no report sink. Mirrors the TS
    /// defaults so existing CLI wrappers keep their behavior on port.
    pub fn new(ingest: IngestFn) -> Self {
        Self {
            interval: Duration::from_millis(1000),
            immediate: true,
            ingest,
            on_report: None,
            on_error: None,
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
    async fn run_tick_skip_if_busy(self: &Arc<Self>) {
        let Ok(_guard) = self.in_flight.try_lock() else {
            return;
        };
        self.run_locked().await;
    }

    /// Run-or-join variant used by the public `tick()`: if a tick is
    /// already running, await its completion via `tick_done` instead of
    /// silently skipping. Mirrors the TS `if (running) return running;`
    /// branch where overlapping callers share the in-flight promise — a
    /// `tick().await` is a real completion barrier rather than a no-op.
    async fn run_tick_or_join(self: &Arc<Self>) {
        // Register interest BEFORE the try_lock peek. `Notified::enable`
        // (added in tokio 1.13) latches the waker on creation so a runner
        // that finishes between our peek and the await still wakes us;
        // without it, a fast in-flight tick could complete and notify
        // before we register, and we'd block until the next `tick_done`.
        let notified = self.tick_done.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if let Ok(_guard) = self.in_flight.try_lock() {
            self.run_locked().await;
            self.tick_done.notify_waiters();
        } else {
            notified.await;
        }
    }

    async fn run_locked(self: &Arc<Self>) {
        let result = (self.ingest)().await;
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
    }
}

impl WatchController {
    /// Run a single tick on demand. If a tick is already in flight, await
    /// it and return when it completes — `tick().await` is a true
    /// completion barrier, matching the TS adapter's shared-promise shape.
    pub async fn tick(&self) {
        self.inner.run_tick_or_join().await;
    }

    /// Stop the periodic task and await any in-flight tick. Idempotent.
    /// We do NOT `abort()` the spawned task — that would cut a tick off
    /// mid-write. The stop is two-phased: set the atomic flag (covers a
    /// notify lost between loop iterations), then notify the parked waiter.
    pub async fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.stop_signal.notify_waiters();
        if let Some(handle) = self.handle.lock().await.take() {
            let _ = handle.await;
        }
        // Belt-and-braces: even if the handle was already taken (idempotent
        // calls), make sure no tick is mid-flight before returning.
        let _ = self.inner.in_flight.lock().await;
    }
}

/// Run a single ingest pass directly. Mirrors TS `runIngestTick(opts)` —
/// callers that want a one-shot sweep instead of a long-running loop use
/// this without going through `start_watch_loop`.
pub async fn run_ingest_tick<F, Fut>(ingest: F) -> anyhow::Result<IngestReport>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<IngestReport>>,
{
    ingest().await
}

/// Spawn a background ticker that calls `opts.ingest` every `opts.interval`,
/// skipping ticks while one is in flight. Returns a [`WatchController`] the
/// caller uses to invoke an extra tick on demand or stop the loop.
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
    let ticker = inner.clone();
    let handle = tokio::spawn(async move {
        if immediate {
            ticker.run_tick_skip_if_busy().await;
        }
        let mut iv = tokio::time::interval(interval);
        // Default `MissedTickBehavior::Burst` would fire catch-up ticks
        // back-to-back after a slow ingest pass, which can spike CPU/IO
        // exactly when the system is already under load. `Delay` schedules
        // the next tick `interval` after the previous fires, preserving
        // stable polling cadence — closer to TS `setInterval` pacing under
        // a single-threaded runner.
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick of `tokio::time::interval` fires immediately; skip it
        // because we already ran one above (or because the caller asked us
        // not to run an immediate tick at all).
        iv.tick().await;
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
            ticker.run_tick_skip_if_busy().await;
        }
    });
    WatchController {
        inner,
        handle: Mutex::new(Some(handle)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ingest_counting(counter: Arc<AtomicUsize>) -> IngestFn {
        Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(IngestReport::default())
            })
        })
    }

    #[tokio::test]
    async fn run_ingest_tick_invokes_callable_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        let report = run_ingest_tick(|| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(IngestReport::default())
            }
        })
        .await
        .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(report.scanned_sessions, 0);
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
        let ingest: IngestFn = Arc::new(move || {
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
        let ingest: IngestFn = Arc::new(|| Box::pin(async move { Err(anyhow::anyhow!("boom")) }));
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
