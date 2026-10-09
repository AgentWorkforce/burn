//! `burn ingest` — keep the ledger up to date from session history. No
//! flags syncs the relayhistory store once and appends what changed;
//! `--watch` keeps doing that as sessions change; `--hook claude --quiet`
//! is the stdin-driven Claude hook path.
//!
//! Thin presenter over the SDK ingest verbs:
//!
//! - No flags: [`ingest_all`] — one sync, then exit.
//! - `--watch`: [`watch_ingest`] — relayhistory's live-capture loop in the
//!   foreground until SIGINT / SIGTERM.
//! - `--hook claude`: [`ingest_claude_transcript_path`] on the payload's
//!   `transcript_path`, so the per-call cost is bounded by that one
//!   transcript, not by the number of sessions on disk.
//!
//! Output shape: every successful run writes a single
//! `[burn] ingest: ingested N session(s) (+M turn(s))` line. The one-shot
//! path emits it on **stdout** so pipelines can capture the summary;
//! `--watch` and `--hook` modes log on **stderr** so the foreground banner
//! and hook breadcrumbs don't pollute downstream stdout consumers.
//! `--quiet` is accepted in every mode: in `--watch` and `--hook` it
//! silences every breadcrumb; in one-shot mode it suppresses the progress
//! spinner but the final stdout summary still prints.

use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use relayburn_sdk::{
    ai_hist::StopToken, ingest_all, ingest_claude_transcript_path, watch_ingest, IngestOptions,
    IngestReport, Ledger, LedgerHandle, LedgerOpenOptions, WatchIngestOptions,
};

use crate::cli::{GlobalArgs, IngestArgs};
use crate::render::error::report_error;
use crate::render::progress::TaskProgress;
use crate::render::stdout::write_stdout;

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

/// One-shot: open the ledger, run a single `ingest_all`, log the summary,
/// exit. The summary line goes to **stdout** so callers can capture it
/// without redirecting stderr; `--quiet` suppresses only the spinner.
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
    let opts = ingest_options(globals, progress.as_ref());
    let result = ingest_all(handle.raw_mut(), &opts);
    if let Some(p) = &progress {
        p.finish_and_clear();
    }
    match result {
        Ok(report) => {
            if let Err(err) = log_report_oneshot(&report) {
                return report_error(&err, globals);
            }
            0
        }
        Err(err) => report_error(&err, globals),
    }
}

/// `--watch` mode: run [`watch_ingest`] on this thread, with a signal
/// thread stopping it on SIGINT / SIGTERM. relayhistory's loop owns the
/// filesystem-event driver and its polling fallback; each tick appends
/// whatever the store gained.
fn run_watch(globals: &GlobalArgs, args: &IngestArgs) -> i32 {
    let interval_ms = match args.interval {
        Some(0) => {
            eprintln!("burn: ingest --interval must be a positive integer in milliseconds");
            return EXIT_FLAG_MISUSE;
        }
        Some(n) => n,
        None => 1000,
    };

    let progress = (!args.quiet).then(|| {
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

    let watch_message = if args.no_fsevents {
        format!("watching (polling every {interval_ms}ms); Ctrl-C to stop")
    } else {
        "watching (FS events with polling fallback); Ctrl-C to stop".to_string()
    };
    if let Some(p) = &progress {
        if p.is_visible() {
            p.set_task(watch_message.clone());
        } else {
            eprintln!("[burn] ingest: foreground ingest {watch_message}");
        }
    }

    let watch = WatchIngestOptions {
        poll_interval: Duration::from_millis(interval_ms),
        use_fs_events: !args.no_fsevents,
        stop: StopToken::new(),
    };
    stop_on_signal(watch.stop.clone());
    // Progress updates would overwrite the watch banner on every tick, so
    // the loop runs without them.
    let opts = IngestOptions {
        ledger_home: globals.ledger_path.clone(),
        ..IngestOptions::default()
    };
    let result = watch_ingest(handle.raw_mut(), &opts, watch, |tick| {
        report_tick(progress.as_ref(), tick);
    });
    if let Some(p) = &progress {
        p.finish_and_clear();
    }
    match result {
        Ok(()) => 0,
        Err(err) => report_error(&err, globals),
    }
}

/// Log a tick that appended turns (unless quiet), or the error that stopped
/// it. Empty ticks stay silent so a quiet machine is a quiet terminal.
fn report_tick(progress: Option<&TaskProgress>, tick: anyhow::Result<IngestReport>) {
    match (tick, progress) {
        (Ok(report), Some(p)) if report.appended_turns > 0 => {
            p.suspend(|| eprint!("{}", render_ingest_line(&report)));
        }
        (Ok(_), _) => {}
        (Err(err), Some(p)) => p.suspend(|| eprintln!("[burn] ingest: {err:#}")),
        (Err(err), None) => eprintln!("[burn] ingest: {err:#}"),
    }
}

/// Stop `stop` from a background thread once SIGINT or SIGTERM arrives.
fn stop_on_signal(stop: StopToken) {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Ok(runtime) = runtime {
            runtime.block_on(wait_for_stop_signal());
        }
        stop.stop();
    });
}

/// `--hook <harness>`: read a JSON payload from stdin and ingest the
/// transcript it references. Today only `--hook claude` is supported.
///
/// The TS implementation tries hard not to fail Claude Code hooks (a
/// non-zero exit can block the surrounding tool call); the Rust port
/// keeps that policy — every error is logged to stderr but the exit
/// code is `0` so the calling Claude Code session continues.
///
/// Fast-path: when the payload carries a `transcript_path` we index just
/// that transcript instead of syncing every provider. Falls back to
/// `ingest_all` when the payload is missing `transcript_path` (older
/// Claude Code releases occasionally elide it) so we still make forward
/// progress.
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
    let opts = ingest_options(globals, progress.as_ref());
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

/// Ingest options honoring the global `--ledger-path` override, driving
/// `progress` when there is one.
fn ingest_options(globals: &GlobalArgs, progress: Option<&TaskProgress>) -> IngestOptions {
    match progress {
        Some(p) => p.ingest_options(globals.ledger_path.clone()),
        None => IngestOptions {
            ledger_home: globals.ledger_path.clone(),
            ..IngestOptions::default()
        },
    }
}

/// Open a ledger honoring the global `--ledger-path` override.
fn open_handle(globals: &GlobalArgs) -> anyhow::Result<LedgerHandle> {
    let opts = match globals.ledger_path.as_deref() {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    Ledger::open(opts)
}

/// Format an `IngestReport` as the canonical log line, shared by the watch
/// loop and one-shot mode.
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
/// one-shot path, so pipelines that capture stdout see the summary.
fn log_report_oneshot(report: &IngestReport) -> std::io::Result<()> {
    write_stdout(&render_ingest_line(report))
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
/// signal.
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
