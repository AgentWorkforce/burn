//! Typed-error → stderr / exit-code mapping for the CLI.
//!
//! The SDK exposes `relayburn_sdk::LedgerError` (and a few sibling
//! typed errors). Wave 2 command handlers will end up with one of
//! three error shapes:
//!
//! - `relayburn_sdk::LedgerError` — typed; we match on it for stable
//!   exit codes.
//! - `anyhow::Error` — generic propagation from anywhere down the
//!   stack. Always falls through to a generic `2` exit code with the
//!   `Display` form of the error on stderr.
//! - `std::io::Error` — broken pipe / write-to-stdout failures from
//!   the rendering helpers. Mapped to exit code `2`, EPIPE silenced
//!   (matches Unix tools-as-citizen conventions).
//!
//! Every helper here writes to stderr in human mode and writes a
//! `{"error": "..."}` envelope to stdout in `--json` mode, then returns
//! the chosen exit code without calling `std::process::exit` itself —
//! the caller (`main::dispatch`) handles the actual exit so we keep
//! testability of the dispatch path.
//!
//! Several helpers below are unused on the scaffold branch (the Wave 2
//! presenter PRs are what call them). `#[allow(dead_code)]` keeps the
//! API surface intact without warnings until those PRs land.

#![allow(dead_code)]

use std::io::{self, Write};

use serde_json::json;

use crate::cli::GlobalArgs;
use relayburn_sdk::LedgerError;

/// Exit code for a typed `LedgerError`. Distinct from generic-error
/// `2` so shell scripts can branch on "ledger problem" vs "other".
pub const EXIT_LEDGER_ERROR: i32 = 3;
/// Exit code for a generic / unknown error path.
pub const EXIT_GENERIC_ERROR: i32 = 2;
/// Exit code for the `not yet implemented` stubs that ship in this PR.
/// Distinct from real-error codes so the smoke test (and callers
/// during the Wave 2 transition) can distinguish "not wired yet" from
/// "something is broken".
pub const EXIT_NOT_YET_IMPLEMENTED: i32 = 1;

/// Map a typed [`LedgerError`] to a stderr message + exit code, with a
/// JSON envelope when `globals.json` is set.
pub fn report_ledger_error(err: &LedgerError, globals: &GlobalArgs) -> i32 {
    report(globals, &err.to_string(), EXIT_LEDGER_ERROR)
}

/// Map any other error (anyhow, io, etc.) to a stderr message + exit
/// code. Use this when the error comes from a non-SDK boundary or when
/// the command handler chose to propagate as `anyhow::Error`.
pub fn report_error<E: std::fmt::Display>(err: &E, globals: &GlobalArgs) -> i32 {
    report(globals, &err.to_string(), EXIT_GENERIC_ERROR)
}

/// `not yet implemented` exit path used by every command stub in this
/// scaffold PR. Keeps the message format consistent across the
/// subcommands so the smoke test can assert on it without each command
/// inventing its own wording.
pub fn report_unimplemented(name: &str, globals: &GlobalArgs) -> i32 {
    let message = format!("burn {name}: not yet implemented");
    report(globals, &message, EXIT_NOT_YET_IMPLEMENTED)
}

/// Print an advisory warning without failing the run. Used when a flag
/// (e.g. `state rebuild archive --full`) is accepted for compatibility
/// but is a no-op in the 2.0 layout — the caller still proceeds with
/// the real rebuild path, but we want a stderr breadcrumb so scripts
/// don't silently get the wrong behaviour.
///
/// Writes to stderr in both human and `--json` modes. We deliberately
/// do NOT route this through the `--json` envelope: stdout in JSON
/// mode stays single-shape (the actual command result), and informative
/// warnings go to stderr where conventional Unix tools put them. The
/// stderr line is prefixed `burn: warning: ` so callers can grep for
/// it.
pub fn report_advisory(message: &str, _globals: &GlobalArgs) {
    let _ = writeln!(io::stderr(), "burn: warning: {message}");
}

/// Internal: do the actual stderr / JSON-envelope writing. Tolerates
/// I/O errors on the way out — if stderr is closed, the best we can
/// do is return the chosen exit code anyway.
fn report(globals: &GlobalArgs, message: &str, code: i32) -> i32 {
    if globals.json {
        let envelope = json!({ "error": message });
        let _ = write_json_envelope(&envelope);
    } else {
        let _ = writeln!(io::stderr(), "burn: {message}");
    }
    code
}

fn write_json_envelope(value: &serde_json::Value) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, value)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    handle.write_all(b"\n")?;
    handle.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_globals() -> GlobalArgs {
        GlobalArgs {
            json: true,
            ledger_path: None,
            no_color: false,
        }
    }

    fn human_globals() -> GlobalArgs {
        GlobalArgs {
            json: false,
            ledger_path: None,
            no_color: false,
        }
    }

    #[test]
    fn unimplemented_returns_exit_one() {
        // We can't easily capture stderr from a unit test without
        // adding plumbing; assert at least that the exit code matches
        // the documented constant.
        assert_eq!(
            report_unimplemented("summary", &human_globals()),
            EXIT_NOT_YET_IMPLEMENTED,
        );
        assert_eq!(
            report_unimplemented("summary", &json_globals()),
            EXIT_NOT_YET_IMPLEMENTED,
        );
    }

    #[test]
    fn generic_error_uses_exit_two() {
        let err = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        assert_eq!(report_error(&err, &human_globals()), EXIT_GENERIC_ERROR);
    }

    #[test]
    fn ledger_error_uses_exit_three() {
        let err = LedgerError::Other("ledger boom".into());
        assert_eq!(
            report_ledger_error(&err, &human_globals()),
            EXIT_LEDGER_ERROR,
        );
    }
}
