//! Fallible stdout writes for human-readable output.
//!
//! `print!` / `println!` panic with "failed printing to stdout" when a
//! downstream consumer closes the pipe early (`burn sessions list |
//! head`), exiting 101. These helpers write to the locked stdout handle
//! instead and return the I/O error marked with
//! [`crate::render::json::stdout_error`] so
//! [`crate::render::error::report_error`] recognizes the early-close as
//! a quiet exit-0, matching the `--json` behavior. File/FIFO writers
//! stay unmarked and remain failures.

use std::io::{self, Write};

use crate::render::json::stdout_error;

/// Write `text` to stdout exactly (no trailing newline is added).
/// `print!` shape; the caller owns all newlines in `text`.
pub fn write_stdout(text: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(text.as_bytes()).map_err(stdout_error)?;
    handle.flush().map_err(stdout_error)
}

/// Write `line` to stdout with a trailing newline. `println!` shape.
pub fn writeln_stdout(line: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(line.as_bytes()).map_err(stdout_error)?;
    handle.write_all(b"\n").map_err(stdout_error)?;
    handle.flush().map_err(stdout_error)
}
