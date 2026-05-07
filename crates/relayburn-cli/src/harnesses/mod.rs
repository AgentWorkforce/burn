//! Harness substrate — Rust port of `packages/cli/src/harnesses/types.ts`
//! and friends.
//!
//! `burn run <harness>` is a wrapper that spawns a coding-agent process
//! (Claude Code, Codex, OpenCode, …), babysits its session log while it
//! runs, and feeds the resulting turns into the relayburn ledger. Every
//! adapter contributes the same five-step shape:
//!
//! 1. **`plan`** — compute the spawn plan (binary + args + env). Per-harness
//!    transports inject session ids or hook arguments here.
//! 2. **`before_spawn`** — fire any pre-spawn side effect: stamp now if the
//!    session id is known up front (claude path), or drop a pending-stamp
//!    manifest the post-spawn ingest pass will resolve (codex / opencode).
//! 3. **`start_watcher`** *(optional)* — return a [`WatchController`] that
//!    drains a session-store directory while the child runs. Adapters that
//!    ingest a single pre-known session file (claude) return `None` here;
//!    adapters that share the pending-stamp shape (codex, opencode) wire
//!    the watch loop through [`pending_stamp::adapter`].
//! 4. **`after_exit`** — run a final ingest pass after the child exits and
//!    return an [`IngestReport`] so the driver can fold it into the unified
//!    `[burn] <name> ingest: …` line.
//! 5. The driver itself owns step zero — collecting `cwd`, passthrough
//!    args, and any user-provided enrichment tags into a [`PlanCtx`] —
//!    and step six — joining the watcher and reporting summary stats.
//!
//! ## Where this fits
//!
//! This PR (#248 part b) is the substrate. The Wave 2 PRs (#248-d/e/f)
//! plug the three concrete adapters into [`registry`] and the
//! `burn run` driver in `commands::run` consumes them. The CLI scaffold
//! (#248 part a, sibling worktree) lands the clap entrypoint independently.
//!
//! ## Trait shape vs the TS sibling
//!
//! `HarnessAdapter` is a `Send + Sync` trait object so the registry can
//! hand out `&'static dyn HarnessAdapter` references. `async fn` in trait
//! is mediated by `async_trait::async_trait` to keep adapter impls
//! ergonomic; the desugared `Pin<Box<dyn Future + Send>>` matches the
//! shape expected by the `burn run` driver, which `tokio::spawn`s the
//! result of `plan` / `after_exit` and joins them at the top level.

use std::path::PathBuf;

use async_trait::async_trait;
use relayburn_sdk::{Enrichment, IngestReport, WatchController};

pub mod codex;
pub mod pending_stamp;
pub mod registry;

pub use registry::{list_harness_names, lookup};

/// Driver-side context handed to every adapter call. Mirrors the TS
/// `HarnessRunContext` shape one-to-one (`cwd`, `passthrough`, `tags`,
/// `spawnStartTs`).
///
/// `tags` is a `BTreeMap<String, String>` (re-exported from the SDK as
/// [`Enrichment`]) so insertion order doesn't matter for the on-disk
/// stamp record — the pending-stamp serializer canonicalizes ordering.
#[derive(Debug, Clone)]
pub struct PlanCtx {
    /// Working directory the user invoked `burn run` from. Forwarded to
    /// the spawned harness so it picks up project-local config.
    pub cwd: PathBuf,
    /// Argv tail after the subcommand boundary, e.g. `burn run claude --
    /// "explain this"` ⇒ `["explain this"]`. Adapters splice this into
    /// their generated argv via [`SpawnPlan::args`].
    pub passthrough: Vec<String>,
    /// User-supplied enrichment that will be merged onto the resulting
    /// stamp. Keys are free-form (`task`, `pr`, …); the Wave 2 driver
    /// translates `--tag k=v` flags into entries here.
    pub tags: Enrichment,
    /// Wall-clock timestamp captured by the driver immediately before
    /// `before_spawn`. Used by the pending-stamp manifest so the
    /// post-exit resolver can match against session-file mtimes.
    pub spawn_start_ts: std::time::SystemTime,
}

/// Spawn plan returned by [`HarnessAdapter::plan`]. The `burn run`
/// driver owns the actual `tokio::process::Command` construction; this
/// struct is the per-adapter contribution to it.
///
/// `session_id` is filled in by adapters that know the session id up
/// front (claude can mint one and inject it via `--session-id` so the
/// pre-spawn stamp is final from the start). Adapters that don't know
/// it ahead of time leave this `None` and rely on the pending-stamp
/// resolver to attach their enrichment to the freshly-discovered
/// session in `after_exit`.
#[derive(Debug, Clone, Default)]
pub struct SpawnPlan {
    pub binary: String,
    pub args: Vec<String>,
    /// Env vars to overlay on top of the parent process env when
    /// spawning. Keep this tight — `tokio::process::Command::env_clear`
    /// + this map is the typical pattern, though Wave 2 may relax that.
    pub env_overrides: Vec<(String, String)>,
    /// Session id the adapter pre-allocated, when known. See struct
    /// docs for when this is `Some` vs `None`.
    pub session_id: Option<String>,
}

impl SpawnPlan {
    /// Convenience: minimal plan that just runs `binary` with `args` and
    /// inherits the parent's env. Most adapters' `plan` returns this
    /// shape directly.
    pub fn new(binary: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            binary: binary.into(),
            args,
            env_overrides: Vec::new(),
            session_id: None,
        }
    }
}

