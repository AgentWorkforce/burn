//! `burn state` — inspect or rebuild derived state under
//! `~/.relayburn` (status, rebuild index | classify | content |
//! archive, prune, reset).
//!
//! Stub. Wave 2 D4 wires this up as a thin presenter over the
//! state-maintenance verbs on the SDK. TS source of truth:
//! `packages/cli/src/commands/state.ts`.

use crate::cli::GlobalArgs;

pub fn run(globals: &GlobalArgs) -> i32 {
    super::not_yet_implemented("state", globals)
}
