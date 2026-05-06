//! TS-CLI-equivalent formatting helpers for human-rendered output.
//!
//! Wave 2 PRs need byte-for-byte parity with `packages/cli/src/format.ts`
//! (the `--json` mode handles its own shape via `serde_json`). The TS
//! helpers are tiny pure functions over numbers and `string[][]`; this
//! module mirrors them in Rust.
//!
//! - [`format_usd`] — money rendering with three rate-tier precisions.
//! - [`format_int`] — `toLocaleString('en-US')` thousands-separator output.
//! - [`render_table`] — pad-end columns separated by `"  "` (two spaces),
//!   `trim_end` each row, `\n`-joined. NOT the comfy-table preset; this
//!   is the spartan layout the TS CLI snapshots assume.

#![allow(dead_code)]

/// `formatUsd` from `packages/cli/src/format.ts`. Three rate tiers:
/// - `0`           → `$0.00`
/// - `< 0.01`      → `$<n.toFixed(4)>`
/// - `< 1`         → `$<n.toFixed(3)>`
/// - `>= 1`        → `$<n.toFixed(2)>`
pub fn format_usd(n: f64) -> String {
    if n == 0.0 {
        return "$0.00".to_string();
    }
    if n.abs() < 0.01 {
        return format!("${:.4}", n);
    }
    if n.abs() < 1.0 {
        return format!("${:.3}", n);
    }
    format!("${:.2}", n)
}

/// `formatInt` from `packages/cli/src/format.ts`. JS `Number.toLocaleString('en-US')`
/// uses thousands separators (`,`) for non-negative integers and a leading `-`
/// for negatives. We mirror that without pulling in a locale dep.
pub fn format_int(n: i64) -> String {
    if n < 0 {
        return format!("-{}", format_int_unsigned(n.unsigned_abs()));
    }
    format_int_unsigned(n as u64)
}

/// Convenience wrapper for unsigned token / count fields.
pub fn format_uint(n: u64) -> String {
    format_int_unsigned(n)
}

fn format_int_unsigned(n: u64) -> String {
    let raw = n.to_string();
    let bytes = raw.as_bytes();
    let len = bytes.len();
    if len <= 3 {
        return raw;
    }
    let mut out = String::with_capacity(len + (len - 1) / 3);
    let first = len % 3;
    if first > 0 {
        out.push_str(&raw[..first]);
        if len > first {
            out.push(',');
        }
    }
    let mut i = first;
    while i + 3 <= len {
        out.push_str(&raw[i..i + 3]);
        if i + 3 < len {
            out.push(',');
        }
        i += 3;
    }
    out
}

/// `table()` from `packages/cli/src/format.ts`. Each column gets padded to
/// the max width seen in that column; columns are joined with two spaces;
/// trailing whitespace is trimmed per row. The first row is the header.
///
/// The cell length is measured as the number of `char`s (Unicode code
/// points), not bytes — same as JS `String.prototype.length` (UTF-16 code
/// units), which for the CLI's actual output is equivalent. Multi-byte
/// glyphs like `—` (U+2014) count as 1.
pub fn render_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let w = cell.chars().count();
            if i >= widths.len() {
                widths.push(w);
            } else if widths[i] < w {
                widths[i] = w;
            }
        }
    }
    let mut out = String::new();
    for (ri, row) in rows.iter().enumerate() {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            // Pad with ASCII spaces to the column width. Skip padding the
            // last cell — TS uses padEnd then trimEnd, which leaves the
            // last column unpadded in the output.
            let target = widths[i];
            let cur = cell.chars().count();
            if cur < target {
                for _ in 0..(target - cur) {
                    line.push(' ');
                }
            }
        }
        let trimmed = line.trim_end();
        out.push_str(trimmed);
        if ri + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

/// Walk a `serde_json::Value` and rewrite any whole-number `f64` field
/// to its integer-shaped equivalent, matching JS's `JSON.stringify`
/// number formatting (which doesn't distinguish int from float). The
/// SDK uses `f64` for cost / token-share fields; left to its default
/// formatter, `serde_json` would emit `0.0` for them where the TS CLI
/// emits `0`. Recursive over arrays and objects.
pub fn coerce_whole_f64_to_int(v: &mut serde_json::Value) {
    use serde_json::{Number, Value};
    match v {
        Value::Number(n) => {
            if !n.is_f64() {
                return;
            }
            let Some(f) = n.as_f64() else {
                return;
            };
            if !(f.is_finite() && f.fract() == 0.0) {
                return;
            }
            if f >= 0.0 && f <= u64::MAX as f64 {
                let int = f as u64;
                // Round-trip check: skip cases where f64 → u64 → f64 lost
                // precision (e.g. very large doubles); leaving them as f64
                // is safer than producing a different integer.
                if int as f64 == f {
                    *n = Number::from(int);
                }
            } else if f < 0.0 && f >= i64::MIN as f64 {
                let int = f as i64;
                if int as f64 == f {
                    *n = Number::from(int);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                coerce_whole_f64_to_int(item);
            }
        }
        Value::Object(obj) => {
            for (_, val) in obj.iter_mut() {
                coerce_whole_f64_to_int(val);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_usd_zero() {
        assert_eq!(format_usd(0.0), "$0.00");
    }

    #[test]
    fn format_usd_under_one_cent() {
        assert_eq!(format_usd(0.0065), "$0.0065");
    }

    #[test]
    fn format_usd_under_one_dollar() {
        assert_eq!(format_usd(0.034), "$0.034");
        assert_eq!(format_usd(0.0335), "$0.034");
    }

    #[test]
    fn format_usd_above_one_dollar() {
        assert_eq!(format_usd(1.234), "$1.23");
    }

    #[test]
    fn format_int_thousands() {
        assert_eq!(format_int(0), "0");
        assert_eq!(format_int(999), "999");
        assert_eq!(format_int(1_000), "1,000");
        assert_eq!(format_int(5_100), "5,100");
        assert_eq!(format_int(19_500), "19,500");
        assert_eq!(format_int(1_000_000), "1,000,000");
    }

    #[test]
    fn format_int_negative() {
        assert_eq!(format_int(-1_500), "-1,500");
    }

    #[test]
    fn render_table_pads_columns_and_trims_trailing_space() {
        let rendered = render_table(&[
            vec!["model".into(), "turns".into()],
            vec!["claude-sonnet-4-6".into(), "12".into()],
            vec!["claude-haiku".into(), "3".into()],
        ]);
        let expected = "model              turns\nclaude-sonnet-4-6  12\nclaude-haiku       3";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn render_table_handles_em_dash_as_single_char_width() {
        let rendered = render_table(&[
            vec!["k".into(), "v".into()],
            vec!["x".into(), "—".into()],
        ]);
        // `—` (U+2014) is one codepoint; column width should be 1.
        assert_eq!(rendered, "k  v\nx  —");
    }

    #[test]
    fn render_table_returns_empty_for_empty_rows() {
        let rendered = render_table(&[]);
        assert_eq!(rendered, "");
    }
}
