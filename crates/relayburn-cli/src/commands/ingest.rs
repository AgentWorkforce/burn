//! `burn ingest` — passive-ingest entrypoint. No flags scans every
//! known session store once; `--watch` keeps polling; `--hook claude
//! --quiet` is the stdin-driven Claude hook path.
//!
//! Thin presenter over the SDK ingest verb plus the SDK's watch-loop
//! controller. TS source of truth: `packages/cli/src/commands/ingest.ts`
//! plus `packages/ingest/src/watch-loop.ts`.
//!
//! The Rust port keeps the three modes as a single subcommand so
//! `burn ingest` retains its TS muscle memory:
//!
//! - No flags = `runIngestOnce` — one full sweep, then exit.
//! - `--watch` = `runIngestWatch` — foreground poll loop until SIGINT
//!   / SIGTERM.
//! - `--hook claude` = `runIngestHook` — stdin-driven hook payload.
//!   Today only `--hook claude` is wired here (Codex / OpenCode hooks
//!   were never part of the TS surface either). The hook path uses
//!   the SDK's single-transcript fast-path
//!   (`ingest_claude_transcript_path`) on the `transcript_path` from
//!   the Claude Code hook payload so the per-call cost is bounded by
//!   the one JSONL file, not by the number of sessions on disk.
//!
//! Output shape: every successful run writes a single
//! `[burn] ingest: ingested N session(s) (+M turn(s))` line. The
//! one-shot path emits it on **stdout** so pipelines can capture the
//! summary (matching the TS `runIngestOnce` source-of-truth at
//! `packages/cli/src/commands/ingest.ts:121-126`); `--watch` and
//! `--hook` modes log on **stderr** so the foreground banner / hook
//! breadcrumbs don't pollute downstream stdout consumers. `--quiet`
//! is accepted in every mode: in `--watch` and `--hook` it silences
//! every breadcrumb; in one-shot mode it suppresses the progress
//! spinner / gap warnings but the final stdout summary still prints.
//!
//! Not ported from 1.x: `--opencode-stream`, `--opencode-url`, and
//! `--opencode-global`. The 1.x stream subscribed to the OpenCode dev
//! server's SSE feed directly. 2.x consumes OpenCode through the
//! file-based session store (`~/.local/share/opencode/storage`) on
//! every sweep, and `--watch` reacts to FS events the moment OpenCode
//! writes a new event file. The `OpencodeStreamIngestor` parser still
//! lives in `relayburn-sdk::reader::opencode_stream` for embedders
//! that want to consume the SSE feed themselves, but the CLI no
//! longer ships HTTP client wiring for it.

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use relayburn_sdk::{
    default_session_roots, ingest_all, ingest_claude_transcript_path, start_watch_loop,
    IngestReport, IngestRoots, Ledger, LedgerHandle, LedgerOpenOptions, StartWatchLoopOptions,
};

use crate::cli::{GlobalArgs, IngestArgs};
use crate::render::error::report_error;
use crate::render::progress::TaskProgress;

/// Exit codes mirror the TS CLI:
/// - `0` happy path (including hook-mode empty-payload no-op).
/// - `1` typed/unknown errors during a non-watch run (parse, IO).
/// - `2` flag misuse (`--watch` + `--hook`, unsupported `--hook`,
///   `--hook` without value, `--interval` not a positive integer).
const EXIT_FLAG_MISUSE: i32 = 2;

/// Entrypoint for the `burn ingest` subcommand. Dispatches on the flag
/// triple (`watch`, `hook`, default) and lets the SDK do the heavy
/// lifting.
pub fn run(globals: &GlobalArgs, args: IngestArgs) -> i32 {
    // Mutually-exclusive guard: TS rejects `--watch --hook` with exit 2
    // before doing any IO. Mirror that here so flag misuse gets a stable
    // shell-script-friendly contract.
    if args.watch && args.hook.is_some() {
        eprintln!("burn: ingest --watch and --hook are mutually exclusive");
        return EXIT_FLAG_MISUSE;
    }

    if let Some(hook) = args.hook.as_deref() {
        return run_hook(globals, hook, args.quiet);
    }
    if args.watch {
        return run_watch(globals, &args);
    }
    run_once(globals, args.quiet)
}

