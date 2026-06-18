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
mod selfupdate;

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
    // On-launch self-update offer. Skipped for `update` itself (it owns
    // the upgrade flow) and `mcp-server` (a stdio protocol channel that a
    // prompt would corrupt). The check is otherwise best-effort and
    // silently no-ops in non-interactive / `--json` / disabled cases — see
    // `selfupdate::maybe_offer_update`. On accept it installs and re-execs,
    // so it may not return.
    if offer_update_for(&args.command) {
        selfupdate::maybe_offer_update(&globals);
    }
    match args.command {
        Command::Summary(sub) => commands::summary::run(&globals, sub),
        Command::Hotspots(sub) => commands::hotspots::run(&globals, sub),
        Command::Overhead(args) => commands::overhead::run(&globals, args),
        Command::Compare(args) => commands::compare::run(&globals, args),
        Command::State(args) => commands::state::run(&globals, args),
        Command::Sessions(args) => commands::sessions::run(&globals, args),
        Command::Flow(args) => commands::flow::run(&globals, args),
        Command::Stamps(args) => commands::stamps::run(&globals, args),
        Command::Ingest(args) => commands::ingest::run(&globals, args),
        Command::Sync(args) => commands::sync::run(&globals, args),
        Command::McpServer(args) => commands::mcp_server::run(&globals, args),
        Command::Update(args) => commands::update::run(&globals, args),
    }
}

/// Whether the on-launch update check should run for this command. The
/// `update` command drives upgrades itself, and `mcp-server` speaks a
/// machine protocol on stdio where an interactive prompt has no place.
fn offer_update_for(command: &Command) -> bool {
    !matches!(command, Command::Update(_) | Command::McpServer(_))
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Summary(_) => "summary",
        Command::Hotspots(_) => "hotspots",
        Command::Overhead(_) => "overhead",
        Command::Compare(_) => "compare",
        Command::State(_) => "state",
        Command::Sessions(_) => "sessions",
        Command::Flow(_) => "flow",
        Command::Stamps(_) => "stamps",
        Command::Ingest(_) => "ingest",
        Command::Sync(_) => "sync",
        Command::McpServer(_) => "mcp-server",
        Command::Update(_) => "update",
    }
}
