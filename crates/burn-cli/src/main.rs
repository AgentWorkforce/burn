//! `burn` CLI — Rust rewrite skeleton.
//!
//! Mirrors `packages/cli/src/` from the TypeScript workspace. Planned
//! command modules (filed as sub-issues under #222):
//!
//! - `commands::summary`         — `burn summary`
//! - `commands::hotspots`        — `burn hotspots`
//! - `commands::hotspots_session` — `burn hotspots --session`
//! - `commands::budget`          — `burn budget`
//! - `commands::budget_plans`    — `burn budget plans`
//! - `commands::compare`         — `burn compare`
//! - `commands::overhead`        — `burn overhead`
//! - `commands::run`             — `burn run <harness>`
//! - `commands::ingest`          — `burn ingest`
//! - `commands::mcp_server`      — `burn mcp-server`
//! - `commands::archive`         — `burn state rebuild archive`
//! - `commands::state`           — `burn state`
//! - `harnesses::{claude,codex,opencode}` — `HarnessAdapter` impls
//! - `harnesses::registry`       — lazy adapter registry
//! - `watch_loop`                — shared polling controller (was `watch-loop.ts`)

fn main() -> anyhow::Result<()> {
    eprintln!(
        "burn — Rust rewrite skeleton. See https://github.com/AgentWorkforce/burn/issues/222"
    );
    Ok(())
}
