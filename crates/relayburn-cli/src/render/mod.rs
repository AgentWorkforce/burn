//! Shared rendering helpers for the CLI's read-path commands.
//!
//! - [`table`] — thin wrapper around `comfy-table` for tabular output.
//! - [`json`] — `--json`-aware structured output writer.
//! - [`error`] — typed-error → stderr / exit-code mapping (with a
//!   JSON-mode envelope for `--json`).
//! - [`logging`] — opt-in structured diagnostics on stderr.
//! - [`progress`] — TTY-only spinners and prettier warning rendering.
//! - [`prompt`] — shared interactive prompt/select helpers.
//! - [`ux`] — status messages, headings, and color policy.
//!
//! Wave 2 PRs add per-command rendering helpers next to their command
//! file, but anything reusable across two or more commands belongs
//! here.

pub mod error;
pub mod format;
pub mod json;
pub mod logging;
pub mod progress;
pub mod prompt;
pub mod table;
pub mod ux;
