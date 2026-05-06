//! `burn compare <model_a,model_b[,...]>` — compare cost across two or
//! more models on the same workload.
//!
//! Stub. Wave 2 D3 wires this up as a thin presenter over
//! `relayburn_sdk::compare`. TS source of truth:
//! `packages/cli/src/commands/compare.ts`.

use crate::cli::GlobalArgs;

pub fn run(globals: &GlobalArgs) -> i32 {
    super::not_yet_implemented("compare", globals)
}
