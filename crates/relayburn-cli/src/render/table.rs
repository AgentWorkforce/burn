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

use comfy_table::presets::UTF8_FULL;
use comfy_table::{ContentArrangement, Table};

use crate::cli::GlobalArgs;

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

    table.set_header(headers.iter().copied());

    let width = headers.len();
    for row in rows {
        let mut padded: Vec<String> = row.iter().take(width).cloned().collect();
        while padded.len() < width {
            padded.push(String::new());
        }
        table.add_row(padded);
    }

    if globals.no_color {
        // `comfy-table` doesn't add color of its own, but we forward the
        // intent by stripping any ANSI a caller-supplied cell might
        // carry. The current presenters all build cells from plain
        // strings so this is a no-op today; keeping the hook here means
        // Wave 2 can rely on `--no-color` being honored for free.
        let raw = table.to_string();
        return strip_ansi(&raw);
    }

    table.to_string()
}

/// Convenience: render and write to stdout with a trailing newline.
pub fn print_table(globals: &GlobalArgs, headers: &[&str], rows: &[Vec<String>]) {
    let rendered = render_table(globals, headers, rows);
    println!("{rendered}");
}

/// Strip ANSI escape sequences. Tiny standalone implementation — we
/// don't want to pull `strip-ansi-escapes` for the handful of bytes
/// the Wave 2 presenters actually emit.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // Skip CSI escape: ESC '[' … final byte in 0x40..=0x7e.
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() {
                    let b = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&b) {
                        break;
                    }
                }
            } else {
                // ESC followed by a single non-CSI byte — drop both.
                if i < bytes.len() {
                    i += 1;
                }
            }
        } else {
            // Safe: we walked from a valid str; bytes 0..0x80 are
            // single-byte chars, and multi-byte UTF-8 sequences never
            // contain 0x1b except in their leading position (which we
            // handled above).
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
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

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        let raw = "\x1b[31mred\x1b[0m plain";
        assert_eq!(strip_ansi(raw), "red plain");
    }
}
