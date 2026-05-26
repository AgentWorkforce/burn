//! Lazy harness registry — Rust port of `packages/cli/src/harnesses/registry.ts`.
//!
//! The TS sibling defers each adapter import (`async () => (await
//! import('./claude.js')).claudeAdapter`) so unrelated commands don't
//! pay ingest/ledger startup cost. The Rust port doesn't have lazy
//! module imports, but trait-object adapters either fit a `phf::Map`
//! (zero-sized unit-struct adapters) or hide behind a [`LazyLock`]
//! (adapters constructed via [`Box::leak`] at first lookup).
//!
//! ## Two-tier layout — and why
//!
//! There are two real categories of harness adapters:
//!
//! 1. **Eager / unit-struct adapters.** Stateless, zero-sized impls
//!    declared as `static FOO: FooAdapter = FooAdapter;`. These fit a
//!    `phf::Map<&'static str, &'static dyn HarnessAdapter>` directly:
//!    the value `&FOO` is a const expression and `phf_map!` is happy.
//!    Claude (#248-d) lands here.
//!
//! 2. **Runtime-constructed adapters.** Codex / opencode are built via
//!    [`super::pending_stamp::adapter_static`], which captures
//!    closures (session-root resolver, ingest callback) inside an
//!    `Arc` and `Box::leak`s the resulting trait object. The leak
//!    happens at runtime — there is no const expression that yields a
//!    `&'static dyn HarnessAdapter` for these, so `phf_map!` cannot
//!    hold them. They live in a [`LazyLock`]-backed sibling map keyed
//!    the same way.
//!
//! [`lookup`] checks the eager `phf::Map` first (single perfect-hash
//! probe, zero allocation), then falls back to the runtime map. List
//! ordering merges both: phf entries first, runtime entries appended.
//!
//! ## Why not push everything into a runtime `HashMap`?
//!
//! Cold-start matters: `burn --help` and `burn summary` shouldn't pay
//! any harness-table init cost. Eager adapters belong in `phf::Map`
//! because they can be there for free. The runtime tier is a precise
//! escape hatch for adapters that genuinely need runtime
//! construction, not a default.
//!
//! ## Wave 2 plug-in points
//!
//! Three slots are reserved for the Wave 2 adapter PRs:
//!
//! * `claude` — #248-d (Wave 2 D5). Stateless unit struct registered
//!   in [`EAGER_ADAPTERS`] as `&CLAUDE_ADAPTER`.
//! * `codex` — #248-e (Wave 2 D6). Built via
//!   [`super::pending_stamp::adapter_static`] inside a [`LazyLock`]
//!   and registered in [`RUNTIME_ADAPTERS`].
//! * `opencode` — #248-f (Wave 2 D7). Same shape as codex.

use std::collections::HashMap;
use std::sync::LazyLock;

use phf::phf_map;

use super::{claude, codex, opencode, HarnessAdapter};

/// Compile-time perfect-hash map from harness name to a `&'static dyn
/// HarnessAdapter`. Holds eager / unit-struct adapters whose value is a
/// const expression (`&SOMETHING_STATIC`). Wave 2 D5 (#248-d) registers
/// the claude adapter here.
///
/// **Do not register pending-stamp adapters here.**
/// `pending_stamp::adapter_static` returns a value produced by
/// `Box::leak` at runtime; that is not a const expression and cannot
/// appear inside `phf_map!`. Those adapters go in [`RUNTIME_ADAPTERS`].
static EAGER_ADAPTERS: phf::Map<&'static str, &'static dyn HarnessAdapter> = phf_map! {
    "claude" => &claude::CLAUDE_ADAPTER,
};

/// Runtime-constructed adapters. The closure runs once on first
/// lookup; afterwards the map is read-only. Each entry's value is a
/// `&'static dyn HarnessAdapter` produced by
/// [`super::pending_stamp::adapter_static`] (`Box::leak`-backed).
///
/// Empty on this branch — populated by the codex (#248-e) and
/// opencode (#248-f) Wave 2 PRs. Wave 2 wiring will look like:
///
/// ```ignore
/// static RUNTIME_ADAPTERS: LazyLock<HashMap<&'static str, &'static dyn HarnessAdapter>> =
///     LazyLock::new(|| {
///         let mut m = HashMap::new();
///         m.insert("codex", pending_stamp::adapter_static(codex_config()));
///         m.insert("opencode", pending_stamp::adapter_static(opencode_config()));
///         m
///     });
/// ```
static RUNTIME_ADAPTERS: LazyLock<HashMap<&'static str, &'static dyn HarnessAdapter>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("codex", codex::adapter()); // #248-e (Wave 2 D6)
        m.insert("opencode", opencode::adapter()); // #248-f (Wave 2 D7)
        m
    });

