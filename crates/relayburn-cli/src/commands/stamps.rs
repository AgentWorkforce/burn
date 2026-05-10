//! `burn stamps export` — export stamp records as JSONL for backup / version-control.
//!
//! Thin presenter over `relayburn_sdk::export_stamps`. Streams each stamp
//! as a JSONL line to stdout (with `--out -`, the default) or to a file
//! (with `--out <path>`). The record format is stable:
//!
//! ```jsonl
//! {"v":1,"kind":"stamp","ts":"2025-01-01T00:00:00Z","selector":{"sessionId":"..."},"enrichment":{"role":"..."}}
//! ```

use relayburn_sdk::{ExportStampsOptions, Ledger, LedgerOpenOptions};

use crate::cli::{GlobalArgs, StampsArgs};
use crate::render::error::report_error;
use crate::render::progress::TaskProgress;

/// Default output is stdout ("-")
const DEFAULT_OUT: &str = "-";

pub fn run(globals: &GlobalArgs, args: StampsArgs) -> i32 {
    match args.command {
        crate::cli::StampsSubcommand::Export(export_args) => run_export(globals, export_args),
    }
}

fn run_export(globals: &GlobalArgs, args: crate::cli::StampsExportArgs) -> i32 {
    let progress = TaskProgress::new(globals, "stamps export");

    // Open the ledger
    let opts = LedgerOpenOptions {
        home: globals.ledger_path.clone(),
        content_home: None,
    };
    progress.set_task("opening ledger");
    let handle = match Ledger::open(opts) {
        Ok(h) => h,
        Err(err) => {
            progress.finish_and_clear();
            return report_error(&err, globals);
        }
    };

    // Export stamps
    progress.set_task("exporting stamps");
    let values: Vec<serde_json::Value> = match handle
        .export_stamps(ExportStampsOptions::default())
    {
        Ok(iter) => iter.collect(),
        Err(err) => {
            progress.finish_and_clear();
            return report_error(&err, globals);
        }
    };
    progress.finish_and_clear();

    // Determine output destination
    let out_path = args.out.as_deref().unwrap_or(DEFAULT_OUT);

    // Serialize output
    let stamp_count = values.len();
    let mut output = String::new();
    for val in &values {
        if let Ok(line) = serde_json::to_string(val) {
            output.push_str(&line);
            output.push('\n');
        }
    }

    // Write to stdout or file
    if out_path == "-" {
        print!("{}", output);
        0
    } else {
        match std::fs::write(out_path, output) {
            Ok(()) => {
                eprintln!("Exported {} stamp(s) to {}", stamp_count, out_path);
                0
            }
            Err(err) => {
                let err = anyhow::anyhow!("failed to write output file: {}", err);
                report_error(&err, globals)
            }
        }
    }
}
