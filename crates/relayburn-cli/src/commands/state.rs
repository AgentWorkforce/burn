//! `burn state` — inspect or rebuild derived state under
//! `$RELAYBURN_HOME` (status, rebuild index | classify | content |
//! archive, prune, reset).
//!
//! Thin presenter over the maintenance verbs on `relayburn-sdk`. The
//! status report walks `burn.sqlite` (per-table row counts in the
//! events DB), `content.sqlite` (row count + size), and the embedded
//! `archive_state` metadata.

use relayburn_sdk::{
    ingest_all, Ledger, LedgerHandle, LedgerOpenOptions, ResetSummary, StateStatus,
};

use crate::cli::{
    GlobalArgs, StateArgs, StateRebuildArgs, StateRebuildTarget, StateSubcommand,
};
use crate::render::error::{report_error, report_ledger_error};
use crate::render::json::render_json;
use crate::render::progress::TaskProgress;

pub fn run(globals: &GlobalArgs, args: StateArgs) -> i32 {
    let sub = args
        .command
        .unwrap_or(StateSubcommand::Status(Default::default()));
    match sub {
        StateSubcommand::Status(_) => run_status(globals),
        StateSubcommand::Rebuild(rebuild) => run_rebuild(globals, rebuild),
        StateSubcommand::Prune(prune) => run_prune(globals, prune),
        StateSubcommand::Reset(reset) => run_reset(globals, reset),
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn run_status(globals: &GlobalArgs) -> i32 {
    let progress = TaskProgress::new(globals, "state");
    let opts = LedgerOpenOptions {
        home: globals.ledger_path.clone(),
        content_home: None,
    };
    progress.set_task("opening ledger");
    let handle = match Ledger::open(opts) {
        Ok(h) => h,
        Err(err) => {
            progress.finish_and_clear();
            return report_anyhow(&err, globals);
        }
    };
    progress.set_task("reading derived state");
    let status = match handle.state_status() {
        Ok(s) => s,
        Err(err) => {
            progress.finish_and_clear();
            return report_anyhow(&err, globals);
        }
    };
    progress.finish_and_clear();

    if globals.json {
        if let Err(err) = render_json(&status) {
            return report_error(&err, globals);
        }
        return 0;
    }

    print!("{}", format_status(&status));
    0
}

fn format_status(s: &StateStatus) -> String {
    let mut out = String::new();
    out.push_str(&format!("derived state at {}:\n", s.home));
    out.push_str("events DB (burn.sqlite):\n");
    out.push_str(&format!(
        "  path: {}\n",
        rel_to_home(&s.burn.path, &s.home)
    ));
    if !s.burn.exists {
        out.push_str("  status: not built yet\n");
    }
    out.push_str(&format!(
        "  tracked rows: {}\n",
        format_int(s.burn.tracked_rows)
    ));
    out.push_str(&format!(
        "    turns:              {}\n",
        format_int(s.burn.rows.turns)
    ));
    out.push_str(&format!(
        "    user_turns:         {}\n",
        format_int(s.burn.rows.user_turns)
    ));
    out.push_str(&format!(
        "    compactions:        {}\n",
        format_int(s.burn.rows.compactions)
    ));
    out.push_str(&format!(
        "    relationships:      {}\n",
        format_int(s.burn.rows.relationships)
    ));
    out.push_str(&format!(
        "    tool_result_events: {}\n",
        format_int(s.burn.rows.tool_result_events)
    ));
    out.push_str(&format!(
        "    sessions:           {}\n",
        format_int(s.burn.rows.sessions)
    ));
    out.push_str(&format!(
        "    stamps:             {}\n",
        format_int(s.burn.rows.stamps)
    ));
    out.push_str("content DB (content.sqlite):\n");
    out.push_str(&format!(
        "  path: {}\n",
        rel_to_home(&s.content.path, &s.home)
    ));
    if !s.content.exists {
        out.push_str("  status: not built yet\n");
    }
    out.push_str(&format!("  rows: {}\n", format_int(s.content.rows)));
    out.push_str("archive state:\n");
    out.push_str(&format!("  schema version: {}\n", s.archive.schema_version));
    out.push_str(&format!(
        "  last built:   {}\n",
        s.archive.last_built_at.as_deref().unwrap_or("never")
    ));
    out.push_str(&format!(
        "  last rebuild: {}\n",
        s.archive.last_rebuild_at.as_deref().unwrap_or("never")
    ));
    out.push_str("config:\n");
    out.push_str(&format!("  store: {}\n", s.config.store));
    let retention = if s.config.retention_forever {
        "forever".to_string()
    } else {
        match s.config.retention_days {
            Some(d) => format!("{} days", format_retention_days(d)),
            None => "forever".to_string(),
        }
    };
    out.push_str(&format!("  retention: {}\n", retention));
    out
}

fn rel_to_home(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    // Normalize trailing slash so `home="/x/home/"` and `home="/x/home"`
    // behave the same; bail if `home` was just `/` (or all slashes) —
    // that's not a meaningful prefix to rewrite.
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return path.to_string();
    }
    // Treat `path` as inside `home` only when it equals home or
    // continues with a `/` separator. This rejects byte-prefix
    // collisions like `/x/home2/foo` against `home="/x/home"`, which
    // the prior `path.starts_with(home)` check would mislabel as
    // `${RELAYBURN_HOME}/2/foo`.
    let rest = match path.strip_prefix(home) {
        Some("") => "",
        Some(after) if after.starts_with('/') => after.trim_start_matches('/'),
        _ => return path.to_string(),
    };
    format!("${{RELAYBURN_HOME}}/{}", rest)
}

fn format_int(n: u64) -> String {
    // Insert thousands separators with commas; matches the TS
    // `formatInt`'s `Intl.NumberFormat('en-US')` output.
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{} bytes", n);
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut v = n as f64 / 1024.0;
    let mut i = 0usize;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    let formatted = if v >= 100.0 {
        format!("{:.0}", v)
    } else if v >= 10.0 {
        format!("{:.1}", v)
    } else {
        format!("{:.2}", v)
    };
    format!("{} {}", formatted, units[i])
}

fn format_retention_days(d: f64) -> String {
    if d.fract() == 0.0 {
        format!("{}", d as u64)
    } else {
        format!("{}", d)
    }
}

// ---------------------------------------------------------------------------
// rebuild
// ---------------------------------------------------------------------------

fn run_rebuild(globals: &GlobalArgs, args: StateRebuildArgs) -> i32 {
    // Every rebuild target (index / classify / content / archive /
    // all) collapses onto a single `rebuild_derivable` SQL
    // transaction in the 2.0 SQLite layout. The per-target arms are
    // kept so `burn state rebuild --help` lists meaningful targets
    // and scripts that select a specific artifact still parse.
    match args.target {
        StateRebuildTarget::Index
        | StateRebuildTarget::Classify
        | StateRebuildTarget::Content
        | StateRebuildTarget::Archive
        | StateRebuildTarget::All => run_rebuild_derivable(globals),
    }
}

fn run_rebuild_derivable(globals: &GlobalArgs) -> i32 {
    let progress = TaskProgress::new(globals, "state");
    let opts = LedgerOpenOptions {
        home: globals.ledger_path.clone(),
        content_home: None,
    };
    progress.set_task("opening ledger");
    let mut handle = match Ledger::open(opts) {
        Ok(h) => h,
        Err(err) => {
            progress.finish_and_clear();
            return report_anyhow(&err, globals);
        }
    };
    progress.set_task("rebuilding derivable state");
    let summary = match handle.raw_mut().rebuild_derivable() {
        Ok(s) => s,
        Err(err) => {
            progress.finish_and_clear();
            return report_ledger_error(&err, globals);
        }
    };
    progress.finish_and_clear();
    if globals.json {
        let payload = serde_json::json!({
            "rowsDropped": summary.rows_dropped,
            "contentRowsDropped": summary.content_rows_dropped,
        });
        if let Err(err) = render_json(&payload) {
            return report_error(&err, globals);
        }
    } else {
        println!(
            "rebuilt derivable state: dropped {} event rows + {} content rows",
            format_int(summary.rows_dropped as u64),
            format_int(summary.content_rows_dropped as u64),
        );
        println!(
            "  re-ingest from upstream session files via 'burn ingest' to \
             repopulate."
        );
    }
    0
}

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

fn run_prune(globals: &GlobalArgs, args: crate::cli::StatePruneArgs) -> i32 {
    use relayburn_sdk::{load_config_with_home, Retention};
    let progress = TaskProgress::new(globals, "state");
    // Load retention config from the same home the ledger will be
    // opened under below, so `--ledger-path /foo` reads
    // `/foo/config.json` instead of mixing in `$RELAYBURN_HOME`'s
    // retention against `/foo`'s DB. Mirrors the equivalent fix in
    // `state_status`.
    progress.set_task("loading retention config");
    let cfg = match load_config_with_home(globals.ledger_path.as_deref()) {
        Ok(c) => c,
        Err(err) => {
            progress.finish_and_clear();
            return report_ledger_error(&err, globals);
        }
    };
    let retention = match args.days.as_deref() {
        Some(s) => match parse_retention(s) {
            Some(r) => r,
            None => {
                progress.finish_and_clear();
                let msg = format!(
                    "invalid --days value: {:?} (expected a number or \"forever\")",
                    s
                );
                if globals.json {
                    let envelope = serde_json::json!({ "error": msg });
                    let _ = render_json(&envelope);
                } else {
                    eprintln!("burn state prune: {msg}");
                }
                return 2;
            }
        },
        None => cfg.content.retention_days,
    };

    let cutoff_ms = match retention {
        Retention::Forever => {
            progress.finish_and_clear();
            if globals.json {
                let payload =
                    serde_json::json!({ "rowsDeleted": 0, "bytesFreed": 0, "retention": "forever" });
                let _ = render_json(&payload);
            } else {
                println!("content retention=forever - nothing to prune");
            }
            return 0;
        }
        Retention::Days(_) => match retention.as_millis() {
            Some(ms) => ms,
            None => {
                progress.finish_and_clear();
                if globals.json {
                    let payload = serde_json::json!({
                        "rowsDeleted": 0, "bytesFreed": 0, "retention": "forever"
                    });
                    let _ = render_json(&payload);
                } else {
                    println!("content retention=forever - nothing to prune");
                }
                return 0;
            }
        },
    };

    // The 2.0 content store stamps each row with a monotonic
    // `ts:{:020}.{:09}` value (see `writer::now_lex_token`); compare by
    // computing a lex-comparable cutoff string in the SAME format from
    // the current wall-clock minus the retention window. Using a
    // narrower padding (e.g. `{:013}.000`) makes every stamped row
    // lexicographically GREATER than the cutoff, so a literal
    // `created_at < cutoff` deletes nothing (or, after a width mismatch
    // flips the sort, deletes everything). The padding must match.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let cutoff_ms = now_ms.saturating_sub(cutoff_ms);
    let cutoff = format_cutoff_ts(cutoff_ms);

    let opts = LedgerOpenOptions {
        home: globals.ledger_path.clone(),
        content_home: None,
    };
    progress.set_task("opening ledger");
    let mut handle = match Ledger::open(opts) {
        Ok(h) => h,
        Err(err) => {
            progress.finish_and_clear();
            return report_anyhow(&err, globals);
        }
    };
    progress.set_task("pruning content rows");
    let stats = match handle.raw_mut().prune_content_older_than(&cutoff) {
        Ok(s) => s,
        Err(err) => {
            progress.finish_and_clear();
            return report_ledger_error(&err, globals);
        }
    };
    progress.finish_and_clear();

    if globals.json {
        let payload = serde_json::json!({
            "rowsDeleted": stats.rows_deleted,
            "bytesFreed": stats.bytes_freed,
            "cutoff": cutoff,
        });
        if let Err(err) = render_json(&payload) {
            return report_error(&err, globals);
        }
    } else {
        println!(
            "pruned {} content row{} ({})",
            format_int(stats.rows_deleted as u64),
            if stats.rows_deleted == 1 { "" } else { "s" },
            format_bytes(stats.bytes_freed.max(0) as u64)
        );
    }
    0
}

/// Format a wall-clock millisecond value as a `ts:{:020}.{:09}` string
/// that is lexically comparable against the `content.created_at` rows
/// stamped by `relayburn_sdk::ledger::writer::now_lex_token`. Both the seconds
/// (20 chars, zero-padded) and the nanosecond fraction (9 chars,
/// zero-padded) widths must match exactly — any narrower padding flips
/// the lexical ordering and breaks the `created_at < cutoff` filter.
fn format_cutoff_ts(ms: u64) -> String {
    let secs = ms / 1_000;
    let nanos = (ms % 1_000) * 1_000_000;
    format!("ts:{:020}.{:09}", secs, nanos)
}

fn parse_retention(s: &str) -> Option<relayburn_sdk::Retention> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("forever") {
        return Some(relayburn_sdk::Retention::Forever);
    }
    let n: f64 = trimmed.parse().ok()?;
    if !n.is_finite() {
        return None;
    }
    if n < 0.0 {
        return Some(relayburn_sdk::Retention::Forever);
    }
    Some(relayburn_sdk::Retention::Days(n))
}

