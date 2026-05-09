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

use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};

/// Parsed top-level argv — what every command handler receives via
/// [`Args::globals`].
#[derive(Debug, Clone)]
pub struct GlobalArgs {
    /// Emit machine-readable JSON instead of human-formatted output.
    /// Read-path commands consult this when picking a renderer; error
    /// reporting also flips to a `{"error": ...}` JSON envelope.
    pub json: bool,
    /// Optional override for the relayburn home directory (the dir
    /// containing `burn.sqlite` + `content.sqlite`). When `None`,
    /// commands fall through to the SDK's env-var / `~/.agentworkforce/burn`
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
    /// or `~/.agentworkforce/burn`.
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
    Overhead(OverheadArgs),

    /// Compare cost across two or more models on the same workload.
    Compare(CompareArgs),

    /// Inspect or rebuild derived state under `~/.agentworkforce/burn`.
    State(StateArgs),

    /// Enumerate sessions in the ledger.
    Sessions(SessionsArgs),

    /// Scan harness session stores and append new turns to the ledger.
    Ingest(IngestArgs),

    /// Stdio MCP server exposing read-only ledger queries for
    /// in-session self-query.
    #[command(name = "mcp-server")]
    McpServer(McpServerArgs),
}

/// Per-command flags for `burn ingest`. Mirrors the TS surface in
/// `packages/cli/src/commands/ingest.ts` so flag muscle memory carries
/// across.
///
/// Three modes, exactly one applies per invocation:
///
/// - No flags: scan all known session stores once and exit.
/// - `--watch` (optionally with `--interval <MS>`): foreground poll loop
///   driven by [`relayburn_sdk::start_watch_loop`].
/// - `--hook <HARNESS> [--quiet]`: stdin-driven hook entrypoint. Today
///   only `--hook claude` is supported; the `--quiet` flag suppresses
///   non-error stderr breadcrumbs so it is safe to call from every
///   Claude Code hook.
///
/// `--watch` and `--hook` are mutually exclusive; the presenter rejects
/// the combination at runtime with exit 2 (matching TS).
#[derive(Debug, Clone, ClapArgs)]
pub struct IngestArgs {
    /// Stay running and poll session stores at `--interval` ms.
    /// Mutually exclusive with `--hook`.
    #[arg(long)]
    pub watch: bool,

    /// Poll interval for `--watch`, in milliseconds. Defaults to 1000.
    /// Ignored without `--watch`.
    #[arg(long, value_name = "MS")]
    pub interval: Option<u64>,

    /// Read a harness-specific hook payload from stdin and ingest the
    /// transcript it references. Today only `claude` is supported.
    /// Mutually exclusive with `--watch`.
    #[arg(long, value_name = "HARNESS")]
    pub hook: Option<String>,

    /// Suppress non-error stderr breadcrumbs. Used by hook callers so
    /// the surrounding tool invocation isn't blocked by a noisy
    /// pipeline. Only meaningful with `--hook`; clap rejects `--quiet`
    /// on its own (or with `--watch`) so a typo can't silently no-op.
    #[arg(long, requires = "hook")]
    pub quiet: bool,
}

/// Per-command flags for `burn mcp-server`. The stdio MCP server speaks
/// JSON-RPC 2.0 line-delimited frames over stdin/stdout and exposes the
/// `burn__sessionCost` read-only tool. Closes #210.
///
/// Global `--ledger-path` (on [`Args`]) is consulted as the SDK ledger
/// home. `--session-id` registers a default session id so MCP clients
/// that omit `sessionId` in `tools/call` get a useful answer (the
/// running agent's own session).
#[derive(Debug, Clone, ClapArgs)]
pub struct McpServerArgs {
    /// Default sessionId to use when `tools/call burn__sessionCost`
    /// omits the argument. Lets the host wrap the server with the
    /// running agent's own session id so the agent can self-query
    /// without knowing it.
    #[arg(long = "session-id", value_name = "ID")]
    pub session_id: Option<String>,

    /// Emit protocol-level diagnostics to stderr. Off by default so a
    /// well-behaved client doesn't see unexpected noise on the channel.
    #[arg(long)]
    pub debug: bool,
}

/// Per-command flag set for `burn compare`. Mirrors
/// `packages/cli/src/commands/compare.ts` so the CLI surfaces match
/// byte-for-byte; see that file for the canonical help text.
///
/// The first positional argument is a comma-separated model list
/// (`claude-sonnet-4-6,claude-haiku-4-5`). The presenter rejects fewer
/// than two distinct models with exit code 2 and a stderr message; this
/// is enforced at runtime rather than by clap so we get the same error
/// message shape as the TS CLI (`burn compare: needs at least 2
/// models...`).
#[derive(Debug, Clone, ClapArgs)]
pub struct CompareArgs {
    /// Comma-separated model list (e.g. `claude-sonnet-4-6,claude-haiku-4-5`).
    /// Required at runtime — see the struct doc comment for the
    /// minimum-models contract.
    #[arg(value_name = "MODELS")]
    pub models: Option<String>,

