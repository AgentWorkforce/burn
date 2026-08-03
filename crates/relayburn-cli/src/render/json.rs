//! `--json` output helper.
//!
//! Writes a serializable value to stdout as JSON, with a trailing
//! newline so shell pipelines see a clean line boundary. The TS CLI's
//! `--json` mode is exactly this: a single JSON document per
//! invocation, no leading garbage. Commands gate their human renderer
//! on `globals.json == false` and call [`render_json`] when it's `true`.
//!
//! Callers that need `JSON.stringify` numeric semantics (where
//! whole-valued `f64`s print as bare integers) should run their value
//! through [`crate::render::format::coerce_whole_f64_to_int`] first.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};

use serde::Serialize;

/// Render `value` as pretty-printed JSON to stdout with a trailing
/// newline. Returns `Ok(())` on success or the underlying I/O error
/// (which the caller should surface via `render::error::report_error`).
pub fn render_json<T: Serialize + ?Sized>(value: &T) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    write_json_pretty(&mut handle, value)?;
    handle.write_all(b"\n").map_err(stdout_error)?;
    handle.flush().map_err(stdout_error)
}

fn write_json_pretty<W: Write, T: Serialize + ?Sized>(writer: &mut W, value: &T) -> io::Result<()> {
    serde_json::to_writer_pretty(writer, value)
        .map_err(serde_error_to_io)
        .map_err(stdout_error)
}

#[derive(Debug)]
struct StdoutError(io::Error);

impl fmt::Display for StdoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Error for StdoutError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// Mark an I/O error as originating from the process stdout renderer while
/// retaining its kind. The marker lets shared reporting distinguish a normal
/// early-closing pipeline from an EPIPE raised by a file or FIFO writer.
pub(crate) fn stdout_error(err: io::Error) -> io::Error {
    let kind = err.kind();
    io::Error::new(kind, StdoutError(err))
}

pub(crate) fn is_stdout_broken_pipe(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::BrokenPipe
        && err
            .get_ref()
            .is_some_and(|source| source.is::<StdoutError>())
}

/// Convert a serde failure back to an I/O error without erasing the error
/// kind reported by the writer. In particular, callers rely on
/// `BrokenPipe` to treat an early-closing pipeline as a successful exit.
pub(crate) fn serde_error_to_io(err: serde_json::Error) -> io::Error {
    match err.io_error_kind() {
        Some(kind) => io::Error::new(kind, err),
        None => io::Error::other(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct BrokenPipeWriter;

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // Smoke test: the helper should accept anything `Serialize` and
    // not panic. Real I/O assertions live in the integration smoke
    // test under `tests/smoke.rs` which drives the binary end-to-end.
    #[test]
    fn render_json_accepts_arbitrary_serialize_input() {
        assert!(render_json(&json!({ "ok": true, "rows": [1, 2, 3] })).is_ok());
    }

    #[test]
    fn json_writer_preserves_broken_pipe_kind() {
        let err = write_json_pretty(&mut BrokenPipeWriter, &json!({ "ok": true }))
            .expect_err("writer should close early");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
