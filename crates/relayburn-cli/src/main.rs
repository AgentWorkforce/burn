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
    crate::render::logging::init(&globals);
    tracing::debug!(
        command = command_name(&args.command),
        json = globals.json,
        no_color = globals.no_color,
        "dispatching command"
    );
    match args.command {
        Command::Summary(sub) => commands::summary::run(&globals, sub),
        Command::Hotspots(sub) => commands::hotspots::run(&globals, sub),
        Command::Overhead(args) => commands::overhead::run(&globals, args),
        Command::Compare(args) => commands::compare::run(&globals, args),
        Command::State(args) => commands::state::run(&globals, args),
        Command::Sessions(args) => commands::sessions::run(&globals, args),
        Command::Ingest(args) => commands::ingest::run(&globals, args),
        Command::McpServer(args) => commands::mcp_server::run(&globals, args),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Summary(_) => "summary",
        Command::Hotspots(_) => "hotspots",
        Command::Overhead(_) => "overhead",
        Command::Compare(_) => "compare",
        Command::State(_) => "state",
        Command::Sessions(_) => "sessions",
        Command::Ingest(_) => "ingest",
        Command::McpServer(_) => "mcp-server",
    }
}