/// One-shot scan: open the ledger, run a single `ingest_all`, log the
/// summary, exit.
///
/// Summary line is emitted on **stdout** (matching TS `runIngestOnce`
/// at `packages/cli/src/commands/ingest.ts:121-126`) so callers can
/// capture pipeline output without redirecting stderr. `--quiet`
/// suppresses the progress spinner and gap-warning side channel but
/// leaves the final stdout summary alone so pipeline consumers always
/// see the report.
fn run_once(globals: &GlobalArgs, quiet: bool) -> i32 {
    let progress = (!quiet).then(|| {
        let p = TaskProgress::new(globals, "ingest");
        p.set_task("opening ledger");
        p
    });
    let mut handle = match open_handle(globals) {
        Ok(h) => h,
        Err(err) => {
            if let Some(p) = &progress {
                p.finish_and_clear();
            }
            return report_error(&err, globals);
        }
    };
    if let Some(p) = &progress {
        p.set_task("scanning sessions");
    }
    let opts = match &progress {
        Some(p) => p.ingest_options(globals.ledger_path.clone()),
        None => TaskProgress::quiet_ingest_options(globals.ledger_path.clone()),
    };
    let result = ingest_all(handle.raw_mut(), &opts);
    if let Some(p) = &progress {
        p.finish_and_clear();
    }
    match result {
        Ok(report) => {
            log_report_oneshot(&report);
            0
        }
        Err(err) => report_error(&err, globals),
    }
}

/// `--watch` mode: spin up [`start_watch_loop`] over a persistent ledger
/// handle and a tokio runtime, then park on SIGINT / SIGTERM.
///
/// We share the ledger handle across ticks via an `Arc<Mutex>` so the
/// poll loop reuses one open SQLite connection per process — same shape
/// as the TS adapter, which keeps a single `withLock('ledger', …)`
/// guarded handle alive for the duration of the watch. `RawIngestOptions`
/// is `Default` per tick because none of the per-tick state (progress
/// callbacks, etc.) needs to survive across ticks.
fn run_watch(globals: &GlobalArgs, args: &IngestArgs) -> i32 {
    let interval_ms = match args.interval {
        Some(0) => {
            eprintln!("burn: ingest --interval must be a positive integer in milliseconds");
            return EXIT_FLAG_MISUSE;
        }
        Some(n) => n,
        None => 1000,
    };

    let quiet = args.quiet;
    let progress = (!quiet).then(|| {
        let p = TaskProgress::new(globals, "ingest");
        p.set_task("opening ledger");
        p
    });
    let handle = match open_handle(globals) {
        Ok(h) => h,
        Err(err) => {
            if let Some(p) = &progress {
                p.finish_and_clear();
            }
            return report_error(&err, globals);
        }
    };

    if let Some(p) = &progress {
        p.set_task("starting watcher");
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            if let Some(p) = &progress {
                p.finish_and_clear();
            }
            return report_error(&err, globals);
        }
    };

    let no_fsevents = args.no_fsevents;
    // The FS-event driver may silently demote to polling at startup
    // (no watchable path yet) or mid-run (notify channel closes), so
    // the banner doesn't assert which driver is live — only that the
    // user opted out of the FS-event attempt.
    let watch_message = if no_fsevents {
        format!("watching (polling every {interval_ms}ms); Ctrl-C to stop")
    } else {
        "watching (FS events with polling fallback); Ctrl-C to stop".to_string()
    };
    if let Some(p) = &progress {
        if p.is_visible() {
            p.set_task(watch_message.clone());
        } else if no_fsevents {
            eprintln!(
                "[burn] ingest: foreground ingest polling every {interval_ms}ms; Ctrl-C to stop",
            );
        } else {
            eprintln!(
                "[burn] ingest: foreground ingest on FS events (polling fallback); Ctrl-C to stop",
            );
        }
    }

    let ledger_home = globals.ledger_path.clone();
    let progress_for_loop = progress.clone();
    rt.block_on(async move {
        let handle_arc: Arc<tokio::sync::Mutex<LedgerHandle>> =
            Arc::new(tokio::sync::Mutex::new(handle));
        let handle_for_ingest = handle_arc.clone();
        let progress_for_ingest = progress_for_loop.clone();
        let watch_message_for_ingest = watch_message.clone();
        let ingest_fn: relayburn_sdk::IngestFn = Arc::new(move || {
            let h = handle_for_ingest.clone();
            let progress = progress_for_ingest.clone();
            let ledger_home = ledger_home.clone();
            let watch_message = watch_message_for_ingest.clone();
            Box::pin(async move {
                if let Some(p) = &progress {
                    p.set_task("scanning sessions");
                }
                let mut guard = h.lock().await;
                let opts = match &progress {
                    Some(p) => p.ingest_options(ledger_home),
                    None => TaskProgress::quiet_ingest_options(ledger_home),
                };
                let result = ingest_all(guard.raw_mut(), &opts);
                if let Some(p) = &progress {
                    p.set_task(watch_message);
                }
                result
            })
        });

        let progress_for_report = progress_for_loop.clone();
        let on_report: relayburn_sdk::ReportSink = Arc::new(move |report: &IngestReport| {
            // Match TS: only log a summary when the tick actually
            // appended turns. Empty ticks would otherwise drown the
            // user with zero-progress lines.
            if let Some(p) = &progress_for_report {
                if report.appended_turns > 0 {
                    p.suspend(|| {
                        eprint!("{}", render_ingest_line(report));
                    });
                }
            }
        });

        let progress_for_error = progress_for_loop.clone();
        let on_error: relayburn_sdk::ErrorSink = Arc::new(move |err: &anyhow::Error| match &progress_for_error {
            Some(p) => p.suspend(|| {
                eprintln!("[burn] ingest: {err}");
            }),
            None => eprintln!("[burn] ingest: {err}"),
        });

        // Default to the `notify`-backed FS-event driver against the
        // three session-store roots ingest scans. Falls back to polling
        // automatically when no path exists yet (fresh install) or
        // when the user passes `--no-fsevents`. The slow polling
        // backstop in the SDK keeps progress on filesystems where FS
        // events are unreliable.
        let watch_paths = default_session_roots(&IngestRoots::default());
        let opts = StartWatchLoopOptions::new(ingest_fn)
            .with_interval(Duration::from_millis(interval_ms))
            .with_immediate(true)
            .with_watch_paths(watch_paths)
            .with_disable_fsevents(no_fsevents)
            .with_on_report(on_report)
            .with_on_error(on_error);
        let controller = start_watch_loop(opts);

        wait_for_stop_signal().await;
        controller.stop().await;
    });
    if let Some(p) = &progress {
        p.finish_and_clear();
    }

    0
}