    /// Comma-separated list of effective providers to include
    /// (e.g. `synthetic,anthropic,openai`).
    #[arg(long, value_name = "LIST")]
    pub provider: Option<String>,

    /// Relative range (e.g. `24h`, `7d`, `4w`) or ISO timestamp.
    /// Defaults to all time.
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// Filter by project path or git-canonical projectKey.
    #[arg(long, value_name = "PATH")]
    pub project: Option<String>,

    /// Filter by sessionId.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,

    /// Filter by stamped workflowId.
    #[arg(long, value_name = "ID")]
    pub workflow: Option<String>,

    /// Filter by stamped agentId.
    #[arg(long, value_name = "ID")]
    pub agent: Option<String>,

    /// Insufficient-sample threshold; cells below this get flagged in
    /// the coverage-notes block. Default 5.
    #[arg(long = "min-sample", value_name = "N")]
    pub min_sample: Option<u64>,

    /// Minimum fidelity class to include
    /// (`full | usage-only | aggregate-only | cost-only | partial`).
    /// Default `usage-only`.
    #[arg(long, value_name = "CLASS")]
    pub fidelity: Option<String>,

    /// Shorthand for `--fidelity partial`.
    #[arg(long = "include-partial")]
    pub include_partial: bool,

    /// Emit a stable CSV with one row per (model, category) pair.
    #[arg(long)]
    pub csv: bool,

    /// Bypass the SQLite archive and stream the ledger directly.
    /// Honored when env `RELAYBURN_ARCHIVE=0`.
    #[arg(long = "no-archive")]
    pub no_archive: bool,
}

/// `burn overhead [trim]` argument set. The top-level form takes the
/// shared `--project` / `--since` / `--kind` flags; the optional
/// `trim` subcommand layers on `--top` for recommendation count.
#[derive(Debug, ClapArgs)]
pub struct OverheadArgs {
    /// Project root to scan for overhead files (CLAUDE.md, .claude/CLAUDE.md,
    /// AGENTS.md). Defaults to the current working directory.
    #[arg(long, value_name = "PATH", global = true)]
    pub project: Option<PathBuf>,

    /// Time window to attribute over: a relative range (`24h`, `7d`,
    /// `4w`, `2m`) or an ISO timestamp. Defaults to all time.
    #[arg(long, value_name = "RANGE", global = true)]
    pub since: Option<String>,

    /// Narrow to a single overhead-file kind.
    #[arg(long, value_enum, value_name = "KIND", global = true)]
    pub kind: Option<OverheadKind>,

    #[command(subcommand)]
    pub action: Option<OverheadAction>,
}

/// CLI-facing mirror of [`relayburn_sdk::OverheadFileKind`]. Lives here
/// so the SDK enum doesn't have to take a `clap` dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OverheadKind {
    #[value(name = "claude-md")]
    ClaudeMd,
    #[value(name = "agents-md")]
    AgentsMd,
}

impl From<OverheadKind> for relayburn_sdk::OverheadFileKind {
    fn from(k: OverheadKind) -> Self {
        match k {
            OverheadKind::ClaudeMd => relayburn_sdk::OverheadFileKind::ClaudeMd,
            OverheadKind::AgentsMd => relayburn_sdk::OverheadFileKind::AgentsMd,
        }
    }
}

/// `burn overhead <action>`.
#[derive(Debug, Subcommand)]
pub enum OverheadAction {
    /// Surface trim recommendations for the highest-cost sections of
    /// each overhead file. Recommendations only — `burn` never
    /// modifies the source files.
    Trim(OverheadTrimArgs),
}

