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

use clap::{Args as ClapArgs, Parser, Subcommand};

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
    Summary,

    /// Surface high-cost / high-overhead hotspots from the ledger.
    Hotspots,

    /// Estimate context overhead and (optionally) trim it.
    Overhead,

    /// Compare cost across two or more models on the same workload.
    Compare,

    /// Run an agent CLI under a harness wrapper that ingests its
    /// session log on exit.
    Run,

    /// Inspect or rebuild derived state under `~/.relayburn`.
    State(StateArgs),

    /// Scan harness session stores and append new turns to the ledger.
    Ingest,

    /// Stdio MCP server exposing read-only ledger queries for
    /// in-session self-query.
    #[command(name = "mcp-server")]
    McpServer,
}

// ---------------------------------------------------------------------------
// `burn state` — typed args + nested subcommand
// ---------------------------------------------------------------------------

/// `burn state [...]` — derived-state inspection / maintenance verbs.
/// Mirrors the TS surface in `packages/cli/src/commands/state.ts`:
///
/// - `burn state status` (default when no subcommand): print the row /
///   file / archive_state report.
/// - `burn state rebuild <target>`: rebuild derivable tables from
///   upstream session files.
/// - `burn state prune`: TTL-based content sidecar prune.
/// - `burn state reset`: wipe derived state and (optionally) re-ingest.
#[derive(Debug, Clone, ClapArgs)]
pub struct StateArgs {
    #[command(subcommand)]
    pub command: Option<StateSubcommand>,
}

/// Nested subcommand for `burn state`. `None` (no positional) is treated
/// as `Status` to match the TS default.
#[derive(Debug, Clone, Subcommand)]
pub enum StateSubcommand {
    /// Print derived-artifact status: file paths, sizes, row counts,
    /// archive-state metadata, resolved retention config.
    Status(StateStatusArgs),

    /// Rebuild derived ledger artifacts from upstream session files.
    Rebuild(StateRebuildArgs),

    /// Prune expired content sidecars below the TTL window.
    Prune(StatePruneArgs),

    /// Wipe derived state under `$RELAYBURN_HOME` (and optionally
    /// re-ingest from upstream session logs).
    Reset(StateResetArgs),
}

/// `burn state status` — flags. `--json` is global and lives on
/// [`Args::json`]; nothing local today, but keep an args struct so
/// future flags (`--minimal`, `--quiet`) land without churning the
/// dispatch sig.
#[derive(Debug, Clone, ClapArgs, Default)]
pub struct StateStatusArgs {}

/// `burn state rebuild` — target + flags. Mirrors the TS surface:
/// `index | classify | content | archive [--full|--vacuum] | all`.
#[derive(Debug, Clone, ClapArgs)]
pub struct StateRebuildArgs {
    #[command(subcommand)]
    pub target: StateRebuildTarget,
}

#[derive(Debug, Clone, Subcommand)]
pub enum StateRebuildTarget {
    /// Rebuild the derivable tables from upstream session logs.
    /// In the 2.0 SQLite layout there is one rebuild path
    /// (`rebuild_derivable`) which drops + replays every derivable
    /// table. The TS subtargets (index / classify / content / archive)
    /// existed because each artifact lived in a separate file; in 2.0
    /// they collapse onto the same SQL transaction.
    Index,
    /// Re-run activity classification on existing turns. Today this
    /// is a no-op stub — the Rust ingest classifier writes the
    /// `activity` field at append time (#274). A standalone reclassify
    /// pass is filed for follow-up.
    Classify(StateRebuildClassifyArgs),
    /// Re-derive content rows from source session files.
    Content,
    /// Apply / rebuild the archive_state metadata.
    Archive(StateRebuildArchiveArgs),
    /// Run content + index + classify + archive in one pass.
    All(StateRebuildAllArgs),
}

#[derive(Debug, Clone, ClapArgs, Default)]
pub struct StateRebuildClassifyArgs {
    /// Force reclassification of every turn even when `activity` is
    /// already populated.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, ClapArgs, Default)]
pub struct StateRebuildArchiveArgs {
    /// Drop archive state and rebuild from zero.
    #[arg(long)]
    pub full: bool,
    /// Reclaim unused SQLite pages after the apply.
    #[arg(long)]
    pub vacuum: bool,
}

#[derive(Debug, Clone, ClapArgs, Default)]
pub struct StateRebuildAllArgs {
    /// Forwarded to `rebuild classify --force` when bundling.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, ClapArgs, Default)]
pub struct StatePruneArgs {
    /// Override the configured retention window. Accepts a number
    /// (days) or the literal `forever`.
    #[arg(long)]
    pub days: Option<String>,
    /// Delete sidecars even when the source session file still exists.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, ClapArgs, Default)]
pub struct StateResetArgs {
    /// Actually delete. Without this flag, reset is a dry-run.
    #[arg(long)]
    pub force: bool,
    /// After a successful `--force` wipe, re-parse all source harness
    /// logs from offset 0.
    #[arg(long)]
    pub reingest: bool,
}
