//! `burn stamps export` — export stamp records as JSONL for backup / version-control.
//!
//! Thin presenter over `relayburn_sdk::export_stamps`. Streams each stamp
//! as a JSONL line to stdout (with `--out -`, the default) or to a file
//! (with `--out <path>`). The record format is stable:
//!
//! ```jsonl
//! {"v":1,"kind":"stamp","ts":"2025-01-01T00:00:00Z","selector":{"sessionId":"..."},"enrichment":{"role":"..."}}
//! ```

use std::fs::File;
use std::io::{self, BufWriter, Write};

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

    progress.set_task("exporting stamps");
    let iter = match handle.export_stamps(ExportStampsOptions::default()) {
        Ok(iter) => iter,
        Err(err) => {
            progress.finish_and_clear();
            return report_error(&err, globals);
        }
    };

    let out_path = args.out.as_deref().unwrap_or(DEFAULT_OUT);
    let result = if out_path == "-" {
        let stdout = io::stdout();
        write_jsonl(&mut BufWriter::new(stdout.lock()), iter)
    } else {
        match File::create(out_path) {
            Ok(file) => write_jsonl(&mut BufWriter::new(file), iter),
            Err(err) => Err(anyhow::anyhow!("failed to open output file: {}", err)),
        }
    };
    progress.finish_and_clear();

    match result {
        Ok(stamp_count) => {
            if out_path != "-" {
                eprintln!("Exported {} stamp(s) to {}", stamp_count, out_path);
            }
            0
        }
        Err(err) => report_error(&err, globals),
    }
}

/// Serialize each stamp directly to `writer` as a JSONL line. Returns the
/// number of records written. Any serialization or I/O error aborts the
/// export and is propagated — partial files are surfaced as failures so a
/// `stamps export` user never silently loses records.
fn write_jsonl<W: Write, I: IntoIterator<Item = serde_json::Value>>(
    writer: &mut W,
    iter: I,
) -> anyhow::Result<usize> {
    let mut count: usize = 0;
    for val in iter {
        serde_json::to_writer(&mut *writer, &val)
            .map_err(|err| anyhow::anyhow!("failed to serialize stamp: {}", err))?;
        writer
            .write_all(b"\n")
            .map_err(|err| anyhow::anyhow!("failed to write stamp: {}", err))?;
        count += 1;
    }
    writer
        .flush()
        .map_err(|err| anyhow::anyhow!("failed to flush output: {}", err))?;
    Ok(count)
}
