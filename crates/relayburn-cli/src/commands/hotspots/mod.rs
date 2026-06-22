//! `burn hotspots` — surface high-cost / high-overhead hotspots from
//! the ledger.
//!
//! Thin presenter over [`relayburn_sdk::hotspots`]. Mirrors
//! `packages/cli/src/commands/hotspots.ts` for the default attribution
//! flow that drives the golden snapshots; the broader TS surface
//! (`--patterns`, `--findings`, `--session` per-session view,
//! `--provider`, `--workflow`) is enumerated as flag wiring + a
//! stub-mode error path.
//!
//! ## Wiring
//!
//! 1. Open a [`relayburn_sdk::LedgerHandle`] honoring `--ledger-path` /
//!    `RELAYBURN_HOME`.
//! 2. Run [`relayburn_sdk::ingest_all`] with TTY-only progress.
//! 3. Call [`relayburn_sdk::hotspots`] (verb-form) with the resolved
//!    [`relayburn_sdk::HotspotsOptions`]. The SDK enforces the coverage
//!    gate, picks the `Sized` vs `EvenSplit` attribution method per
//!    session, and emits the discriminated union; for the default flow
//!    we expect [`relayburn_sdk::HotspotsResult::Attribution`] and
//!    unwrap that branch.
//! 4. Render JSON or human format. JSON output drops the `kind`
//!    discriminator and emits the inner `HotspotsAttributionResult`
//!    shape directly (TS contract).

use clap::{Args, ValueEnum};
use relayburn_sdk::{
    hotspots as sdk_hotspots, ingest_all, HotspotsGroupBy, HotspotsOptions, Ledger,
    LedgerOpenOptions,
};

use crate::cli::GlobalArgs;
use crate::render::error::report_error;
use crate::render::progress::TaskProgress;

mod human;
mod json;

use human::*;
use json::*;

const DEFAULT_TOP_N: usize = 10;

/// Per-command flags for `burn hotspots`. Mirrors the TS surface in
/// `packages/cli/src/commands/hotspots.ts`.
#[derive(Debug, Clone, Args)]
pub struct HotspotsArgs {
    /// Slice the ledger to events at or after `<since>`. ISO timestamp
    /// or relative range (`24h`, `7d`, `4w`, `2m`).
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// Restrict to a single project.
    #[arg(long, value_name = "PROJECT")]
    pub project: Option<String>,

    /// Restrict to a single session id (or pass without a value to drop
    /// into the per-session attribution view).
    #[arg(long, value_name = "SESSION_ID", num_args = 0..=1, default_missing_value = "")]
    pub session: Option<String>,

    /// Filter by enrichment workflow id.
    #[arg(long, value_name = "WORKFLOW_ID")]
    pub workflow: Option<String>,

    /// Provider filter (CSV of provider names; case-insensitive).
    #[arg(long, value_name = "PROVIDERS")]
    pub provider: Option<String>,

    /// Show all rows in human mode instead of capping at the default
    /// top-N (10).
    #[arg(long)]
    pub all: bool,

    /// Group by a single dimension. Defaults to the full attribution
    /// view; pass `bash`, `bash-verb`, `file`, or `subagent` to focus
    /// a single rollup.
    #[arg(long = "group-by", value_name = "DIM")]
    pub group_by: Option<String>,

    /// Comma-separated waste-pattern detectors to run instead of the
    /// attribution view. Pass without a value to enable every detector.
    #[arg(long, value_name = "PATTERNS", num_args = 0..=1, default_missing_value = "")]
    pub patterns: Option<String>,

    /// Render the unified `findings` view rather than the per-detector
    /// summary. Implies `--patterns` if it isn't already set.
    #[arg(long)]
    pub findings: bool,

    /// Surface session relationship drift on top of the default attribution
    /// view. Currently a stub in the Rust port — the relationship drift
    /// query verb is not yet exposed by the SDK.
    #[arg(long = "explain-drift")]
    pub explain_drift: bool,

    /// Ranking dimension for the per-tool tables (files, bash, bash verbs,
    /// subagents). `cost` (default) keeps the historical USD-descending
    /// order; `bytes` sorts by `totalOutputBytes` so blowouts that get
    /// truncated to ~0 tokens still surface (#436). JSON output is
    /// unaffected — both rankings ship every field; downstream consumers
    /// pick their own sort.
    #[arg(long = "rank-by", value_name = "DIM", value_enum, default_value_t = RankBy::Cost)]
    pub rank_by: RankBy,

    /// Run a pre-query ingest sweep so hotspots reflects freshly appended
    /// sessions. Off by default: `hotspots` is a read verb, and a full-store
    /// sweep re-stats every session file under every harness store — seconds
    /// on a large ledger. Keep the ledger current out of band with
    /// `burn ingest --watch` (or the Claude Stop hook); pass `--ingest` only
    /// for a one-off freshen. Mirrors `burn summary --ingest`.
    #[arg(long = "ingest")]
    pub ingest: bool,
}

/// Sort dimension for the per-tool human-mode tables. Mirrors `--rank-by`.
/// `ValueEnum` so clap validates at parse time — invalid values fail before
/// any ingest work runs, in both human and `--json` modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RankBy {
    Cost,
    Bytes,
}

