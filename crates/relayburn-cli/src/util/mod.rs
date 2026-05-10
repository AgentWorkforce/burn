//! Crate-internal utility modules shared between the binary and the
//! library tree (`harnesses/*`).
//!
//! Lives in `lib.rs` so both the `burn` binary (`commands/run.rs`) and
//! the library tree (`harnesses/claude.rs`) can reach the same helpers.
//! Keep this surface minimal — anything that can live in a single
//! call-site module should live there instead.

// `pub` (not `pub(crate)`) because `commands/run.rs` lives in the binary
// crate tree and reaches the helpers through the library crate's public
// surface (`relayburn_cli::util::time::*`). The library crate is consumed
// only by the in-repo binary + tests, so `pub` here is still effectively
// crate-private from a published-API standpoint.
pub mod home;
pub mod time;
