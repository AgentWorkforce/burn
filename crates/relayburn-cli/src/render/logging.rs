//! Structured logging bootstrap.
//!
//! `burn` keeps normal output quiet. Set `RELAYBURN_LOG=debug` (or `RUST_LOG`)
//! to enable compact structured diagnostics on stderr.

use std::io;

use tracing_subscriber::EnvFilter;

use crate::cli::GlobalArgs;
use crate::render::ux;

pub fn init(globals: &GlobalArgs) {
    let has_filter =
        std::env::var_os("RELAYBURN_LOG").is_some() || std::env::var_os("RUST_LOG").is_some();
    if !has_filter {
        return;
    }

    let filter = EnvFilter::try_from_env("RELAYBURN_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ux::colors_enabled(globals))
        .with_writer(io::stderr)
        .compact()
        .try_init();
}