// Detector kinds the SDK's `run_hotspots_findings` filter expects. These are
// the *finding* kind strings emitted by `WasteFinding.kind` (e.g.
// `compaction-loss`, `skill-recall-dup`), NOT the detector-input names. The
// SDK matches `wanted_set.contains(&f.kind)` for pattern-derived findings and
// also keys `tool-output-bloat` / `ghost-surface` / `tool-call-pattern` off
// the same set, so this list has to use the finding-kind spelling on every
// row.
const PATTERN_KINDS: &[&str] = &[
    "retry-loop",
    "failure-run",
    "cancellation-run",
    "compaction-loss",
    "edit-revert",
    "edit-heavy",
    "skill-recall-dup",
    "skill-pruning-protection",
    "system-prompt-tax",
    "ghost-surface",
    "tool-output-bloat",
    "tool-call-pattern",
];

fn resolve_pattern_selection(raw: &str) -> Result<Vec<String>, String> {
    if raw.is_empty() {
        return Ok(PATTERN_KINDS.iter().map(|s| (*s).to_string()).collect());
    }
    let mut out: Vec<String> = Vec::new();
    for piece in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !PATTERN_KINDS.contains(&piece) {
            return Err(format!(
                "unknown --patterns value \"{}\". Valid: {}",
                piece,
                PATTERN_KINDS.join(", ")
            ));
        }
        if !out.iter().any(|s| s == piece) {
            out.push(piece.to_string());
        }
    }
    if out.is_empty() {
        return Ok(PATTERN_KINDS.iter().map(|s| (*s).to_string()).collect());
    }
    Ok(out)
}

pub fn run(globals: &GlobalArgs, args: HotspotsArgs) -> i32 {
    match run_inner(globals, args) {
        Ok(code) => code,
        Err(err) => report_error(&err, globals),
    }
}

fn run_inner(globals: &GlobalArgs, args: HotspotsArgs) -> anyhow::Result<i32> {
    // The TS surface treats `--session` (no value) as "drop into the
    // per-session aggregate / gap report." That view weaves session
    // relationships, tool-result chronology, and per-session attribution —
    // none of which the SDK exposes yet. Keep it a clear stub.
    if matches!(args.session.as_deref(), Some("")) {
        eprintln!(
            "burn: per-session aggregate view (`--session` with no id) is not yet implemented in the Rust port. Pass a session id to filter the standard hotspots view."
        );
        return Ok(2);
    }
    if args.explain_drift {
        eprintln!(
            "burn: --explain-drift is not yet implemented in the Rust port (relationship-drift query verb hasn't landed in relayburn-sdk yet)."
        );
        return Ok(2);
    }

    // `--findings` standalone means "render findings unified view"; pin it
    // to `--patterns` (all detectors) so the resolver below sees a value.
    let patterns_arg: Option<&str> = if args.patterns.is_some() {
        args.patterns.as_deref()
    } else if args.findings {
        Some("")
    } else {
        None
    };
    let patterns_selection: Option<Vec<String>> = match patterns_arg {
        None => None,
        Some(raw) => match resolve_pattern_selection(raw) {
            Ok(sel) => Some(sel),
            Err(msg) => {
                eprintln!("burn: {msg}");
                return Ok(2);
            }
        },
    };

    let group_by = match args.group_by.as_deref() {
        None => None,
        Some("attribution") => Some(HotspotsGroupBy::Attribution),
        Some("bash") => Some(HotspotsGroupBy::Bash),
        Some("bash-verb") => Some(HotspotsGroupBy::BashVerb),
        Some("file") => Some(HotspotsGroupBy::File),
        Some("subagent") => Some(HotspotsGroupBy::Subagent),
        Some(other) => {
            eprintln!(
                "burn: unknown --group-by value \"{}\". Valid: attribution, bash, bash-verb, file, subagent",
                other
            );
            return Ok(2);
        }
    };

    if group_by.is_some() && patterns_selection.is_some() {
        eprintln!(
            "burn: --group-by and --patterns/--findings are mutually exclusive (group-by selects an attribution rollup; patterns/findings drive the detector view)."
        );
        return Ok(2);
    }

    let provider_filter: Option<Vec<String>> = args.provider.as_deref().and_then(|raw| {
        let parts: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        (!parts.is_empty()).then_some(parts)
    });

    // Open + ingest. We open the handle locally so ingest sees the same
    // sealed `RELAYBURN_HOME` the verb call does.
    let ledger_home = globals.ledger_path.clone();
    let progress = TaskProgress::new(globals, "hotspots");
    let opts = match &ledger_home {
        Some(h) => LedgerOpenOptions::with_home(h),
        None => LedgerOpenOptions::default(),
    };
    progress.set_task("opening ledger");
    let mut handle = Ledger::open(opts)?;

    // Read-verb default: skip the pre-query sweep (see `burn summary` /
    // `burn compare`). `--ingest` opts back into a one-off freshen; otherwise
    // go straight to the query and let `burn ingest --watch` / the Claude Stop
    // hook keep the ledger current out of band.
    if args.ingest {
        progress.set_task("refreshing ledger");
        let raw_opts = progress.ingest_options(ledger_home.clone());
        ingest_all(handle.raw_mut(), &raw_opts)?;
    }
    drop(handle);

    let session_filter = match args.session.as_deref() {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    };

    progress.set_task("analyzing hotspots");
    let result = sdk_hotspots(HotspotsOptions {
        session: session_filter,
        project: args.project.clone(),
        since: args.since.clone(),
        group_by,
        patterns: patterns_selection,
        workflow: args.workflow.clone(),
        provider: provider_filter,
        ledger_home,
    })?;
    progress.finish_and_clear();

    if globals.json {
        emit_json(&result)?;
        return Ok(0);
    }
    let limit = if args.all { usize::MAX } else { DEFAULT_TOP_N };
    emit_human(&result, limit, args.findings, args.rank_by);
    Ok(0)
}