// ---------------------------------------------------------------------------
// reset
// ---------------------------------------------------------------------------
//
// Wipes derived state under `$RELAYBURN_HOME`: truncate every derivable
// + first-party table inside `burn.sqlite` and the `content` table
// inside `content.sqlite`, then blank the ingest cursors so the next
// `burn ingest` walks every upstream file from offset 0.
//
// Without `--force`, this is a dry-run: it opens the ledger, counts
// what would be dropped, prints the report, and exits 0. With
// `--force`, the SDK `reset()` actually performs the wipe. With
// `--force --reingest`, a follow-up `ingest_all` sweep runs on the
// same handle.

fn run_reset(globals: &GlobalArgs, args: crate::cli::StateResetArgs) -> i32 {
    let progress = TaskProgress::new(globals, "state");
    let opts = LedgerOpenOptions {
        home: globals.ledger_path.clone(),
        content_home: None,
    };
    progress.set_task("opening ledger");
    let mut handle = match Ledger::open(opts) {
        Ok(h) => h,
        Err(err) => {
            progress.finish_and_clear();
            return report_anyhow(&err, globals);
        }
    };

    if !args.force {
        progress.set_task("counting reset targets");
        let summary = match handle.raw().count_reset_targets() {
            Ok(s) => s,
            Err(err) => {
                progress.finish_and_clear();
                return report_ledger_error(&err, globals);
            }
        };
        progress.finish_and_clear();
        return print_reset_report(globals, &summary, /*executed=*/ false, None);
    }

    progress.set_task("resetting derived state");
    let summary = match handle.raw_mut().reset() {
        Ok(s) => s,
        Err(err) => {
            progress.finish_and_clear();
            return report_ledger_error(&err, globals);
        }
    };

    let ingest_report = if args.reingest {
        match run_reset_reingest(&mut handle, globals.ledger_path.clone(), &progress) {
            Ok(r) => Some(r),
            Err(err) => {
                progress.finish_and_clear();
                return report_error(&err, globals);
            }
        }
    } else {
        None
    };

    progress.finish_and_clear();
    print_reset_report(globals, &summary, /*executed=*/ true, ingest_report.as_ref())
}

