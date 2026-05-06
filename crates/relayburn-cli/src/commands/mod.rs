//! Per-subcommand presenter modules. Each `run` here is a stub that
//! prints `not yet implemented` to stderr and returns a non-zero exit
//! code. Wave 2 fan-out PRs replace these stubs with thin presenters
//! over `relayburn-sdk`.
//!
//! Subcommands deliberately get one file each so the eight Wave 2 PRs
//! can land in parallel without touching a shared dispatcher table:
//!
//! - `summary`     — wraps `relayburn_sdk::summary`
//! - `hotspots`    — wraps `relayburn_sdk::hotspots`
//! - `overhead`    — wraps `relayburn_sdk::overhead` (+ `overhead trim`)
//! - `compare`     — wraps `relayburn_sdk::compare`
//! - `run`         — driver around `HarnessAdapter` (added in #248-b)
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
pub mod run;
pub mod state;
pub mod summary;

use crate::cli::GlobalArgs;
use crate::render::error::report_unimplemented;

/// Shared "not yet implemented" exit path for every subcommand stub.
/// Honors `--json` via [`crate::render::error::report_unimplemented`].
pub(crate) fn not_yet_implemented(name: &str, globals: &GlobalArgs) -> i32 {
    report_unimplemented(name, globals)
}