/// `burn overhead trim` flags layered on top of [`OverheadArgs`].
#[derive(Debug, ClapArgs)]
pub struct OverheadTrimArgs {
    /// Number of recommendations per file. Defaults to 3.
    #[arg(long, value_name = "N")]
    pub top: Option<u64>,
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
    /// Drop every derivable table and stage them for re-ingest. In 2.0
    /// classification happens at ingest time (see
    /// `reader/classifier.rs`), so a standalone classify-only replay
    /// would be a no-op against an unchanged corpus. This target runs
    /// the same full `rebuild_derivable` drop-and-rebuild path as the
    /// other targets; follow with `burn ingest` to repopulate the
    /// derivable tables with fresh classifications.
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
    /// Accepted for 1.x script compatibility. In 2.0 classification
    /// runs at ingest time, so --force is a no-op (an advisory prints
    /// when set).
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, ClapArgs, Default)]
pub struct StateRebuildArchiveArgs {
    /// Legacy positional from the TS CLI: `burn state rebuild archive
    /// vacuum`. Equivalent to `--vacuum`; kept so existing scripts that
    /// target the 1.x surface keep parsing. In 2.0 there is no separate
    /// archive.sqlite to vacuum, so this is a no-op (advisory prints).
    #[arg(value_name = "ACTION")]
    pub action: Option<ArchiveAction>,
    /// 1.x-compat flag: drop archive state and rebuild from zero. In 2.0
    /// every rebuild already replays from zero, so this is a no-op
    /// (advisory prints when set).
    #[arg(long)]
    pub full: bool,
    /// 1.x-compat flag: reclaim unused SQLite pages after the apply. In
    /// 2.0 archive_state lives inside burn.sqlite, so there is nothing
    /// to vacuum; no-op (advisory prints when set).
    #[arg(long)]
    pub vacuum: bool,
}

/// Legacy positional action for `burn state rebuild archive`. Today
/// `vacuum` is the only accepted value; both the positional and
/// `--vacuum` flag route through the same `rebuild_derivable` path
/// in 2.0 (there's no separate `archive.sqlite` to vacuum), but the
/// surface stays so 1.x automation doesn't error out.
#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ArchiveAction {
    Vacuum,
}

#[derive(Debug, Clone, ClapArgs, Default)]
pub struct StateRebuildAllArgs {
    /// 1.x-compat flag: in 1.x this forwarded to `rebuild classify
    /// --force`. In 2.0 classification happens at ingest time and
    /// rebuild collapses onto a single transaction, so --force is a
    /// no-op (advisory prints when set).
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, ClapArgs, Default)]
pub struct StatePruneArgs {
    /// Override the configured retention window. Accepts a number
    /// (days) or the literal `forever`.
    #[arg(long)]
    pub days: Option<String>,
    /// 1.x-compat flag: in 1.x this skipped the "is the source session
    /// file still present?" guard before unlinking content sidecars. In
    /// 2.0 prune is purely TTL-based against `content.sqlite` (no
    /// recoverable on-disk sidecars to skip), so --force is a no-op
    /// (advisory prints when set).
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, ClapArgs, Default)]
pub struct StateResetArgs {
    /// Actually delete. Without this flag, reset is a dry-run.
    #[arg(long)]
    pub force: bool,
    /// After a successful `--force` wipe, re-parse all source harness
    /// logs from offset 0. Only meaningful with `--force`; clap rejects
    /// `--reingest` on its own so a typo can't silently no-op.
    #[arg(long, requires = "force")]
    pub reingest: bool,
}

// ---------------------------------------------------------------------------
// `burn sessions` — typed args + nested subcommand
// ---------------------------------------------------------------------------

/// `burn sessions [...]` — session enumeration verbs.
///
/// Today the only nested verb is `list`, which prints recent sessions
/// most-recent first so callers can find a session id to feed into
/// `burn summary --session <id>` / `burn hotspots --session <id>`.
/// The args struct exists so future verbs (`show`, `tag`, …) can land
/// without churning the dispatcher.
#[derive(Debug, Clone, ClapArgs)]
pub struct SessionsArgs {
    #[command(subcommand)]
    pub command: SessionsSubcommand,
}

/// Nested subcommand for `burn sessions`. Required (no positional default)
/// — the subcommand surface is small enough that `burn sessions` on its
/// own is more confusing than `burn sessions list` would be helpful.
#[derive(Debug, Clone, Subcommand)]
pub enum SessionsSubcommand {
    /// Print a table of recent sessions (most-recent first).
    List(SessionsListArgs),
}

/// `burn sessions list` — flags. `--json`, `--ledger-path`, `--no-color`
/// are inherited via [`Args`].
#[derive(Debug, Clone, ClapArgs)]
pub struct SessionsListArgs {
    /// Slice the ledger to events at or after `<since>`. Accepts either an
    /// ISO timestamp or a relative range (`24h`, `7d`, `4w`, `2m`).
    /// Defaults to `7d` so the table is bounded for a typical "what did
    /// I run recently" lookup; pass an explicit value (e.g. `--since 30d`)
    /// to widen the window.
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// Restrict to a single project (matches `project` or `projectKey`).
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<String>,

    /// Case-insensitive substring filter. Matched against `session_id` and
    /// the resolved project label.
    #[arg(long, value_name = "PATTERN")]
    pub grep: Option<String>,

    /// Row cap. Defaults to 20.
    #[arg(long, value_name = "N")]
    pub limit: Option<u64>,
}