/// `HarnessAdapter` — five-method contract every harness implements. The
/// TS sibling lives at `packages/cli/src/harnesses/types.ts` and the
/// shape mirrors it; see the module docs for what each step does.
///
/// Adapters are zero-sized (or near-zero-sized) stateless types that the
/// registry hands out as `&'static dyn HarnessAdapter`. State that lives
/// across `before_spawn` → `after_exit` rides on `PlanCtx` / `SpawnPlan`,
/// or in the pending-stamps directory on disk.
#[async_trait]
pub trait HarnessAdapter: Send + Sync {
    /// Lowercase identifier — `claude`, `codex`, `opencode`, … — used as
    /// the dispatch key and as the harness label in log lines.
    fn name(&self) -> &'static str;

    /// Per-harness session-store root. Today this is a fixed path
    /// resolved against the user's home directory; future iterations
    /// may thread `BurnConfig` through so the root is configurable.
    fn session_root(&self) -> PathBuf;

    /// Compute the spawn plan. Inject session ids or transport-level
    /// args here. Populate `SpawnPlan::session_id` when known so
    /// `before_spawn` / `after_exit` can stamp eagerly.
    async fn plan(&self, ctx: &PlanCtx) -> anyhow::Result<SpawnPlan>;

    /// Pre-spawn side effects. Stamp now if the session id is in `plan`,
    /// otherwise drop a pending-stamp manifest the post-spawn ingest can
    /// resolve. Default impl is a no-op so simple adapters don't have to
    /// spell it out.
    async fn before_spawn(&self, _ctx: &PlanCtx, _plan: &SpawnPlan) -> anyhow::Result<()> {
        Ok(())
    }

    /// Optional. Return a [`WatcherController`] from
    /// [`relayburn_sdk::start_watch_loop`] to drain a session store
    /// while the child runs; return `None` for adapters that ingest a
    /// single pre-known file at exit.
    ///
    /// `on_report` is a callback the driver routes into its summary
    /// accumulator so the final `[burn] <name> ingest:` line reflects
    /// every tick that fired during the run, not just `after_exit`.
    fn start_watcher(
        &self,
        _ctx: &PlanCtx,
        _on_report: relayburn_sdk::ReportSink,
    ) -> Option<WatcherController> {
        None
    }

    /// Final ingest pass after the child exits. Returns an
    /// [`IngestReport`] the driver folds into its summary line.
    async fn after_exit(&self, ctx: &PlanCtx, plan: &SpawnPlan) -> anyhow::Result<IngestReport>;
}

/// Wrapper around the SDK's [`WatchController`]. Today this is just a
/// newtype so callers don't have to import `relayburn_sdk` directly to
/// construct or stop a watcher; tomorrow it gives us a stable boundary
/// to attach harness-side observability (e.g. a `name`, a per-adapter
/// metric counter) without leaking through to the SDK.
pub struct WatcherController {
    inner: WatchController,
}

impl WatcherController {
    /// Wrap a raw SDK controller. `pending_stamp::adapter` is the
    /// canonical caller; bespoke adapters that build their own watch
    /// loop also funnel through here.
    pub fn new(inner: WatchController) -> Self {
        Self { inner }
    }

    /// Run a single tick on demand. Forwards to
    /// [`WatchController::tick`].
    pub async fn tick(&self) {
        self.inner.tick().await;
    }

    /// Stop the periodic loop and await any in-flight tick. Idempotent.
    /// `burn run` calls this once the spawned child exits.
    pub async fn stop(&self) {
        self.inner.stop().await;
    }

    /// Borrow the wrapped controller for callers that need the raw
    /// SDK type (e.g. integration tests parking on `tick_done`).
    pub fn raw(&self) -> &WatchController {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: `SpawnPlan::new` produces an inherit-env plan the
    /// driver can hand straight to `tokio::process::Command`. Catches
    /// accidental shape changes on the struct.
    #[test]
    fn spawn_plan_new_minimal_shape() {
        let plan = SpawnPlan::new("claude", vec!["--help".into()]);
        assert_eq!(plan.binary, "claude");
        assert_eq!(plan.args, vec!["--help".to_string()]);
        assert!(plan.env_overrides.is_empty());
        assert!(plan.session_id.is_none());
    }

    /// Trait dispatch sanity: a fake adapter implementing `HarnessAdapter`
    /// must be coercible to `&dyn HarnessAdapter` so the registry can
    /// hand out trait-object references.
    struct FakeAdapter;

    #[async_trait]
    impl HarnessAdapter for FakeAdapter {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn session_root(&self) -> PathBuf {
            PathBuf::from("/tmp/fake")
        }
        async fn plan(&self, _ctx: &PlanCtx) -> anyhow::Result<SpawnPlan> {
            Ok(SpawnPlan::new("fake", vec![]))
        }
        async fn after_exit(
            &self,
            _ctx: &PlanCtx,
            _plan: &SpawnPlan,
        ) -> anyhow::Result<IngestReport> {
            Ok(IngestReport::default())
        }
    }

    #[tokio::test]
    async fn fake_adapter_round_trip() {
        let adapter: &dyn HarnessAdapter = &FakeAdapter;
        assert_eq!(adapter.name(), "fake");
        assert_eq!(adapter.session_root(), PathBuf::from("/tmp/fake"));

        let ctx = PlanCtx {
            cwd: PathBuf::from("/tmp"),
            passthrough: vec![],
            tags: Enrichment::new(),
            spawn_start_ts: std::time::SystemTime::now(),
        };
        let plan = adapter.plan(&ctx).await.unwrap();
        assert_eq!(plan.binary, "fake");

        let report = adapter.after_exit(&ctx, &plan).await.unwrap();
        assert_eq!(report.scanned_sessions, 0);
        assert_eq!(report.ingested_sessions, 0);
    }
}
