//! `burn summary` — aggregate session usage and cost.
//!
//! Stub. Wave 2 D1 wires this up as a thin presenter over
//! `relayburn_sdk::summary` (and its `--by-provider` / `--by-tool` /
//! `--by-subagent-type` / `--by-relationship` / `--subagent-tree`
//! variants). See `packages/cli/src/commands/summary.ts` for the
//! canonical TS surface this should replicate.

use crate::cli::GlobalArgs;

pub fn run(globals: &GlobalArgs) -> i32 {
    super::not_yet_implemented("summary", globals)
}
