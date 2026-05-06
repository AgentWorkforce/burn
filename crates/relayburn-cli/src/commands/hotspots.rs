//! `burn hotspots` — surface high-cost / high-overhead hotspots from
//! the ledger.
//!
//! Stub. Wave 2 D1 wires this up as a thin presenter over
//! `relayburn_sdk::hotspots`. The TS source of truth is
//! `packages/cli/src/commands/hotspots.ts` (plus `hotspots-session.ts`
//! for the per-session drift / graph view).

use crate::cli::GlobalArgs;

pub fn run(globals: &GlobalArgs) -> i32 {
    super::not_yet_implemented("hotspots", globals)
}
