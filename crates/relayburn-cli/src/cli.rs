//! Top-level clap derive root for the `burn` binary.
//!
//! Mirrors the global flag set of the TypeScript CLI (`packages/cli`):
//!
//! - `--json` toggles structured-output mode; honored by every read-path
//!   command via [`render::json::render_json`](crate::render::json::render_json).
//! - `--ledger-path <PATH>` overrides the resolved `RELAYBURN_HOME`
//!   directory for this invocation. Per-command handlers translate this
//!   into a `relayburn_sdk::LedgerOpenOptions::with_home(...)`.
//! - `--no-color` disables ANSI escape sequences in human-rendered
//!   output. Wave 2 commands branch on this when calling into the table
//!   renderer / colorized status output.
//!
//! Per-command flags (e.g. `--since`, `--by-tool`, `--top`) are NOT
//! defined here — they live on the individual `*Args` structs that the
//! Wave 2 fan-out PRs add to each `commands/*.rs`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Parsed top-level argv — what every command handler receives via
/// [`Args::globals`].
//
// `ledger_path` and `no_color` are unused on this branch because the
// command stubs don't read them yet; Wave 2 presenter PRs are what
// actually consume them. Suppress the resulting dead-code warnings
// without losing the field on the struct.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct GlobalArgs {
    /// Emit machine-readable JSON instead of human-formatted output.
    /// Read-path commands consult this when picking a renderer; error
    /// reporting also flips to a `{"error": ...}` JSON envelope.
    pub json: bool,
    /// Optional override for the relayburn home directory (the dir
    /// containing `burn.sqlite` + `content.sqlite`). When `None`,
    /// commands fall through to the SDK's env-var / `~/.relayburn`
    /// resolution.
    pub ledger_path: Option<PathBuf>,
    /// Suppress ANSI color output. Honored by the table renderer and
    /// any human-formatted status messages.
    pub no_color: bool,
}

/// `burn` — token usage & cost attribution for agent CLIs.
#[derive(Debug, Parser)]
#[command(
    name = "burn",
    bin_name = "burn",
    about = "token usage & cost attribution for agent CLIs",
    long_about = None,
    version,
    propagate_version = true,
    // The TS CLI emits its own help block; clap's auto-generated one is
    // close enough for the scaffold and is what every Wave 2 PR will
    // extend with per-command flag docs.
    disable_help_subcommand = false,
)]
pub struct Args {
    /// Emit machine-readable JSON instead of human-formatted output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Override the relayburn home directory (the dir containing
    /// `burn.sqlite` + `content.sqlite`). Defaults to `$RELAYBURN_HOME`
    /// or `~/.relayburn`.
    #[arg(long, global = true, value_name = "PATH")]
    pub ledger_path: Option<PathBuf>,

    /// Disable ANSI color output.
    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Command,
}

impl Args {
    /// Bundle the global flags into a single struct passed to every
    /// command handler. Cheap clone — three small fields.
    pub fn globals(&self) -> GlobalArgs {
        GlobalArgs {
            json: self.json,
            ledger_path: self.ledger_path.clone(),
            no_color: self.no_color,
        }
    }
}

/// Top-level subcommand enum. One variant per binary subcommand. The
/// Wave 2 PRs replace each unit variant with a fully-typed `*Args`
/// struct (`Summary(SummaryArgs)`, etc.) once the per-command flag set
/// is wired up; until then, every variant is a stub that prints a
/// "not yet implemented" message and exits 1.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Aggregate session usage and cost.
    Summary(crate::commands::summary::SummaryArgs),

    /// Surface high-cost / high-overhead hotspots from the ledger.
    Hotspots(crate::commands::hotspots::HotspotsArgs),

    /// Estimate context overhead and (optionally) trim it.
    Overhead,

    /// Compare cost across two or more models on the same workload.
    Compare,

    /// Run an agent CLI under a harness wrapper that ingests its
    /// session log on exit.
    Run,

    /// Inspect or rebuild derived state under `~/.relayburn`.
    State,

    /// Scan harness session stores and append new turns to the ledger.
    Ingest,

    /// Stdio MCP server exposing read-only ledger queries for
    /// in-session self-query.
    #[command(name = "mcp-server")]
    McpServer,
}
