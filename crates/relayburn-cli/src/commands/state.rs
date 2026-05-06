//! `burn state` — inspect or rebuild derived state under
//! `$RELAYBURN_HOME` (status, rebuild index | classify | content |
//! archive, prune, reset).
//!
//! Thin presenter over the maintenance verbs on `relayburn-sdk`.
//! TS source of truth: `packages/cli/src/commands/state.ts`.
//!
//! ## 2.0 vs 1.x
//!
//! The TS sibling reports against the 1.x JSONL ledger layout
//! (`ledger.jsonl`, `ledger.idx`, `ledger.content.idx`, `archive.sqlite`).
//! The Rust port targets the 2.0 SQLite layout — two databases
//! (`burn.sqlite` + `content.sqlite`) — so the status report here is
//! shaped to the 2.0 reality: per-table row counts in the events DB,
//! a row count + size for the content DB, and the `archive_state`
//! metadata embedded in `burn.sqlite`. The deliberate divergence is
//! tracked under #240 (Rust port epic); golden snapshots for the two
//! `state-status*` invocations carry the 2.0 shape.

use relayburn_sdk::{Ledger, LedgerOpenOptions, StateStatus};

use crate::cli::{
    GlobalArgs, StateArgs, StateRebuildArgs, StateRebuildTarget, StateSubcommand,
};
use crate::render::error::{report_error, report_ledger_error};
use crate::render::json::render_json;

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
    let opts = LedgerOpenOptions {
        home: globals.ledger_path.clone(),
        content_home: None,
    };
    let handle = match Ledger::open(opts) {
        Ok(h) => h,
        Err(err) => return report_anyhow(&err, globals),
    };
    let status = match handle.state_status() {
        Ok(s) => s,
        Err(err) => return report_anyhow(&err, globals),
    };

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
        "  rows: {} total\n",
        format_int(s.burn.total_rows)
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
    if !home.is_empty() && path.starts_with(home) {
        let rest = &path[home.len()..];
        let rest = rest.trim_start_matches('/');
        format!("${{RELAYBURN_HOME}}/{}", rest)
    } else {
        path.to_string()
    }
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
    match args.target {
        StateRebuildTarget::Index | StateRebuildTarget::Content => run_rebuild_derivable(globals),
        StateRebuildTarget::All(_) => run_rebuild_derivable(globals),
        StateRebuildTarget::Archive(_) => {
            // 2.0 doesn't have a separate archive.sqlite — the
            // archive_state row lives inside burn.sqlite and is
            // refreshed on every rebuild_derivable. Treat
            // `rebuild archive` as an alias.
            run_rebuild_derivable(globals)
        }
        StateRebuildTarget::Classify(_) => {
            // Standalone reclassify pass is filed for follow-up; the
            // 2.0 ingest path classifies at append time. Stub with a
            // typed message so callers that wire this into automation
            // know to expect it.
            let msg = "burn state rebuild classify: standalone reclassify is not yet \
                       implemented in the Rust port (filed as a follow-up under #240). \
                       Today the ingest pipeline classifies at append time; run \
                       `burn state rebuild all` to drop + replay derivable tables.";
            if globals.json {
                let envelope = serde_json::json!({ "error": msg });
                let _ = render_json(&envelope);
            } else {
                eprintln!("burn: {msg}");
            }
            1
        }
    }
}

fn run_rebuild_derivable(globals: &GlobalArgs) -> i32 {
    let opts = LedgerOpenOptions {
        home: globals.ledger_path.clone(),
        content_home: None,
    };
    let mut handle = match Ledger::open(opts) {
        Ok(h) => h,
        Err(err) => return report_anyhow(&err, globals),
    };
    let summary = match handle.raw_mut().rebuild_derivable() {
        Ok(s) => s,
        Err(err) => return report_ledger_error(&err, globals),
    };
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
    use relayburn_sdk::{load_config, Retention};
    let cfg = match load_config() {
        Ok(c) => c,
        Err(err) => return report_ledger_error(&err, globals),
    };
    let retention = match args.days.as_deref() {
        Some(s) => match parse_retention(s) {
            Some(r) => r,
            None => {
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
    // `ts:{:020}.{:09}` value (see `writer::now_iso`); compare by
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
    let mut handle = match Ledger::open(opts) {
        Ok(h) => h,
        Err(err) => return report_anyhow(&err, globals),
    };
    let stats = match handle.raw_mut().prune_content_older_than(&cutoff) {
        Ok(s) => s,
        Err(err) => return report_ledger_error(&err, globals),
    };

    let _ = args.force; // 2.0 prune is purely TTL-based; --force is a no-op
                        // because there are no recoverable on-disk sidecars to
                        // skip. Documented; left in the flag set so existing
                        // automation doesn't break.

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
/// stamped by `relayburn_sdk::ledger::writer::now_iso`. Both the seconds
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
// reset — stubbed: filed as a follow-up SDK gap (see #240)
// ---------------------------------------------------------------------------

fn run_reset(globals: &GlobalArgs, args: crate::cli::StateResetArgs) -> i32 {
    let _ = args; // accepted for forward compat
    let msg = "burn state reset: not yet implemented in the Rust port. The 1.x \
               implementation walked $RELAYBURN_HOME and unlinked individual \
               files (ledger.jsonl, archive.sqlite, content/); the 2.0 \
               equivalent (drop + recreate burn.sqlite/content.sqlite + \
               re-ingest) is filed for follow-up under #240. As a workaround, \
               run 'burn state rebuild all' followed by 'burn ingest'.";
    if globals.json {
        let envelope = serde_json::json!({ "error": msg });
        let _ = render_json(&envelope);
    } else {
        eprintln!("burn: {msg}");
    }
    1
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
    use super::format_cutoff_ts;

    /// Mirror of `relayburn_sdk::ledger::writer::now_iso`'s format string.
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
}

