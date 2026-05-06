//! `burn` — relayburn CLI binary entrypoint.
//!
//! This is the Rust port of `@relayburn/cli`. The clap derive root,
//! subcommand enum, and global flag set live in [`cli`]; per-command
//! presenter logic lives under [`commands`]; shared rendering helpers
//! (table, JSON, typed error reporting) live under [`render`].
//!
//! This file is intentionally tiny: parse argv with clap, dispatch to
//! the subcommand handler, and let `render::error::report_error` do the
//! exit-code mapping for typed SDK errors. Anything else surfaces as a
//! generic `anyhow` error and lands in the same reporter.

mod cli;
mod commands;
mod render;

use clap::Parser;

use crate::cli::{Args, Command};

fn main() {
    let args = Args::parse();
    let exit_code = dispatch(args);
    std::process::exit(exit_code);
}

/// Dispatch the parsed [`Args`] to the matching subcommand handler.
/// Each command stub today returns a non-zero exit code; Wave 2 PRs
/// replace the stubs with real presenters that wrap `relayburn-sdk`
/// calls.
fn dispatch(args: Args) -> i32 {
    let globals = args.globals();
    match args.command {
        Command::Summary => commands::summary::run(&globals),
        Command::Hotspots => commands::hotspots::run(&globals),
        Command::Overhead(args) => commands::overhead::run(&globals, args),
        Command::Compare => commands::compare::run(&globals),
        Command::Run => commands::run::run(&globals),
        Command::State => commands::state::run(&globals),
        Command::Ingest => commands::ingest::run(&globals),
        Command::McpServer => commands::mcp_server::run(&globals),
    }
}
