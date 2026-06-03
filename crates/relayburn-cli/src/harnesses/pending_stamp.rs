//! `pending_stamp::adapter` factory — Rust port of
//! `packages/cli/src/harnesses/pending-stamp.ts`'s `createPendingStampAdapter`.
//!
//! Codex and OpenCode share an identical wrapper shape: pre-spawn pending
//! stamp, while-running watch loop draining the session store, post-exit
//! ingest pass. The TS sibling captures that shape once via a factory; the
//! Rust port does the same here so the Wave 2 codex / opencode adapter PRs
//! are one-line constructions instead of two near-duplicate `impl`s.
//!
//! ## Composition
//!
//! ```text
//! plan         → `SpawnPlan::new(name, ctx.passthrough)`  (no env, no session_id)
//! before_spawn → `relayburn_sdk::write_pending_stamp(...)` + log
//! start_watcher→ `relayburn_sdk::start_watch_loop(non-immediate, on_report → ingest_fn)`
//! after_exit   → `(config.ingest_sessions)(...)`
//! ```
//!
//! `ingest_sessions` is a caller-supplied async closure (Wave 2 will pass
//! `relayburn_sdk::ingest` with codex-only or opencode-only roots). The
//! factory doesn't reach into `relayburn_sdk::ingest` directly so adapter
//! authors can swap in test doubles without monkey-patching env vars. The
//! closure receives the active ledger home so its ingest pass can read the
//! same sidecar directory `before_spawn` wrote to.
//!
//! ## What this PR does NOT do
//!
//! - No concrete codex / opencode adapter — those land in #248-e / #248-f.
//! - No log line yet (`[burn] codex spawn: pending stamp …`); the TS
//!   sibling writes it through `process.stderr.write`. The Rust factory
//!   exposes the manifest filename via the `before_spawn` log hook so
//!   Wave 2 adapters can print it under whatever logging discipline the
//!   CLI scaffold (#248-a) settles on. Today we just route through
//!   `eprintln!`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use relayburn_sdk::{
    start_watch_loop, write_pending_stamp, IngestFn, IngestReport, Ledger, LedgerOpenOptions,
    PendingStampHarness, PendingStampWriteOptions, RawIngestOptions, RawLedger, ReportSink,
    StartWatchLoopOptions, WatchController,
};

use super::{HarnessAdapter, PlanCtx, SpawnPlan};

/// Async ingest callback supplied by the caller. Returns the report the
/// watch loop and `after_exit` hand back to the driver.
pub type IngestSessionsFn = Arc<
    dyn Fn(Option<PathBuf>) -> Pin<Box<dyn Future<Output = anyhow::Result<IngestReport>> + Send>>
        + Send
        + Sync,
>;

/// Configuration for [`adapter`]. Mirrors the TS
/// `PendingStampAdapterOptions` shape.
#[derive(Clone)]
pub struct PendingStampAdapter {
    /// Lowercase harness name — `codex` or `opencode`. The factory
    /// asserts this maps to a [`PendingStampHarness`] variant; passing
    /// anything else is a programmer error and panics on construction.
    pub name: &'static str,
    /// Per-harness session-store root (e.g. `~/.codex/sessions`).
    /// Resolved lazily via the supplied closure so tests can inject
    /// temp dirs without touching `$HOME`.
    pub session_root: Arc<dyn Fn() -> PathBuf + Send + Sync>,
    /// Final ingest pass — called by `after_exit` and by every tick of
    /// the watch loop while the child runs.
    pub ingest_sessions: IngestSessionsFn,
    /// Watch-loop tick interval. Defaults to 1s (matches the TS sibling).
    pub watch_interval: Duration,
}

impl PendingStampAdapter {
    /// Construct a factory config with the standard 1s tick. Callers
    /// that need a different cadence build the struct directly.
    pub fn new(
        name: &'static str,
        session_root: Arc<dyn Fn() -> PathBuf + Send + Sync>,
        ingest_sessions: IngestSessionsFn,
    ) -> Self {
        Self {
            name,
            session_root,
            ingest_sessions,
            watch_interval: Duration::from_millis(1000),
        }
    }
}

/// Build a [`HarnessAdapter`] from a [`PendingStampAdapter`] config.
///
/// Returns a heap-allocated trait object the caller owns. Tests and bespoke
/// drivers that don't need to feed the result into the `phf::Map` registry
/// use this form. Wave 2 codex / opencode adapter PRs go through
/// [`adapter_static`] instead so the registry can store them as
/// `&'static dyn HarnessAdapter`.
pub fn adapter(config: PendingStampAdapter) -> Box<dyn HarnessAdapter> {
    Box::new(PendingStampAdapterImpl::new(config))
}

