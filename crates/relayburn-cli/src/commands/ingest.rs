//! `burn ingest` — passive-ingest entrypoint. No flags scans every
//! known session store once; `--watch` keeps polling; `--hook claude
//! --quiet` is the stdin-driven Claude hook path.
//!
//! Stub. Wave 2 D8 wires this up over the `relayburn_sdk::ingest_all`
//! verb plus the `relayburn_sdk` watch-loop primitives. TS source of
//! truth: `packages/cli/src/commands/ingest.ts` plus
//! `packages/ingest/src/watch-loop.ts`.

use crate::cli::GlobalArgs;

pub fn run(globals: &GlobalArgs) -> i32 {
    super::not_yet_implemented("ingest", globals)
}
