//! Per-subcommand presenter modules. Each `run` here is a stub that
//! reports `not yet implemented` (stderr in human mode, stdout JSON
//! envelope in `--json` mode) and returns a non-zero exit code.
//! Wave 2 fan-out PRs replace these stubs with thin presenters over
//! `relayburn-sdk`.
//!
//! Subcommands deliberately get one file each so the eight Wave 2 PRs
//! can land in parallel without touching a shared dispatcher table:
//!
//! - `summary`     — wraps `relayburn_sdk::summary`
//! - `hotspots`    — wraps `relayburn_sdk::hotspots`
//! - `overhead`    — wraps `relayburn_sdk::overhead` (+ `overhead trim`)
//! - `compare`     — wraps `relayburn_sdk::compare`
//! - `state`       — status / rebuild / prune / reset
//! - `ingest`      — no-flag, `--watch`, `--hook claude --quiet`
//! - `mcp_server`  — rmcp wrapper around the SDK query verbs
//!
//! `mod.rs` only re-exports submodules; do not add cross-command logic
//! here. Shared rendering helpers live in `crate::render`.

pub mod compare;
pub mod hotspots;
pub mod ingest;
pub mod mcp_server;
pub mod overhead;
pub mod sessions;
pub mod stamps;
pub mod state;
pub mod summary;