/// `--hook <harness>`: read a JSON payload from stdin and ingest the
/// transcript it references. Today only `--hook claude` is supported.
///
/// The TS implementation tries hard not to fail Claude Code hooks (a
/// non-zero exit can block the surrounding tool call); the Rust port
/// keeps that policy — every error is logged to stderr but the exit
/// code is `0` so the calling Claude Code session continues.
///
/// Fast-path: when the payload carries a `transcript_path` we drive
/// the SDK's single-transcript verb against just that JSONL file
/// instead of a full sweep. Falls back to `ingest_all` when the
/// payload is missing `transcript_path` (older Claude Code releases
/// occasionally elide it) so we still make forward progress.
fn run_hook(globals: &GlobalArgs, hook: &str, quiet: bool) -> i32 {
    if hook != "claude" {
        eprintln!("burn: unsupported hook harness: {hook}");
        return EXIT_FLAG_MISUSE;
    }
    let raw = match read_stdin() {
        Ok(s) => s,
        Err(err) => {
            // Hook callers expect us not to break the parent. Log + 0.
            eprintln!("[burn] ingest: failed to read stdin: {err}");
            return 0;
        }
    };
    if raw.trim().is_empty() {
        if !quiet {
            eprintln!("[burn] ingest: empty stdin payload, nothing to do");
        }
        return 0;
    }

    // Validate the payload shape. The TS hook ignores payloads missing
    // `session_id`; mirror that. `transcript_path` is optional — when
    // present we drive the single-transcript fast-path, when absent we
    // fall back to `ingest_all` so older Claude Code releases that
    // elide the field still make forward progress.
    let transcript_path = match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => {
            let has_session = v.get("session_id").and_then(|x| x.as_str()).is_some();
            if !has_session {
                if !quiet {
                    eprintln!("[burn] ingest: payload missing session_id; ignoring");
                }
                return 0;
            }
            v.get("transcript_path")
                .and_then(|x| x.as_str())
                .map(PathBuf::from)
        }
        Err(err) => {
            eprintln!("[burn] ingest: invalid JSON payload: {err}");
            return 0;
        }
    };

    let progress = (!quiet).then(|| TaskProgress::new(globals, "ingest"));
    if let Some(progress) = &progress {
        progress.set_task("opening ledger");
    }
    let mut handle = match open_handle(globals) {
        Ok(h) => h,
        Err(err) => {
            // Hook policy: never fail the parent.
            if let Some(progress) = &progress {
                progress.finish_and_clear();
            }
            eprintln!("[burn] ingest: {err}");
            return 0;
        }
    };
    if let Some(progress) = &progress {
        progress.set_task("ingesting transcript");
    }
    let opts = match &progress {
        Some(progress) => progress.ingest_options(globals.ledger_path.clone()),
        None => TaskProgress::quiet_ingest_options(globals.ledger_path.clone()),
    };
    let result = match transcript_path.as_deref() {
        Some(path) => ingest_claude_transcript_path(handle.raw_mut(), path, &opts),
        None => ingest_all(handle.raw_mut(), &opts),
    };
    if let Some(progress) = &progress {
        progress.finish_and_clear();
    }
    match result {
        Ok(report) => {
            // In hook mode we keep stderr quiet by default; only log
            // when work was actually done so a per-tool-call hook
            // doesn't spam the user.
            if !quiet && report.appended_turns > 0 {
                eprint!("{}", render_ingest_line(&report));
            }
        }
        Err(err) => {
            eprintln!("[burn] ingest: {err}");
        }
    }
    0
}