/// Sibling list of runtime adapter names in stable, deterministic
/// order. Read by [`list_harness_names`] so the CLI's `--help` output
/// (a) doesn't force [`RUNTIME_ADAPTERS`] lazy init — `HashMap::keys()`
/// would, via `LazyLock`'s `Deref` — and (b) doesn't flicker between
/// runs (Rust's `HashMap` randomizes iteration order across program
/// runs for HashDoS resistance).
///
/// **Wave 2 sync requirement.** This list MUST stay in lockstep with
/// [`RUNTIME_ADAPTERS`]: every name registered in the lazy map needs a
/// matching entry here, in the order it should appear in `--help`.
/// When codex (#248-e) and opencode (#248-f) land, each PR uncomments
/// **two** rows: its `RUNTIME_ADAPTERS` insert AND its
/// `RUNTIME_ADAPTER_NAMES` entry. The deterministic-ordering test in
/// this module's `tests` block pins the resulting order.
static RUNTIME_ADAPTER_NAMES: &[&str] = &[
    // Wave 2 PRs populate these slots in lockstep with RUNTIME_ADAPTERS:
    "codex",    // #248-e (Wave 2 D6)
    "opencode", // #248-f (Wave 2 D7)
];

/// Look up an adapter by name. Returns `None` for unknown names; the
/// Callers can map `None` to a "did you mean …?" diagnostic using
/// [`list_harness_names`].
///
/// Eager adapters (single perfect-hash probe) are checked first; the
/// runtime map is consulted only on a miss so common-case lookups
/// never touch the [`LazyLock`].
pub fn lookup(name: &str) -> Option<&'static dyn HarnessAdapter> {
    if let Some(adapter) = EAGER_ADAPTERS.get(name).copied() {
        return Some(adapter);
    }
    RUNTIME_ADAPTERS.get(name).copied()
}

