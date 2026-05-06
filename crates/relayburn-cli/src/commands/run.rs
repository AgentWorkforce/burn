//! `burn run <harness>` — wrapper that spawns an agent CLI under a
//! `HarnessAdapter` and ingests its session log on exit.
//!
//! Stub. Wave 2 D5 wires this up using the `HarnessAdapter` trait +
//! lazy `phf` registry from #248-b. TS source of truth:
//! `packages/cli/src/commands/run.ts` plus the per-harness adapters
//! under `packages/cli/src/harnesses/`.

use crate::cli::GlobalArgs;

pub fn run(globals: &GlobalArgs) -> i32 {
    super::not_yet_implemented("run", globals)
}
