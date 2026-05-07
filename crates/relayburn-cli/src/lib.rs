//! `relayburn-cli` library surface.
//!
//! The CLI ships as a binary (`burn`) backed by `src/main.rs`. This
//! `lib.rs` exists so internal modules can be unit-tested with `cargo
//! test -p relayburn-cli` and so future integration tests under `tests/`
//! can reach the harness substrate without re-declaring the module tree.
//!
//! Today the only public surface here is [`harnesses`] — the `HarnessAdapter`
//! trait, the lazy registry, and the shared pending-stamp adapter factory
//! introduced in #248-b. Wave 2 PRs (claude / codex / opencode) will plug
//! their adapters in via [`harnesses::registry`]; the CLI binary will reach
//! them through `lookup` / `list_harness_names`.
//!
//! Keeping this surface as a library crate alongside the binary lets the
//! Wave 2 fan-out PRs add per-adapter modules and unit tests without
//! disturbing `main.rs`.

pub mod harnesses;
pub mod util;
