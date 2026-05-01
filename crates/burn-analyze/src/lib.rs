//! `burn-analyze` — pricing, per-record cost derivation, and comparison aggregator.
//!
//! Mirrors `packages/analyze/src/` from the TypeScript workspace. Planned
//! modules (filed as sub-issues under #222):
//!
//! - `pricing`        — vendored models.dev snapshot + lookup
//! - `cost`           — per-record cost derivation
//! - `compare`        — model-by-activity comparison aggregator
//! - `summary`        — totals roll-up (powers `burn summary`)
//! - `hotspots`       — file/command/subagent attribution + pattern detectors
//! - `overhead`       — CLAUDE.md / AGENTS.md attribution
//! - `budget`         — quota window + plan-cycle accounting
//!
//! Refresh `pricing` via the existing `pnpm run pricing:update` workflow,
//! committed as `crates/burn-analyze/src/pricing/snapshot.json`.

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_compiles() {}
}
