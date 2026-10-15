//! `relayburn-cli` library surface: the internal modules, exposed so they
//! can be unit-tested with `cargo test -p relayburn-cli` and reached from
//! integration tests under `tests/`.

// Seal the test process away from the user's real stores before any test runs.
#[cfg(test)]
use relayburn_test_seal as _;

pub mod util;