/// Build a `&'static dyn HarnessAdapter` from a [`PendingStampAdapter`] config.
///
/// Wave 2 codex / opencode adapter PRs use this form so the value can be
/// stored in `super::registry::ADAPTERS`, which is a compile-time
/// `phf::Map<&'static str, &'static dyn HarnessAdapter>` and therefore
/// cannot hold a runtime `Box`.
///
/// ## Memory leak by design
///
/// Internally this calls [`Box::leak`]. The allocation is unrecoverable
/// for the rest of the process lifetime — there is no way to drop the
/// returned `&'static`. That is fine in this codebase for two reasons:
///
/// 1. There are at most three pending-stamp harnesses ever (codex,
///    opencode, and any future addition that joins the same protocol),
///    so the leaked footprint is bytes, not megabytes.
/// 2. Each adapter is registered exactly once at process start (via a
///    `LazyLock<&'static dyn HarnessAdapter>` next to its module
///    declaration in `super::registry`) and lives until process exit.
///    There is no register/deregister churn.
///
/// Callers that need a non-static, droppable adapter — e.g. unit tests
/// that construct a one-off adapter and let it fall out of scope —
/// should use [`adapter`] instead.
pub fn adapter_static(config: PendingStampAdapter) -> &'static dyn HarnessAdapter {
    Box::leak(Box::new(PendingStampAdapterImpl::new(config)))
}

/// Fn pointer for an SDK session ingestor (`ingest_codex_sessions`,
/// `ingest_opencode_sessions`). Both verbs are sync in the SDK; the
/// per-tick `Box::pin` adaptation that drops them into [`IngestSessionsFn`]
/// happens at the call site in [`session_store_adapter`] so the helper
/// stays a fn pointer (no per-tick closure allocation).
pub type SessionIngestor = fn(&mut RawLedger, &RawIngestOptions) -> anyhow::Result<IngestReport>;

/// One-call factory for pending-stamp adapters whose only differences are
/// the harness name, the session-root resolver, and which SDK ingest pass
/// they call. Codex and OpenCode share the entire wrapper shape — opening
/// a fresh ledger handle per tick, threading the active ledger home
/// through `RawIngestOptions`, deferring to an SDK ingest function — so
/// this helper folds that boilerplate behind a single line per harness.
///
/// Returns the same `&'static dyn HarnessAdapter` form as
/// [`adapter_static`] so the result drops directly into the runtime
/// registry. See [`adapter_static`] for the leak rationale; this helper
/// has the same per-process bytes-not-megabytes footprint.
pub fn session_store_adapter(
    name: &'static str,
    session_root: fn() -> PathBuf,
    ingestor: SessionIngestor,
) -> &'static dyn HarnessAdapter {
    let session_root: Arc<dyn Fn() -> PathBuf + Send + Sync> = Arc::new(session_root);
    let ingest_sessions: IngestSessionsFn = Arc::new(move |ledger_home| {
        Box::pin(async move {
            // Per-tick ledger open mirrors the TS sibling's
            // `withLock('ledger', …)` pattern. SQLite WAL keeps the open
            // cheap (no DDL after first open). Use the typed ledger home
            // so explicit `--ledger-path` runs keep manifest writes and
            // resolution scoped to the same home the writer used.
            let ledger_opts = match ledger_home.as_deref() {
                Some(home) => LedgerOpenOptions::with_home(home),
                None => LedgerOpenOptions::default(),
            };
            let mut handle = Ledger::open(ledger_opts)?;
            let opts = RawIngestOptions {
                ledger_home,
                ..RawIngestOptions::default()
            };
            ingestor(handle.raw_mut(), &opts)
        })
    });
    adapter_static(PendingStampAdapter::new(
        name,
        session_root,
        ingest_sessions,
    ))
}

/// `HarnessAdapter` implementation backing the [`adapter`] factory. Kept
/// private so callers can't construct it directly without going through
/// the validated factory.
struct PendingStampAdapterImpl {
    name: &'static str,
    harness: PendingStampHarness,
    session_root: Arc<dyn Fn() -> PathBuf + Send + Sync>,
    ingest_sessions: IngestSessionsFn,
    watch_interval: Duration,
}

