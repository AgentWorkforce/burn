//! `burn run <harness>` — wrapper that spawns an agent CLI under a
//! `HarnessAdapter` and ingests its session log on exit.
//!
//! Mirrors `packages/cli/src/commands/run.ts`. Lifecycle:
//!
//! 1. Resolve the named adapter from
//!    [`relayburn_cli::harnesses::lookup`]. Unknown name → typed error
//!    listing the known set.
//! 2. Build a [`relayburn_cli::harnesses::PlanCtx`] from `cwd`,
//!    `passthrough`, and the merged `--tag` / `RELAYBURN_*` enrichment.
//! 3. `adapter.plan(&ctx).await` → [`relayburn_cli::harnesses::SpawnPlan`].
//! 4. `adapter.before_spawn(&ctx, &plan).await` — claude stamps now;
//!    pending-stamp adapters drop a manifest the post-exit pass resolves.
//! 5. Optional `adapter.start_watcher(&ctx, sink)` — claude returns
//!    `None`; codex/opencode (D6) drain their session store while the
//!    child runs. Reports flow into the same accumulator as `after_exit`.
//! 6. Spawn the child. `stdio: 'inherit'` mirrors the TS sibling.
//! 7. Wait for exit. The driver is **transparent** — the user-visible
//!    exit code is the child's; relayburn's own ingest failures fall
//!    through `report_error`.
//! 8. Stop the watcher (if any), run `adapter.after_exit(&ctx, &plan).await`,
//!    fold both reports into a single
//!    `[burn] <name> ingest: N session(s) (+M turns)` line on stderr.
//!
//! The driver is async so adapter calls can stay async; we drive it on a
//! current-thread tokio runtime, the same pattern the D1 summary
//! presenter uses for `ingest_all`. Process spawn goes through
//! `tokio::process::Command::status().await` so the watcher can tick
//! while the child is alive — `std::process::Command::status()` would
//! synchronously block the only thread on the current-thread runtime.

use std::collections::BTreeMap;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};

use relayburn_cli::harnesses::{list_harness_names, lookup, HarnessAdapter, PlanCtx};
use relayburn_cli::util::time::iso_from_system_time;
use relayburn_sdk::{Enrichment, IngestReport, ReportSink};
use tokio::process::Command as TokioCommand;

use crate::cli::{GlobalArgs, RunArgs};
use crate::render::error::report_error;

/// Spawner-owned tagging contract. Mirrors `SPAWN_ENV_TAG_KEYS` in
/// `packages/cli/src/spawn-tags.ts` byte-for-byte. Keep this in lockstep
/// with the TS sibling — orchestrators thread the same env vars across.
const SPAWN_ENV_TAG_KEYS: &[(&str, &str)] = &[
    ("RELAYBURN_WORKFLOW_ID", "workflowId"),
    ("RELAYBURN_STEP_ID", "stepId"),
    ("RELAYBURN_AGENT_ID", "agentId"),
    ("RELAYBURN_PARENT_AGENT_ID", "parentAgentId"),
    ("RELAYBURN_PERSONA", "persona"),
    ("RELAYBURN_TIER", "tier"),
];

const RUN_HELP_PREFIX: &str = "burn run — spawn an agent harness with attribution\n\n\
Usage:\n  burn run <harness> [--tag k=v ...] [-- <harness args>]\n\n";

const RUN_HELP_EXAMPLES: &str = "\nExamples:\n  \
burn run claude   --tag workflow=refactor -- --resume\n  \
burn run codex    --tag workflow=refactor\n  \
burn run opencode --tag workflow=refactor\n";

pub fn run(globals: &GlobalArgs, args: RunArgs) -> i32 {
    match run_inner(globals, args) {
        Ok(code) => code,
        Err(err) => report_error(&err, globals),
    }
}

fn run_inner(globals: &GlobalArgs, args: RunArgs) -> anyhow::Result<i32> {
    // No harness positional → print help + exit 2 (TS sibling does the
    // same; clap won't trigger this for `burn run --help` because clap's
    // built-in help short-circuits the dispatch entirely with exit 0).
    let harness_name = match args.harness.as_deref() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => {
            print_run_help();
            return Ok(2);
        }
    };

    let adapter = match lookup(&harness_name) {
        Some(a) => a,
        None => {
            let known = list_harness_names().join(", ");
            return Err(anyhow::anyhow!(
                "unknown harness \"{harness_name}\". Known: {known}"
            ));
        }
    };

    let tags = build_enrichment(&args.tag)?;

    // `--ledger-path` is honored by setting RELAYBURN_HOME for the rest
    // of this process. The adapter's `before_spawn`/`after_exit` open
    // their own `Ledger` via env-var fallback, and the spawned child
    // inherits the same value. Mirrors how summary.rs threads
    // `globals.ledger_path` into `LedgerOpenOptions::with_home`, but
    // works through env so adapter calls see it.
    if let Some(p) = globals.ledger_path.as_deref() {
        std::env::set_var("RELAYBURN_HOME", p);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(drive(globals, adapter, args.passthrough, tags))
}

