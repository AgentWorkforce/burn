//! `burn overhead` (and `burn overhead trim`) — estimate context
//! overhead and optionally surface trim recommendations.
//!
//! Stub. Wave 2 D2 wires this up as a thin presenter over
//! `relayburn_sdk::overhead` and `relayburn_sdk::overhead_trim`. TS
//! source of truth: `packages/cli/src/commands/overhead.ts`.

use crate::cli::GlobalArgs;

pub fn run(globals: &GlobalArgs) -> i32 {
    super::not_yet_implemented("overhead", globals)
}
