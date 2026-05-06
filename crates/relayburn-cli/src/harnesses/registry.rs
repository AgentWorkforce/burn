//! Lazy harness registry — Rust port of `packages/cli/src/harnesses/registry.ts`.
//!
//! The TS sibling defers each adapter import (`async () => (await
//! import('./claude.js')).claudeAdapter`) so unrelated commands don't
//! pay ingest/ledger startup cost. The Rust port doesn't have lazy
//! module imports, but trait-object adapters are zero-sized so the
//! equivalent is "don't construct heavy state at registry build time".
//! All three Wave 2 adapters will be unit structs — adding them to the
//! `phf::Map` below is free.
//!
//! ## Why `phf` and not `OnceLock<HashMap<…>>`
//!
//! `phf::Map` is built at compile time; lookup is a single perfect-hash
//! probe with zero allocation. Cold-start matters here: `burn --help`
//! and `burn summary` should not pay any harness-table init cost.
//! `OnceLock<HashMap<…>>` is what we'd reach for if the table needed
//! runtime configuration (e.g. user-pluggable harnesses), which is not
//! on the roadmap.
//!
//! ## Wave 2 plug-in points
//!
//! Three slots are reserved below for the Wave 2 adapter PRs:
//!
//! * `claude` — #248-d (Wave 2 D5)
//! * `codex` — #248-e (Wave 2 D6)
//! * `opencode` — #248-f (Wave 2 D7)
//!
//! Each adds `pub mod claude;` (or codex / opencode) here and a single
//! row in [`ADAPTERS`]. The codex + opencode adapters are constructed
//! through [`super::pending_stamp::adapter`] so they share the manifest
//! + watch-loop wiring.

use phf::phf_map;

use super::HarnessAdapter;

/// Compile-time perfect-hash map from harness name to a `&'static dyn
/// HarnessAdapter`. Empty on this branch — populated by the three Wave 2
/// fan-out PRs (#248-d/e/f).
///
/// `&'static dyn HarnessAdapter` requires the value side to be a trait
/// object reference; `phf` supports that as long as the referent has a
/// `'static` lifetime, which works for stateless unit-struct adapters
/// or adapters defined as `static`s in their own module.
static ADAPTERS: phf::Map<&'static str, &'static dyn HarnessAdapter> = phf_map! {
    // Wave 2 PRs will populate these slots:
    //
    // "claude"   => &claude::CLAUDE_ADAPTER,        // #248-d
    // "codex"    => &codex::CODEX_ADAPTER,          // #248-e
    // "opencode" => &opencode::OPENCODE_ADAPTER,    // #248-f
};

/// Look up an adapter by name. Returns `None` for unknown names; the
/// `burn run` driver maps `None` to a "did you mean …?" diagnostic
/// using [`list_harness_names`].
pub fn lookup(name: &str) -> Option<&'static dyn HarnessAdapter> {
    ADAPTERS.get(name).copied()
}

/// List every registered harness name. The CLI's `--help` block reads
/// this so the harness list updates automatically when a new adapter is
/// registered. Order is the iteration order of `phf::Map` (stable but
/// not guaranteed alphabetical) — callers that want deterministic order
/// should sort the result, mirroring how the TS test sorts for
/// comparison.
pub fn list_harness_names() -> Vec<&'static str> {
    ADAPTERS.keys().copied().collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use async_trait::async_trait;
    use relayburn_sdk::IngestReport;

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

    /// Static fake registry shaped exactly like production [`ADAPTERS`].
    /// `phf_map!` requires its output to live for `'static`, so the
    /// fixture is module-scoped rather than declared inside the test
    /// body. This proves a `&'static dyn HarnessAdapter` round-trips
    /// through the same `phf::Map::get` → `Option::copied` path that
    /// [`lookup`] uses, without needing to mutate the real table (which
    /// is compile-time and intentionally unreachable from tests).
    static FAKE_REGISTRY: phf::Map<&'static str, &'static dyn HarnessAdapter> = phf_map! {
        "fake" => &FAKE,
    };

    /// Lookup-by-name on a static fake adapter. Mirrors what `lookup`
    /// does internally and what the Wave 2 PRs will rely on once they
    /// register `claude` / `codex` / `opencode`.
    #[test]
    fn dyn_adapter_round_trip_by_name() {
        let got = FAKE_REGISTRY
            .get("fake")
            .copied()
            .expect("fake registered");
        assert_eq!(got.name(), "fake");
        assert_eq!(got.session_root(), PathBuf::from("/tmp/fake-sessions"));

        assert!(FAKE_REGISTRY.get("missing").is_none());
    }

    /// On this branch the production registry is intentionally empty;
    /// Wave 2 PRs (claude/codex/opencode) flip this to the `["claude",
    /// "codex", "opencode"]` shape the TS sibling already ships. Once
    /// those merge, this test should be tightened to assert all three.
    #[test]
    fn list_is_empty_until_wave_2_adapters_land() {
        let names = list_harness_names();
        assert!(
            names.is_empty(),
            "expected registry to be empty on this branch, got {names:?}"
        );
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("nope").is_none());
        assert!(lookup("").is_none());
        assert!(lookup("claude ").is_none());
    }
}
