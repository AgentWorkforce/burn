//! `--json` output helper.
//!
//! Writes a serializable value to stdout as JSON, with a trailing
//! newline so shell pipelines see a clean line boundary. The TS CLI's
//! `--json` mode is exactly this: a single JSON document per
//! invocation, no leading garbage. Wave 2 commands gate their human
//! renderer on `globals.json == false` and call [`render_json`] when
//! it's `true`.
//!
//! Helpers here are unused on the scaffold branch; Wave 2 PRs are what
//! consume them. `#[allow(dead_code)]` keeps the surface intact.

#![allow(dead_code)]

use std::io::{self, Write};

use serde::Serialize;

/// Render `value` as pretty-printed JSON to stdout with a trailing
/// newline. Returns `Ok(())` on success or the underlying I/O error
/// (which the caller should surface via `render::error::report_error`).
pub fn render_json<T: Serialize + ?Sized>(value: &T) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(&mut handle, value)
        .map_err(io::Error::other)?;
    handle.write_all(b"\n")?;
    handle.flush()
}

/// Compact-form variant. The TS CLI defaults to pretty-printed output
/// because `burn ... --json | jq` is the dominant invocation; compact
/// mode is here for embedded callers that want to pipe output through
/// `wc` or measure size budgets.
pub fn render_json_compact<T: Serialize + ?Sized>(value: &T) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, value)
        .map_err(io::Error::other)?;
    handle.write_all(b"\n")?;
    handle.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Smoke test: the helpers should accept anything `Serialize` and
    // not panic. Real I/O assertions live in the integration smoke
    // test under `tests/smoke.rs` which drives the binary end-to-end.
    #[test]
    fn render_json_accepts_arbitrary_serialize_input() {
        assert!(render_json(&json!({ "ok": true, "rows": [1, 2, 3] })).is_ok());
        assert!(render_json_compact(&json!({ "ok": true })).is_ok());
    }
}
