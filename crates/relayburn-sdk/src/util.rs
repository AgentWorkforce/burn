//! Crate-internal helpers shared across reader / analyze / query modules.
//!
//! Modules here are deliberately `pub(crate)`; they do not appear on the
//! published SDK surface. New helpers should live here only if they're
//! genuinely cross-module — single-module utilities belong with their
//! caller.

pub(crate) mod time;
