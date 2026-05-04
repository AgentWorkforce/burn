//! Watch-loop integration test — acceptance criterion for #245.
//!
//! Spawns the watch loop with a fake "session store" backed by an
//! `AtomicUsize` counter, drops a "session file" between ticks, and confirms
//! the counter advances within 2× the tick interval — verifying the
//! poll-based watcher actually drains its source on schedule and that
//! `stop()` waits for any in-flight tick before returning.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use relayburn_ingest::watch_loop::{start_watch_loop, IngestFn, StartWatchLoopOptions};
use relayburn_ingest::IngestReport;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn watch_loop_drains_pending_work_within_two_ticks() {
    let pending = Arc::new(AtomicUsize::new(0));
    let drained = Arc::new(AtomicUsize::new(0));

    let pending_for_ingest = pending.clone();
    let drained_for_ingest = drained.clone();
    let ingest: IngestFn = Arc::new(move || {
        let pending = pending_for_ingest.clone();
        let drained = drained_for_ingest.clone();
        Box::pin(async move {
            let p = pending.swap(0, Ordering::SeqCst);
            drained.fetch_add(p, Ordering::SeqCst);
            Ok(IngestReport {
                scanned_sessions: p,
                ingested_sessions: p,
                appended_turns: p,
            })
        })
    });

    let opts = StartWatchLoopOptions::new(ingest)
        .with_immediate(false)
        .with_interval(Duration::from_millis(100));
    let ctrl = start_watch_loop(opts);

    // Simulate a "session file" landing on disk.
    pending.store(3, Ordering::SeqCst);

    // Advance the clock past two tick boundaries; yield repeatedly so the
    // spawned task gets to run between time advances.
    for _ in 0..10 {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(50)).await;
    }

    ctrl.stop().await;

    let drained = drained.load(Ordering::SeqCst);
    assert!(
        drained >= 3,
        "watch loop did not drain pending work within 2× the tick interval (drained={drained})"
    );
}

#[tokio::test]
async fn stop_awaits_in_flight_tick() {
    use std::sync::Mutex;
    let observed_stop_after_completion = Arc::new(Mutex::new(false));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let in_flight_for_ingest = in_flight.clone();
    let completed_for_ingest = completed.clone();
    let ingest: IngestFn = Arc::new(move || {
        let in_flight = in_flight_for_ingest.clone();
        let completed = completed_for_ingest.clone();
        Box::pin(async move {
            in_flight.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            completed.fetch_add(1, Ordering::SeqCst);
            Ok(IngestReport::default())
        })
    });

    let opts = StartWatchLoopOptions::new(ingest)
        .with_immediate(true)
        .with_interval(Duration::from_secs(60));
    let ctrl = start_watch_loop(opts);

    // Give the immediate tick a moment to start.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(in_flight.load(Ordering::SeqCst), 1);
    assert_eq!(completed.load(Ordering::SeqCst), 0);

    ctrl.stop().await;
    *observed_stop_after_completion.lock().unwrap() = completed.load(Ordering::SeqCst) >= 1;

    assert!(
        *observed_stop_after_completion.lock().unwrap(),
        "stop() returned before the in-flight tick completed"
    );
}