/// Drive a single `ingest_all` sweep on the open handle. Mirrors the
/// `run_ingest` helper in `commands/summary.rs`: the SDK verb is async,
/// so we spin a current-thread tokio runtime to drive it from this
/// otherwise-sync presenter.
///
/// `ledger_home` propagates the global `--ledger-path` override into
/// `RawIngestOptions::ledger_home` so sidecar ingest state (config and
/// pending-stamp manifests) resolves under the same home as the open
/// handle. Without this, `burn --ledger-path <custom> state reset
/// --force --reingest` would write turns into the custom DB while
/// reading config / pending stamps from `$RELAYBURN_HOME`.
fn run_reset_reingest(
    handle: &mut LedgerHandle,
    ledger_home: Option<std::path::PathBuf>,
    progress: &TaskProgress,
) -> anyhow::Result<relayburn_sdk::IngestReport> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    progress.set_task("re-ingesting sessions");
    let opts = progress.ingest_options(ledger_home);
    rt.block_on(ingest_all(handle.raw_mut(), &opts))
}

fn print_reset_report(
    globals: &GlobalArgs,
    summary: &ResetSummary,
    executed: bool,
    ingest_report: Option<&relayburn_sdk::IngestReport>,
) -> i32 {
    if globals.json {
        let mut payload = serde_json::json!({
            "executed": executed,
            "rowsDropped": summary.rows_dropped,
            "stampsDropped": summary.stamps_dropped,
            "contentRowsDropped": summary.content_rows_dropped,
        });
        if let Some(report) = ingest_report {
            payload["reingest"] = serde_json::json!({
                "scannedSessions": report.scanned_sessions,
                "ingestedSessions": report.ingested_sessions,
                "appendedTurns": report.appended_turns,
                "appliedPendingStamps": report.applied_pending_stamps,
            });
        }
        if let Err(err) = render_json(&payload) {
            return report_error(&err, globals);
        }
        return 0;
    }

    if executed {
        println!(
            "reset derived state: dropped {} event row{} + {} stamp{} + {} content row{}",
            format_int(summary.rows_dropped as u64),
            if summary.rows_dropped == 1 { "" } else { "s" },
            format_int(summary.stamps_dropped as u64),
            if summary.stamps_dropped == 1 { "" } else { "s" },
            format_int(summary.content_rows_dropped as u64),
            if summary.content_rows_dropped == 1 { "" } else { "s" },
        );
        match ingest_report {
            Some(report) => {
                println!(
                    "  re-ingested {} session{} (+{} turn{}).",
                    format_int(report.ingested_sessions as u64),
                    if report.ingested_sessions == 1 { "" } else { "s" },
                    format_int(report.appended_turns as u64),
                    if report.appended_turns == 1 { "" } else { "s" },
                );
            }
            None => {
                println!(
                    "  re-ingest from upstream session files via 'burn ingest' to \
                     repopulate (or re-run with --reingest)."
                );
            }
        }
    } else {
        println!(
            "burn state reset (dry run): would drop {} event row{} + {} stamp{} + {} content row{}.",
            format_int(summary.rows_dropped as u64),
            if summary.rows_dropped == 1 { "" } else { "s" },
            format_int(summary.stamps_dropped as u64),
            if summary.stamps_dropped == 1 { "" } else { "s" },
            format_int(summary.content_rows_dropped as u64),
            if summary.content_rows_dropped == 1 { "" } else { "s" },
        );
        println!("  re-run with --force to actually wipe (add --reingest to repopulate).");
    }
    0
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Surface an `anyhow::Error` via the generic-error path. The SDK's
/// `state_status` returns `anyhow::Result`; we still want a `--json`
/// envelope and the same exit-code mapping the typed reporters use.
fn report_anyhow(err: &anyhow::Error, globals: &GlobalArgs) -> i32 {
    // Prefer the typed reporter when the underlying cause is a
    // `LedgerError` so the `EXIT_LEDGER_ERROR` code surfaces.
    if let Some(le) = err.downcast_ref::<relayburn_sdk::LedgerError>() {
        return report_ledger_error(le, globals);
    }
    report_error(err, globals)
}

#[cfg(test)]
mod tests {
    use super::{format_cutoff_ts, rel_to_home};

    /// Mirror of `relayburn_sdk::ledger::writer::now_lex_token`'s format string.
    /// Re-deriving it locally guards against the writer's format drifting
    /// without the cutoff helper following.
    fn writer_style_ts(secs: u64, nanos_part: u64) -> String {
        format!("ts:{:020}.{:09}", secs, nanos_part)
    }

    #[test]
    fn cutoff_matches_writer_format_byte_for_byte() {
        // 1234.567 seconds since epoch, expressed in ms, must produce
        // the same string the writer would stamp for that instant.
        let ms = 1_234_567u64;
        let writer = writer_style_ts(1_234, 567_000_000);
        assert_eq!(format_cutoff_ts(ms), writer);
    }

    #[test]
    fn cutoff_is_lex_comparable_against_writer_rows() {
        // A row stamped *before* the cutoff sorts lex-less; a row
        // stamped *after* sorts lex-greater. This is the invariant
        // `prune_content_older_than(&cutoff)` relies on.
        let cutoff = format_cutoff_ts(2_000); // 2.000s
        let earlier_row = writer_style_ts(1, 500_000_000); // 1.500s
        let later_row = writer_style_ts(2, 500_000_000); // 2.500s
        assert!(earlier_row.as_str() < cutoff.as_str());
        assert!(later_row.as_str() > cutoff.as_str());
    }

    #[test]
    fn cutoff_padding_widths_are_stable() {
        // Width of `ts:` + 20-digit secs + `.` + 9-digit nanos = 33.
        // A narrower padding (the original `{:013}.000` bug) would flip
        // the lex ordering — cover the constant here so a formatting
        // tweak that breaks the invariant fails this test first.
        assert_eq!(format_cutoff_ts(0).len(), 33);
        assert_eq!(format_cutoff_ts(u64::MAX).len(), 33);
    }

    #[test]
    fn rel_to_home_rewrites_paths_inside_home() {
        assert_eq!(
            rel_to_home("/x/home/burn.sqlite", "/x/home"),
            "${RELAYBURN_HOME}/burn.sqlite"
        );
        assert_eq!(
            rel_to_home("/x/home/sub/dir/file", "/x/home"),
            "${RELAYBURN_HOME}/sub/dir/file"
        );
    }

    #[test]
    fn rel_to_home_rejects_byte_prefix_siblings() {
        // The bug guarded against: `/x/home2/...` mustn't be treated as
        // `home="/x/home"`'s child (would have rewritten to
        // `${RELAYBURN_HOME}/2/...` under the old `starts_with` byte
        // match).
        assert_eq!(
            rel_to_home("/x/home2/burn.sqlite", "/x/home"),
            "/x/home2/burn.sqlite"
        );
        assert_eq!(rel_to_home("/x/homer", "/x/home"), "/x/homer");
    }

    #[test]
    fn rel_to_home_normalizes_trailing_slash_on_home() {
        // `home` with or without a trailing slash should produce the
        // same rewrite for paths underneath.
        assert_eq!(
            rel_to_home("/x/home/burn.sqlite", "/x/home/"),
            "${RELAYBURN_HOME}/burn.sqlite"
        );
        assert_eq!(
            rel_to_home("/x/home2/foo", "/x/home/"),
            "/x/home2/foo"
        );
    }

    #[test]
    fn rel_to_home_handles_degenerate_home_inputs() {
        // Empty home is a passthrough; a `/`-only home is too — the
        // rewrite would be meaningless ("everything is inside root").
        assert_eq!(rel_to_home("/x/home/foo", ""), "/x/home/foo");
        assert_eq!(rel_to_home("/x/home/foo", "/"), "/x/home/foo");
        assert_eq!(rel_to_home("/x/home/foo", "//"), "/x/home/foo");
    }

    #[test]
    fn rel_to_home_path_equals_home() {
        // `path == home` preserves the prior trailing-slash output
        // shape (`${RELAYBURN_HOME}/`) — callers downstream that
        // pattern-match on the prefix shouldn't see a behavioral
        // change here.
        assert_eq!(rel_to_home("/x/home", "/x/home"), "${RELAYBURN_HOME}/");
        assert_eq!(rel_to_home("/x/home/", "/x/home"), "${RELAYBURN_HOME}/");
    }
}