/// List every registered harness name. The CLI's `--help` block reads
/// this so the harness list updates automatically when a new adapter
/// is registered. Order is `phf::Map` iteration order (eager
/// adapters; deterministic, fixed at compile time by the perfect-hash
/// build) followed by [`RUNTIME_ADAPTER_NAMES`] (runtime adapters; a
/// hand-ordered slice).
///
/// **Cold-start contract:** this function must not force
/// [`RUNTIME_ADAPTERS`] lazy init. `burn --help` calls into here, and
/// the registry-level doc comment promises help-path callers don't
/// pay harness-table construction cost. The runtime tier is read via
/// the sibling [`RUNTIME_ADAPTER_NAMES`] slice for that reason; touching
/// `RUNTIME_ADAPTERS.keys()` would defeat the goal (and also leak
/// `HashMap`'s non-deterministic iteration order into help output).
pub fn list_harness_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = EAGER_ADAPTERS.keys().copied().collect();
    names.extend(RUNTIME_ADAPTER_NAMES.iter().copied());
    names
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, LazyLock};

    use async_trait::async_trait;
    use relayburn_sdk::IngestReport;

    use super::super::pending_stamp::{self, IngestSessionsFn, PendingStampAdapter};
    use super::super::{HarnessAdapter, PlanCtx, SpawnPlan};
    use super::*;

    /// A registry-injectable fake adapter used to exercise the lookup
    /// path. The real registry only ships the Wave 2 adapters; this test
    /// asserts the substrate independently of which slots are populated.
    struct FakeAdapter;

    #[async_trait]
    impl HarnessAdapter for FakeAdapter {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn session_root(&self) -> PathBuf {
            PathBuf::from("/tmp/fake-sessions")
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

    static FAKE: FakeAdapter = FakeAdapter;

    /// Static fake registry shaped exactly like production
    /// [`EAGER_ADAPTERS`]. `phf_map!` requires its output to live for
    /// `'static`, so the fixture is module-scoped rather than declared
    /// inside the test body. This proves a `&'static dyn
    /// HarnessAdapter` round-trips through the same `phf::Map::get` →
    /// `Option::copied` path that [`lookup`] uses, without needing to
    /// mutate the real table (which is compile-time and intentionally
    /// unreachable from tests).
    static FAKE_EAGER_REGISTRY: phf::Map<&'static str, &'static dyn HarnessAdapter> = phf_map! {
        "fake" => &FAKE,
    };

    /// Lookup-by-name on a static fake adapter. Mirrors what `lookup`
    /// does internally and what the claude Wave 2 PR will rely on
    /// once it registers `claude` in [`EAGER_ADAPTERS`].
    #[test]
    fn dyn_adapter_round_trip_by_name() {
        let got = FAKE_EAGER_REGISTRY
            .get("fake")
            .copied()
            .expect("fake registered");
        assert_eq!(got.name(), "fake");
        assert_eq!(got.session_root(), PathBuf::from("/tmp/fake-sessions"));

        assert!(FAKE_EAGER_REGISTRY.get("missing").is_none());
    }

    /// On this branch the production registry is intentionally empty;
    /// Wave 2 PRs (claude/codex/opencode) flip this to the `["claude",
    /// "codex", "opencode"]` shape the TS sibling already ships. Once
    /// those merge, update [`EXPECTED_HARNESS_NAMES`] below to the
    /// post-Wave-2 contract.
    ///
    /// This test pins the **deterministic ordering** contract for
    /// [`list_harness_names`]: callers (CLI `--help`, "did you mean"
    /// diagnostics) get the same sequence on every run, regardless of
    /// `HashMap`'s HashDoS-randomized iteration order. The contract is
    /// "eager `phf::Map` order, then [`RUNTIME_ADAPTER_NAMES`] order"
    /// — both compile-time fixed.
    ///
    /// **Cold-start proof.** This test passes without [`lookup`]
    /// being called, so [`RUNTIME_ADAPTERS`] is never dereferenced.
    /// Inspection of [`list_harness_names`] confirms it reads
    /// [`RUNTIME_ADAPTER_NAMES`] (a plain `&[&'static str]`) and never
    /// touches the `LazyLock`. Wave 2 adapters added to
    /// [`RUNTIME_ADAPTERS`] must not change this property — i.e. don't
    /// reach for `RUNTIME_ADAPTERS.keys()` here later.
    #[test]
    fn list_harness_names_is_deterministic() {
        /// Snapshot of the expected harness ordering. Wave 2 D5 (#248-d)
        /// landed claude in `EAGER_ADAPTERS`; codex (#248-e) and
        /// opencode (#248-f) will append their runtime entries here.
        const EXPECTED_HARNESS_NAMES: &[&str] = &[
            "claude",   // #248-d (eager)
            "codex",    // #248-e (runtime)
            "opencode", // #248-f (runtime)
        ];

        let names = list_harness_names();
        assert_eq!(
            names, EXPECTED_HARNESS_NAMES,
            "harness ordering drifted; if a Wave 2 adapter just landed, \
             update EXPECTED_HARNESS_NAMES to match the new contract"
        );

        // Calling twice yields identical output — guards against any
        // future regression that swaps RUNTIME_ADAPTER_NAMES for a
        // hashed source.
        assert_eq!(
            list_harness_names(),
            names,
            "list_harness_names must be deterministic across calls"
        );
    }

    /// Sync invariant: every name advertised in
    /// [`RUNTIME_ADAPTER_NAMES`] must resolve through [`lookup`] to an
    /// adapter actually registered in [`RUNTIME_ADAPTERS`]. This
    /// catches the "uncomment one row, forget the other" footgun
    /// Wave 2 PRs face — see the doc comment on
    /// [`RUNTIME_ADAPTER_NAMES`].
    ///
    /// On this branch both lists are empty, so the loop is a no-op
    /// but the contract is pinned for Wave 2.
    #[test]
    fn runtime_adapter_names_match_runtime_adapters() {
        for name in RUNTIME_ADAPTER_NAMES.iter().copied() {
            assert!(
                lookup(name).is_some(),
                "RUNTIME_ADAPTER_NAMES advertises {name:?} but RUNTIME_ADAPTERS \
                 has no entry for it; the two lists must stay in lockstep"
            );
        }
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("nope").is_none());
        assert!(lookup("").is_none());
        assert!(lookup("claude ").is_none());
    }

    /// Wave 2 wiring proof for **pending-stamp** adapters: a
    /// pending-stamp adapter built via
    /// [`pending_stamp::adapter_static`] satisfies the `&'static dyn
    /// HarnessAdapter` value bound that [`RUNTIME_ADAPTERS`] requires.
    /// This is the regression test that would have caught the
    /// architectural mismatch where the registry stored `&'static`
    /// but the factory returned a runtime `Box`.
    ///
    /// The static here mirrors the shape codex/opencode will use in
    /// production once their slots in [`RUNTIME_ADAPTERS`] are
    /// populated:
    ///
    /// ```ignore
    /// static CODEX_ADAPTER: LazyLock<&'static dyn HarnessAdapter> =
    ///     LazyLock::new(|| pending_stamp::adapter_static(...));
    /// ```
    ///
    /// We use the same value type (`HashMap<&'static str, &'static
    /// dyn HarnessAdapter>` wrapped in `LazyLock`) that production
    /// uses for [`RUNTIME_ADAPTERS`], so this test exercises the
    /// actual production wiring path: if `adapter_static`'s return
    /// type stopped fitting the runtime registry's value bound, this
    /// test would fail to compile.
    static FAKE_PENDING_STAMP_ADAPTER: LazyLock<&'static dyn HarnessAdapter> =
        LazyLock::new(|| {
            let session_root: Arc<dyn Fn() -> PathBuf + Send + Sync> =
                Arc::new(|| PathBuf::from("/tmp/codex-sessions"));
            let ingest_sessions: IngestSessionsFn =
                Arc::new(|_ledger_home| Box::pin(async { Ok(IngestReport::default()) }));
            pending_stamp::adapter_static(PendingStampAdapter::new(
                "codex",
                session_root,
                ingest_sessions,
            ))
        });

    /// Module-scoped runtime fake registry. Same value type as the
    /// production [`RUNTIME_ADAPTERS`] above, so this fixture
    /// asserts the leaked reference fits the *exact* container shape
    /// codex/opencode will land in.
    static FAKE_RUNTIME_REGISTRY: LazyLock<
        std::collections::HashMap<&'static str, &'static dyn HarnessAdapter>,
    > = LazyLock::new(|| {
        let mut m = std::collections::HashMap::new();
        m.insert("codex", *FAKE_PENDING_STAMP_ADAPTER);
        m
    });

    #[test]
    fn pending_stamp_adapter_static_fits_runtime_registry() {
        // Lookup goes through the same `HashMap::get → Option::copied`
        // path that `lookup` uses for the runtime tier. If
        // `adapter_static` ever stopped returning `&'static dyn
        // HarnessAdapter` (e.g. regressed to `Box<dyn HarnessAdapter>`),
        // construction of `FAKE_RUNTIME_REGISTRY` would fail to compile.
        let got = FAKE_RUNTIME_REGISTRY
            .get("codex")
            .copied()
            .expect("codex registered");
        assert_eq!(got.name(), "codex");
        assert_eq!(got.session_root(), PathBuf::from("/tmp/codex-sessions"));
        assert!(FAKE_RUNTIME_REGISTRY.get("opencode").is_none());
    }

    /// Compile-time proof that
    /// [`pending_stamp::adapter_static`]'s return type is exactly
    /// the value bound the runtime registry holds. This is a stricter
    /// check than the runtime test above: even if a hypothetical
    /// future refactor accidentally made the runtime test pass via
    /// some implicit conversion, this assertion would still fire if
    /// the types diverged.
    ///
    /// The bound is enforced by storing the function pointer in a
    /// const with the explicit signature; mismatched types cause a
    /// compile error here, not a test failure.
    const _ASSERT_ADAPTER_STATIC_FITS_REGISTRY: fn(
        PendingStampAdapter,
    ) -> &'static dyn HarnessAdapter = pending_stamp::adapter_static;
}
