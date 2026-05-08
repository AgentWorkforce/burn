//! `relayburn-cli` library surface.
//!
//! The CLI ships as a binary (`burn`) backed by `src/main.rs`. This
//! `lib.rs` exists so internal modules can be unit-tested with `cargo
//! test -p relayburn-cli` and so future integration tests under `tests/`
//! can reach the harness substrate without re-declaring the module tree.
//!
//! Today the only public surface here is [`harnesses`] — legacy adapter
//! reference code plus the shared pending-stamp adapter factory introduced
//! in #248-b. Runtime launcher integrations should prefer the public
//! `relayburn-sdk` / `@relayburn/sdk` pending-stamp APIs.
//!
//! Keeping this surface as a library crate alongside the binary lets the
//! Wave 2 fan-out PRs add per-adapter modules and unit tests without
//! disturbing `main.rs`.

pub mod harnesses;
pub mod util;