fn print_run_help() {
    let mut s = String::new();
    s.push_str(RUN_HELP_PREFIX);
    s.push_str("Known harnesses: ");
    s.push_str(&list_harness_names().join(", "));
    s.push('\n');
    s.push_str(RUN_HELP_EXAMPLES);
    print!("{s}");
}

/// Async core. Owns the plan → before_spawn → spawn → after_exit
/// sequence and aggregates ingest reports.
async fn drive(
    globals: &GlobalArgs,
    adapter: &'static dyn HarnessAdapter,
    passthrough: Vec<String>,
    user_tags: Enrichment,
) -> anyhow::Result<i32> {
    // Merge env-derived defaults with explicit `--tag` flags. CLI flags
    // win on key collision.
    let mut tags: Enrichment = read_spawn_env_tags();
    for (k, v) in user_tags {
        tags.insert(k, v);
    }
    tags.insert("harness".to_string(), adapter.name().to_string());
    tags.insert("burnSpawn".to_string(), "1".to_string());
    let spawn_start_ts = std::time::SystemTime::now();
    tags.insert("burnSpawnTs".to_string(), iso_from_system_time(spawn_start_ts));

    let cwd = std::env::current_dir()?;
    let ctx = PlanCtx {
        cwd,
        passthrough,
        tags: tags.clone(),
        ledger_home: globals.ledger_path.clone(),
        spawn_start_ts,
    };

    let plan = adapter.plan(&ctx).await?;
    adapter.before_spawn(&ctx, &plan).await?;

    // Watcher accumulator: every tick adds to the running totals; we
    // aggregate after_exit's report on top. The TS sibling does the same.
    let totals = Arc::new(Mutex::new(IngestReport::default()));
    let totals_for_sink = totals.clone();
    let on_report: ReportSink =
        Arc::new(move |report: &IngestReport| {
            if let Ok(mut t) = totals_for_sink.lock() {
                t.scanned_sessions += report.scanned_sessions;
                t.ingested_sessions += report.ingested_sessions;
                t.appended_turns += report.appended_turns;
            }
        });

    let watcher = adapter.start_watcher(&ctx, on_report);
    if watcher.is_some() {
        eprintln!("[burn] {}: ingest watcher ready", adapter.name());
    }
    eprintln!("[burn] {}: starting {}", adapter.name(), plan.binary);

    // Spawn the child. inherits stdio so the user-facing harness UI
    // stays interactive. Layer plan.env_overrides on top of the parent
    // env, plus re-export the merged tag bag so transitive `burn …`
    // invocations inside the child see the same context.
    //
    // Use `tokio::process::Command::status().await` (not
    // `std::process::Command::status()`): the driver runs on a
    // current-thread tokio runtime so a synchronous `status()` call
    // would block the only thread, starving any watcher ticks scheduled
    // on the same runtime. The async variant yields between tokio
    // primitives so periodic watcher work can land while the child
    // lives.
    let mut cmd = TokioCommand::new(&plan.binary);
    cmd.args(&plan.args);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (k, v) in spawn_tag_env_overrides(&tags) {
        cmd.env(k, v);
    }
    for (k, v) in &plan.env_overrides {
        cmd.env(k, v);
    }

    // First tick fires immediately so a fast-finishing child has at
    // least one chance to drain new sessions before exit. This mirrors
    // `void watcher.tick()` in run.ts. We swallow tick errors on
    // purpose — the watch loop logs internally and the after_exit pass
    // is the source-of-truth fallback.
    if let Some(w) = &watcher {
        w.tick().await;
    }

    // Capture the spawn outcome up front so cleanup ALWAYS runs:
    // `before_spawn` may have written a stamp / pending manifest that
    // `after_exit` needs to reconcile, and the watcher may have
    // accumulated reports during its first tick. Returning early on
    // spawn failure (the previous shape) skipped both. The TS sibling
    // runs finalization regardless of spawn success; this matches that.
    let spawn_outcome: SpawnOutcome = match cmd.status().await {
        Ok(status) => SpawnOutcome::Exited(status),
        Err(err) => {
            eprintln!("[burn] failed to spawn {}: {err}", plan.binary);
            SpawnOutcome::SpawnFailed
        }
    };

    if let Some(w) = &watcher {
        w.stop().await;
    }

    // `after_exit` may itself fail (stamp resolve, ledger I/O); fold
    // that error into the summary line rather than short-circuiting,
    // so the user always gets the `[burn] <name> ingest: …` line.
    match adapter.after_exit(&ctx, &plan).await {
        Ok(final_report) => {
            let mut t = totals.lock().unwrap();
            t.scanned_sessions += final_report.scanned_sessions;
            t.ingested_sessions += final_report.ingested_sessions;
            t.appended_turns += final_report.appended_turns;
        }
        Err(err) => {
            eprintln!("[burn] {} after_exit failed: {err}", adapter.name());
        }
    }
    let totals = totals.lock().unwrap().clone();
    let session_word = if totals.ingested_sessions == 1 {
        "session"
    } else {
        "sessions"
    };
    let turn_word = if totals.appended_turns == 1 {
        "turn"
    } else {
        "turns"
    };
    eprintln!(
        "[burn] {} ingest: {} {} (+{} {})",
        adapter.name(),
        totals.ingested_sessions,
        session_word,
        totals.appended_turns,
        turn_word,
    );

    // Match the TS sibling: 127 for spawn failure (POSIX "command not
    // found"-ish), otherwise propagate the child's exit code (0 if it
    // exited via signal, mirroring `ExitStatus::code().unwrap_or(0)`).
    Ok(match spawn_outcome {
        SpawnOutcome::Exited(status) => status.code().unwrap_or(0),
        SpawnOutcome::SpawnFailed => 127,
    })
}

