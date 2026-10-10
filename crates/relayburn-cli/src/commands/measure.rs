//! `burn measure` — one caller-selected session artifact in, one versioned
//! metrics document out. This command deliberately ignores ledger/discovery
//! configuration; it is the runtime-friendly complement to historical views.

use relayburn_sdk::{measure_session, MeasureSessionOptions};

use crate::cli::{GlobalArgs, MeasureArgs};
use crate::render::error::report_error;
use crate::render::format::{format_uint, format_usd};
use crate::render::json::render_json;
use crate::render::stdout::write_stdout;

pub fn run(globals: &GlobalArgs, args: MeasureArgs) -> i32 {
    match run_inner(globals, args) {
        Ok(()) => 0,
        Err(error) => report_error(&error, globals),
    }
}

fn run_inner(globals: &GlobalArgs, args: MeasureArgs) -> anyhow::Result<()> {
    let report = measure_session(MeasureSessionOptions {
        input_path: args.input,
        harness: args.harness.into(),
        pricing_path: args.pricing,
    })?;

    if globals.json {
        render_json(&report)?;
        return Ok(());
    }

    let cost = report
        .cost_usd_micros
        .map(|micros| format_usd(micros as f64 / 1_000_000.0))
        .unwrap_or_else(|| "unknown".to_string());
    let session = report.session_id.as_deref().unwrap_or("not reported");
    write_stdout(&format!(
        "session: {session}\nharness: {}\nturns: {}\ntokens: {}\ncost: {cost}\nmodels: {}\n",
        report.harness,
        format_uint(report.turn_count),
        format_uint(report.usage.total_tokens),
        report.models.len(),
    ))?;
    Ok(())
}