impl PendingStampAdapterImpl {
    fn new(config: PendingStampAdapter) -> Self {
        let harness = match config.name {
            "codex" => PendingStampHarness::Codex,
            "opencode" => PendingStampHarness::Opencode,
            other => {
                // Programmer error: the SDK's pending-stamp protocol only
                // recognises codex + opencode. Adding a third pending-stamp
                // harness is a coordinated change with the SDK manifest
                // schema, not a CLI-side decision.
                panic!(
                    "pending_stamp::adapter only supports codex|opencode, got {other:?}; \
                     extending the protocol requires an SDK change"
                )
            }
        };
        Self {
            name: config.name,
            harness,
            session_root: config.session_root,
            ingest_sessions: config.ingest_sessions,
            watch_interval: config.watch_interval,
        }
    }

    /// Build the IngestFn the watch loop calls each tick. Captures the
    /// caller-supplied `ingest_sessions` closure so the loop runs the
    /// same path `after_exit` does.
    fn ingest_fn(&self, ledger_home: Option<PathBuf>) -> IngestFn {
        let ingest_sessions = self.ingest_sessions.clone();
        // `_force` is ignored here: the pending-stamp watcher runs its own
        // caller-supplied ingest closure (not `ingest_all` directly), so the
        // FS-event force-scan signal isn't plumbed through this path.
        Arc::new(move |_force: bool| {
            let f = ingest_sessions.clone();
            let ledger_home = ledger_home.clone();
            Box::pin(async move { f(ledger_home).await })
        })
    }

    /// Convenience: just the file-name component of a manifest path,
    /// for stable log lines that don't dump the user's home directory.
    fn manifest_basename(path: &Path) -> String {
        path.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    }
}