/// Captured spawn result. The driver finalizes (stops the watcher, runs
/// `after_exit`, emits the summary line) regardless of which arm fired,
/// then maps to a process exit code at the very end.
enum SpawnOutcome {
    Exited(ExitStatus),
    SpawnFailed,
}

/// Parse `--tag k=v` repetitions into an [`Enrichment`]. Mirrors the
/// TS sibling's `--tag` parser shape — bad input throws a typed error
/// rather than silently dropping the entry.
fn build_enrichment(tags: &[String]) -> anyhow::Result<Enrichment> {
    let mut out: Enrichment = BTreeMap::new();
    for raw in tags {
        let (k, v) = raw
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--tag expects k=v, got \"{raw}\""))?;
        if k.is_empty() {
            return Err(anyhow::anyhow!("--tag key must be non-empty (got \"{raw}\")"));
        }
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

/// Read `RELAYBURN_*` env vars into an enrichment bag. Mirrors
/// `readSpawnEnvTags` in `spawn-tags.ts`.
fn read_spawn_env_tags() -> Enrichment {
    let mut out: Enrichment = BTreeMap::new();
    for (env, tag) in SPAWN_ENV_TAG_KEYS {
        if let Ok(v) = std::env::var(env) {
            if !v.is_empty() {
                out.insert((*tag).to_string(), v);
            }
        }
    }
    out
}

/// Inverse of `read_spawn_env_tags`: re-export the merged tag bag as
/// `RELAYBURN_*` env so the spawned harness's transitive `burn …`
/// invocations inherit the same context.
fn spawn_tag_env_overrides(final_tags: &Enrichment) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (env, tag) in SPAWN_ENV_TAG_KEYS {
        if let Some(v) = final_tags.get(*tag) {
            if !v.is_empty() {
                out.push(((*env).to_string(), v.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_enrichment_parses_kv_pairs() {
        let got = build_enrichment(&[
            "workflow=refactor".into(),
            "agent=alpha".into(),
        ])
        .unwrap();
        assert_eq!(got.get("workflow").map(String::as_str), Some("refactor"));
        assert_eq!(got.get("agent").map(String::as_str), Some("alpha"));
    }

    #[test]
    fn build_enrichment_rejects_missing_eq() {
        let err = build_enrichment(&["workflow".into()]).unwrap_err();
        assert!(format!("{err}").contains("--tag expects k=v"));
    }

    #[test]
    fn build_enrichment_rejects_empty_key() {
        let err = build_enrichment(&["=missing-key".into()]).unwrap_err();
        assert!(format!("{err}").contains("--tag key must be non-empty"));
    }

    #[test]
    fn spawn_tag_env_overrides_re_exports_known_keys() {
        let mut tags: Enrichment = BTreeMap::new();
        tags.insert("workflowId".into(), "wf-1".into());
        tags.insert("agentId".into(), "agent-x".into());
        tags.insert("burnSpawn".into(), "1".into()); // not in keys → dropped
        let env = spawn_tag_env_overrides(&tags);
        let map: BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(map.get("RELAYBURN_WORKFLOW_ID").map(String::as_str), Some("wf-1"));
        assert_eq!(map.get("RELAYBURN_AGENT_ID").map(String::as_str), Some("agent-x"));
        assert!(!map.contains_key("RELAYBURN_BURN_SPAWN"));
    }
}
