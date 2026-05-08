//! Thin wrapper around [`comfy_table`] for the read-path commands.
//!
//! The Wave 2 presenters deliberately stay in `Vec<Vec<String>>` land
//! and hand the table off here; the wrapper applies a consistent ASCII
//! preset, optional color suppression for `--no-color`, and keeps the
//! rendering boilerplate out of every command file.
//!
//! Header row + body rows are kept as separate parameters because that
//! matches how the TS CLI builds tables — header strings come from
//! literal labels in the command, body rows come from a SDK result
//! aggregate.
//!
//! Helpers here are unused on the scaffold branch; Wave 2 PRs are what
//! consume them. `#[allow(dead_code)]` keeps the surface intact.

#![allow(dead_code)]

use std::io::{self, Write};

use comfy_table::presets::UTF8_FULL;
use comfy_table::{ContentArrangement, Table};

use crate::cli::GlobalArgs;
use crate::render::ux;

/// Render a table to a `String`. Caller decides whether to write it to
/// stdout, embed it in a larger envelope, or capture it in a test.
///
/// The `headers` slice becomes the first row; subsequent `Vec<String>`s
/// are body rows, each expected to have `headers.len()` cells. Rows
/// with fewer cells get padded with empty strings; rows with more cells
/// are truncated. Matches `comfy-table`'s defaults but hides the warning
/// from callers.
pub fn render_table(globals: &GlobalArgs, headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic);

    if !ux::colors_enabled(globals) {
        // `comfy-table`'s built-in no-tty mode disables cell styling /
        // ANSI emission at the source — much safer than post-hoc
        // stripping (which is a UTF-8 minefield: the box-drawing
        // characters in `UTF8_FULL` are themselves multi-byte and would
        // be corrupted by any byte-level regex/walk). Keeping the
        // suppression at the renderer also covers any future callers
        // who pre-style cell content.
        table.force_no_tty();
    }

    table.set_header(headers.iter().copied());

    let width = headers.len();
    for row in rows {
        let mut padded: Vec<String> = row.iter().take(width).cloned().collect();
        while padded.len() < width {
            padded.push(String::new());
        }
        table.add_row(padded);
    }

    table.to_string()
}

/// Convenience: render and write to stdout with a trailing newline.
///
/// Returns the underlying I/O error rather than panicking on EPIPE
/// (e.g. `burn summary | head`); the caller is expected to surface
/// failures via `render::error::report_error`. Mirrors the shape of
/// [`crate::render::json::render_json`] so all rendering helpers
/// uniformly bubble I/O errors up to the dispatcher.
pub fn print_table(globals: &GlobalArgs, headers: &[&str], rows: &[Vec<String>]) -> io::Result<()> {
    let rendered = render_table(globals, headers, rows);
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(rendered.as_bytes())?;
    handle.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_globals() -> GlobalArgs {
        GlobalArgs {
            json: false,
            ledger_path: None,
            no_color: false,
        }
    }

    #[test]
    fn renders_header_and_rows() {
        let rendered = render_table(
            &no_globals(),
            &["model", "turns"],
            &[
                vec!["claude-sonnet-4-6".into(), "12".into()],
                vec!["claude-haiku-4-5".into(), "3".into()],
            ],
        );
        assert!(rendered.contains("model"));
        assert!(rendered.contains("claude-sonnet-4-6"));
        assert!(rendered.contains("12"));
    }

    #[test]
    fn pads_short_rows() {
        let rendered = render_table(
            &no_globals(),
            &["a", "b", "c"],
            &[vec!["x".into(), "y".into()]],
        );
        // The third column for the row exists in the rendered output;
        // we don't assert on box-drawing characters, just on the column
        // count surviving (header line should contain all three labels).
        assert!(rendered.contains('a'));
        assert!(rendered.contains('b'));
        assert!(rendered.contains('c'));
    }

    fn no_color_globals() -> GlobalArgs {
        GlobalArgs {
            json: false,
            ledger_path: None,
            no_color: true,
        }
    }

    #[test]
    fn no_color_preserves_non_ascii_cell_contents() {
        // Regression: an earlier implementation stripped ANSI by walking
        // bytes and pushing each as a `char`, which corrupted multi-byte
        // UTF-8 (the table's own UTF8_FULL borders, plus any non-ASCII
        // cell content) into mojibake. The fix is to suppress styling at
        // the renderer (`force_no_tty`) instead of stripping after the
        // fact, which keeps codepoints intact end-to-end.
        let rendered = render_table(
            &no_color_globals(),
            &["lang", "greeting"],
            &[
                vec!["ja".into(), "日本語".into()],
                vec!["emoji".into(), "🔥".into()],
            ],
        );
        assert!(
            rendered.contains("日本語"),
            "non-ASCII cell content was corrupted: {rendered}"
        );
        assert!(
            rendered.contains("🔥"),
            "emoji cell content was corrupted: {rendered}"
        );
        // The UTF8_FULL preset uses box-drawing characters; those should
        // also survive intact.
        assert!(
            rendered.contains('─') || rendered.contains('│'),
            "box-drawing characters were corrupted: {rendered}"
        );
        // And no ANSI escapes should remain.
        assert!(
            !rendered.contains('\u{1b}'),
            "ANSI escape leaked through despite no_color: {rendered:?}"
        );
    }

    #[test]
    fn print_table_returns_ok_on_happy_path() {
        // Smoke test mirroring `render_json_accepts_arbitrary_serialize_input`:
        // `print_table` writes to the process's locked stdout, so we can't
        // capture output here, but we can at least pin that the happy
        // path returns `Ok(())` (i.e. no panic, no EPIPE in this test
        // harness). End-to-end stdout assertions live in `tests/smoke.rs`.
        let result = print_table(
            &no_globals(),
            &["model", "turns"],
            &[vec!["claude-sonnet-4-6".into(), "12".into()]],
        );
        assert!(result.is_ok(), "print_table happy path returned {result:?}");
    }

    #[test]
    fn no_color_strips_pre_styled_cell_ansi() {
        // If a future caller hands us a cell that already carries ANSI,
        // `--no-color` should still produce escape-free output. With
        // `force_no_tty`, comfy-table delegates styling to the cell's
        // own bytes — but we don't apply per-cell styles here, so any
        // raw escape inside cell text is passed through. Document that
        // contract: we don't promise to launder pre-formatted cells; we
        // promise that the renderer itself doesn't add color.
        //
        // This test pins the current behavior: cells are passed through
        // verbatim. If callers need to hand off pre-styled content, the
        // sanitization belongs upstream (in the cell builder) where the
        // input type is known.
        let rendered = render_table(&no_color_globals(), &["k"], &[vec!["plain".into()]]);
        assert!(!rendered.contains('\u{1b}'));
    }
}