#[async_trait]
impl HarnessAdapter for PendingStampAdapterImpl {
    fn name(&self) -> &'static str {
        self.name
    }

    fn session_root(&self) -> PathBuf {
        (self.session_root)()
    }

    async fn plan(&self, ctx: &PlanCtx) -> anyhow::Result<SpawnPlan> {
        Ok(SpawnPlan::new(self.name, ctx.passthrough.clone()))
    }

    async fn before_spawn(&self, ctx: &PlanCtx, _plan: &SpawnPlan) -> anyhow::Result<()> {
        let session_dir_hint = (self.session_root)();
        let opts = PendingStampWriteOptions {
            harness: self.harness,
            ledger_home: ctx.ledger_home.clone(),
            cwd: ctx.cwd.to_string_lossy().into_owned(),
            enrichment: ctx.tags.clone(),
            session_dir_hint: Some(session_dir_hint.to_string_lossy().into_owned()),
            spawn_start_ts: Some(ctx.spawn_start_ts),
            spawner_pid: None,
        };
        let written = write_pending_stamp(opts)
            .map_err(|err| anyhow::anyhow!("failed to write {} pending stamp: {err}", self.name))?;
        eprintln!(
            "[burn] {} spawn: pending stamp {}",
            self.name,
            Self::manifest_basename(&written.file)
        );
        Ok(())
    }

    fn start_watcher(&self, ctx: &PlanCtx, on_report: ReportSink) -> Option<WatchController> {
        // Match the TS adapter: do not run an immediate first tick. The
        // child has barely started; let the periodic interval drive the
        // first scan so we don't spawn an ingest pass that races the
        // freshly-written pending stamp.
        //
        // Watch the per-harness session root with `notify` so the loop
        // wakes on session writes instead of polling every second
        // (#250). `watch_interval` becomes the polling fallback cadence
        // when `notify` cannot attach (fresh install with no session
        // dir yet, network mount, etc.).
        let opts = StartWatchLoopOptions::new(self.ingest_fn(ctx.ledger_home.clone()))
            .with_immediate(false)
            .with_interval(self.watch_interval)
            .with_watch_paths(vec![(self.session_root)()])
            .with_on_report(on_report);
        Some(start_watch_loop(opts))
    }

    async fn after_exit(&self, ctx: &PlanCtx, _plan: &SpawnPlan) -> anyhow::Result<IngestReport> {
        (self.ingest_sessions)(ctx.ledger_home.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use relayburn_sdk::Enrichment;

    use super::*;

    /// `adapter()` round-trips through the trait surface for codex.
    /// Exercises name + session_root + plan.
    #[tokio::test]
    async fn codex_factory_round_trip() {
        let session_root: Arc<dyn Fn() -> PathBuf + Send + Sync> =
            Arc::new(|| PathBuf::from("/tmp/codex-sessions"));
        let ingest_sessions: IngestSessionsFn =
            Arc::new(|_ledger_home| Box::pin(async { Ok(IngestReport::default()) }));
        let config = PendingStampAdapter::new("codex", session_root, ingest_sessions);
        let adapter: Box<dyn HarnessAdapter> = adapter(config);

        assert_eq!(adapter.name(), "codex");
        assert_eq!(adapter.session_root(), PathBuf::from("/tmp/codex-sessions"));

        let ctx = PlanCtx {
            cwd: PathBuf::from("/tmp"),
            passthrough: vec!["--help".into()],
            tags: Enrichment::new(),
            ledger_home: None,
            spawn_start_ts: std::time::SystemTime::now(),
        };
        let plan = adapter.plan(&ctx).await.unwrap();
        assert_eq!(plan.binary, "codex");
        assert_eq!(plan.args, vec!["--help".to_string()]);

        // `after_exit` runs the user-supplied closure verbatim.
        let report = adapter.after_exit(&ctx, &plan).await.unwrap();
        assert_eq!(report.scanned_sessions, 0);
    }

    /// `adapter()` round-trips through the trait surface for opencode —
    /// same shape, different name.
    #[tokio::test]
    async fn opencode_factory_round_trip() {
        let session_root: Arc<dyn Fn() -> PathBuf + Send + Sync> =
            Arc::new(|| PathBuf::from("/tmp/opencode-storage"));
        let ingest_sessions: IngestSessionsFn =
            Arc::new(|_ledger_home| Box::pin(async { Ok(IngestReport::default()) }));
        let config = PendingStampAdapter::new("opencode", session_root, ingest_sessions);
        let adapter = adapter(config);
        assert_eq!(adapter.name(), "opencode");
        assert_eq!(
            adapter.session_root(),
            PathBuf::from("/tmp/opencode-storage")
        );
    }

    /// Bogus harness names panic on construction — the factory doesn't
    /// silently fall through to a default. This catches typos at adapter
    /// registration time rather than at runtime.
    #[test]
    #[should_panic(expected = "pending_stamp::adapter only supports")]
    fn unknown_name_panics() {
        let session_root: Arc<dyn Fn() -> PathBuf + Send + Sync> =
            Arc::new(|| PathBuf::from("/tmp"));
        let ingest_sessions: IngestSessionsFn =
            Arc::new(|_ledger_home| Box::pin(async { Ok(IngestReport::default()) }));
        let _ = adapter(PendingStampAdapter::new(
            "cursor",
            session_root,
            ingest_sessions,
        ));
    }

    /// `after_exit` invokes the supplied `ingest_sessions` closure. We
    /// use an atomic counter to confirm it was called.
    #[tokio::test]
    async fn after_exit_invokes_supplied_ingest_fn() {
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_closure = count.clone();
        let session_root: Arc<dyn Fn() -> PathBuf + Send + Sync> =
            Arc::new(|| PathBuf::from("/tmp/codex-sessions"));
        let ingest_sessions: IngestSessionsFn = Arc::new(move |_ledger_home| {
            let c = count_for_closure.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(IngestReport::default())
            })
        });
        let config = PendingStampAdapter::new("codex", session_root, ingest_sessions);
        let adapter = adapter(config);

        let ctx = PlanCtx {
            cwd: PathBuf::from("/tmp"),
            passthrough: vec![],
            tags: Enrichment::new(),
            ledger_home: None,
            spawn_start_ts: std::time::SystemTime::now(),
        };
        let plan = adapter.plan(&ctx).await.unwrap();
        adapter.after_exit(&ctx, &plan).await.unwrap();
        adapter.after_exit(&ctx, &plan).await.unwrap();

        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn before_spawn_writes_pending_stamp_under_context_ledger_home() {
        let ledger_home = tempdir("ledger-home");
        let cwd = tempdir("cwd");
        let session_root_path = tempdir("session-root");
        let session_root: Arc<dyn Fn() -> PathBuf + Send + Sync> =
            Arc::new(move || session_root_path.clone());
        let ingest_sessions: IngestSessionsFn =
            Arc::new(|_ledger_home| Box::pin(async { Ok(IngestReport::default()) }));
        let config = PendingStampAdapter::new("codex", session_root, ingest_sessions);
        let adapter = adapter(config);

        let ctx = PlanCtx {
            cwd: cwd.clone(),
            passthrough: vec![],
            tags: Enrichment::new(),
            ledger_home: Some(ledger_home.clone()),
            spawn_start_ts: std::time::SystemTime::now(),
        };
        let plan = adapter.plan(&ctx).await.unwrap();
        adapter.before_spawn(&ctx, &plan).await.unwrap();

        let dir = ledger_home.join("pending-stamps");
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    fn tempdir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "burn-pending-stamp-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