/// Open a ledger honoring the global `--ledger-path` override.
fn open_handle(globals: &GlobalArgs) -> anyhow::Result<LedgerHandle> {
    let opts = match globals.ledger_path.as_deref() {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    Ledger::open(opts)
}

/// Format an `IngestReport` as the canonical TS log line. Kept as a
/// pure helper so the watch loop and one-shot mode share output shape.
fn render_ingest_line(report: &IngestReport) -> String {
    let session_word = if report.ingested_sessions == 1 {
        "session"
    } else {
        "sessions"
    };
    let turn_word = if report.appended_turns == 1 {
        "turn"
    } else {
        "turns"
    };
    format!(
        "[burn] ingest: ingested {} {session_word} (+{} {turn_word})\n",
        report.ingested_sessions, report.appended_turns,
    )
}

/// Log the canonical `[burn] ingest: ...` line on **stdout** for the
/// one-shot path. TS source of truth: `runIngestOnce` at
/// `packages/cli/src/commands/ingest.ts:121-126` writes the rendered
/// report via `process.stdout.write`, so pipelines that capture stdout
/// see the summary. `--watch` and `--hook` keep their own stderr
/// emitters (`render_ingest_line` is the shared formatter).
fn log_report_oneshot(report: &IngestReport) {
    print!("{}", render_ingest_line(report));
}

/// Read all of stdin into a String. Returns empty string when stdin is
/// a TTY (no payload) — TS uses the same `process.stdin.isTTY` guard.
fn read_stdin() -> io::Result<String> {
    use std::io::IsTerminal;
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Ok(String::new());
    }
    let mut buf = String::new();
    stdin.lock().read_to_string(&mut buf)?;
    Ok(buf)
}

/// Park until SIGINT or SIGTERM. Cross-platform via tokio's `ctrl_c` for
/// SIGINT; SIGTERM is wired only on Unix because Windows lacks the
/// signal. The watch loop's controller will drain in-flight ticks before
/// returning so callers see all observable side effects.
async fn wait_for_stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                // If we can't install SIGTERM, fall back to ctrl_c only.
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_ingest_line_pluralizes_consistently() {
        let one = render_ingest_line(&IngestReport {
            scanned_sessions: 1,
            ingested_sessions: 1,
            appended_turns: 1,
            applied_pending_stamps: 0,
        });
        assert_eq!(one, "[burn] ingest: ingested 1 session (+1 turn)\n");

        let many = render_ingest_line(&IngestReport {
            scanned_sessions: 3,
            ingested_sessions: 2,
            appended_turns: 5,
            applied_pending_stamps: 0,
        });
        assert_eq!(many, "[burn] ingest: ingested 2 sessions (+5 turns)\n");

        let zero = render_ingest_line(&IngestReport::default());
        assert_eq!(zero, "[burn] ingest: ingested 0 sessions (+0 turns)\n");
    }
}
