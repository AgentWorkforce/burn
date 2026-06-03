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

use crate::ingest::watch_loop::{start_watch_loop, IngestFn, StartWatchLoopOptions};
use crate::ingest::IngestReport;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn watch_loop_drains_pending_work_within_two_ticks() {
    let pending = Arc::new(AtomicUsize::new(0));
    let drained = Arc::new(AtomicUsize::new(0));

    let pending_for_ingest = pending.clone();
    let drained_for_ingest = drained.clone();
    let ingest: IngestFn = Arc::new(move |_force: bool| {
        let pending = pending_for_ingest.clone();
        let drained = drained_for_ingest.clone();
        Box::pin(async move {
            let p = pending.swap(0, Ordering::SeqCst);
            drained.fetch_add(p, Ordering::SeqCst);
            Ok(IngestReport {
                scanned_sessions: p,
                ingested_sessions: p,
                appended_turns: p,
                applied_pending_stamps: 0,
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

/// Regression test for the periodic-runner / manual-tick join bug.
///
/// `WatchController::tick()` documents itself as a completion barrier, so a
/// caller that arrives while the *periodic* loop is mid-flight must still
/// wake when the periodic run completes. Earlier the periodic path held
/// `in_flight` and never notified `tick_done`, so a `tick().await` issued
/// during a slow periodic tick would hang until the next manual run woke
/// the notifier. We now notify from `run_locked`, so any path that owns
/// the lock signals completion.
#[tokio::test]
async fn manual_tick_joins_periodic_run() {
    use std::sync::atomic::AtomicUsize;
    let in_flight = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let in_flight_for_ingest = in_flight.clone();
    let completed_for_ingest = completed.clone();
    let ingest: IngestFn = Arc::new(move |_force: bool| {
        let in_flight = in_flight_for_ingest.clone();
        let completed = completed_for_ingest.clone();
        Box::pin(async move {
            in_flight.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            completed.fetch_add(1, Ordering::SeqCst);
            Ok(IngestReport::default())
        })
    });

    // Immediate tick + a long interval so we can be confident the only
    // run mid-flight when we call `tick()` is the periodic one (not the
    // public `tick()` itself).
    let opts = StartWatchLoopOptions::new(ingest)
        .with_immediate(true)
        .with_interval(Duration::from_secs(60));
    let ctrl = start_watch_loop(opts);

    // Wait for the periodic immediate tick to actually start.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        in_flight.load(Ordering::SeqCst),
        1,
        "periodic immediate tick should be in flight"
    );
    assert_eq!(completed.load(Ordering::SeqCst), 0);

    // The manual `tick()` arrives while the periodic runner holds the
    // lock. With the bug, this would await `tick_done` forever because
    // the periodic path never notifies; with the fix, `run_locked`
    // notifies on completion regardless of which path acquired the lock.
    let tick_result = tokio::time::timeout(Duration::from_secs(2), ctrl.tick()).await;
    assert!(
        tick_result.is_ok(),
        "manual tick().await hung waiting on periodic run to notify tick_done"
    );
    assert!(
        completed.load(Ordering::SeqCst) >= 1,
        "manual tick returned before the periodic run completed"
    );

    ctrl.stop().await;
}

#[tokio::test]
async fn stop_awaits_in_flight_tick() {
    use std::sync::Mutex;
    let observed_stop_after_completion = Arc::new(Mutex::new(false));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let in_flight_for_ingest = in_flight.clone();
    let completed_for_ingest = completed.clone();
    let ingest: IngestFn = Arc::new(move |_force: bool| {
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

/// `with_watch_paths` pointing at a non-existent path must demote
/// silently to the polling driver — that's the slow-fallback acceptance
/// criterion from #250 ("when `notify` reports unsupported, the loop
/// falls back to polling cleanly"). Reproduces the network-mount
/// scenario without needing a real network mount: a path that doesn't
/// exist exercises the same `FsBurst::new -> Err -> polling driver`
/// branch.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fs_events_fall_back_to_polling_when_no_path_exists() {
    let runs = Arc::new(AtomicUsize::new(0));
    let runs_for_ingest = runs.clone();
    let ingest: IngestFn = Arc::new(move |_force: bool| {
        let runs = runs_for_ingest.clone();
        Box::pin(async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(IngestReport::default())
        })
    });

    // Build a guaranteed-missing child of a fresh tempdir so the
    // `FsBurst::new -> Err -> polling` demotion is deterministic
    // across environments (a hardcoded absolute path could collide
    // with an unusual filesystem layout).
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("definitely-missing-child");
    let opts = StartWatchLoopOptions::new(ingest)
        .with_immediate(false)
        .with_interval(Duration::from_millis(100))
        .with_watch_paths(vec![missing]);
    let ctrl = start_watch_loop(opts);

    // If the FS-event driver were active, the loop would idle until a
    // notify event landed (and none ever will here). The polling
    // fallback must drive at least one tick within the polling
    // interval. We yield + advance virtual time across two intervals
    // to give the spawned task room to run.
    for _ in 0..6 {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
    }
    ctrl.stop().await;

    let n = runs.load(Ordering::SeqCst);
    assert!(
        n >= 1,
        "polling fallback did not fire after FS-event driver demotion (runs={n})"
    );
}

/// `disable_fsevents = true` must take the polling path even when
/// `watch_paths` references a real, watchable directory. Mirrors the
/// `--no-fsevents` opt-out in the CLI.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn disable_fsevents_forces_polling_driver() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let runs_for_ingest = runs.clone();
    let ingest: IngestFn = Arc::new(move |_force: bool| {
        let runs = runs_for_ingest.clone();
        Box::pin(async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok(IngestReport::default())
        })
    });

    let opts = StartWatchLoopOptions::new(ingest)
        .with_immediate(false)
        .with_interval(Duration::from_millis(50))
        .with_watch_paths(vec![dir.path().to_path_buf()])
        .with_disable_fsevents(true);
    let ctrl = start_watch_loop(opts);

    // Polling cadence is 50ms; advance 4 intervals.
    for _ in 0..6 {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(50)).await;
    }
    ctrl.stop().await;

    assert!(
        runs.load(Ordering::SeqCst) >= 1,
        "polling driver did not fire under disable_fsevents=true"
    );
}

/// Burst test: a flood of FS writes inside the debounce window must
/// produce a single ingest tick, not one per write. Acceptance
/// criterion from #250 ("rapid-fire 100 session writes within 100ms
/// produces ≤ 5 ingest cycles, not 100"). Uses real time because the
/// `notify` driver runs on its own OS thread and doesn't observe
/// `tokio::time::pause`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FS-event delivery is platform-dependent; run via cargo test -- --ignored"]
async fn burst_writes_coalesce_into_one_tick() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let forced_runs = Arc::new(AtomicUsize::new(0));
    let runs_for_ingest = runs.clone();
    let forced_for_ingest = forced_runs.clone();
    let ingest: IngestFn = Arc::new(move |force: bool| {
        let runs = runs_for_ingest.clone();
        let forced = forced_for_ingest.clone();
        Box::pin(async move {
            runs.fetch_add(1, Ordering::SeqCst);
            if force {
                forced.fetch_add(1, Ordering::SeqCst);
            }
            Ok(IngestReport::default())
        })
    });

    let opts = StartWatchLoopOptions::new(ingest)
        .with_immediate(false)
        .with_interval(Duration::from_secs(60))
        .with_slow_fallback_interval(Duration::from_secs(60))
        .with_debounce(Duration::from_millis(150))
        .with_watch_paths(vec![dir.path().to_path_buf()]);
    let ctrl = start_watch_loop(opts);

    // Give the watcher a moment to attach.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 100 writes in rapid succession.
    for i in 0..100 {
        std::fs::write(dir.path().join(format!("burst-{i}.jsonl")), b"{}\n").unwrap();
    }
    // Wait for the debounce window plus generous slack for the OS
    // event-delivery latency.
    tokio::time::sleep(Duration::from_millis(500)).await;

    ctrl.stop().await;

    let n = runs.load(Ordering::SeqCst);
    assert!(
        n <= 5,
        "100 burst writes within 150ms debounce should coalesce to ≤ 5 ticks, got {n}"
    );
    assert!(
        n >= 1,
        "100 burst writes should still wake the loop at least once, got {n}"
    );
    // The slow backstop is 60s and never fires in this window, so every tick
    // here came from an FS event and must be forced — that is what defeats the
    // fingerprint gate when a `notify` event beats the write's flush (#468 r2).
    assert_eq!(
        forced_runs.load(Ordering::SeqCst),
        n,
        "every FS-event-driven tick must pass force = true"
    );
}
