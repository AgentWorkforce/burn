//! `burn overhead` (and `burn overhead trim`) — estimate context
//! overhead and optionally surface trim recommendations.
//!
//! Thin presenter over `relayburn_sdk::overhead` and
//! `relayburn_sdk::overhead_trim`. TS source of truth:
//! `packages/cli/src/commands/overhead.ts`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use relayburn_sdk::{
    describe_applies_to, overhead as sdk_overhead, overhead_trim as sdk_overhead_trim,
    OverheadFileSummary, OverheadOptions, OverheadPerFileEntry, OverheadResult,
    OverheadSectionCost, OverheadTrimOptions, OverheadTrimResult,
};

use crate::cli::{GlobalArgs, OverheadAction, OverheadArgs};
use crate::render::error::report_error;

pub fn run(globals: &GlobalArgs, args: OverheadArgs) -> i32 {
    match args.action {
        Some(OverheadAction::Trim(trim)) => run_trim(globals, args.project, args.since, args.kind, trim.top),
        None => run_report(globals, args.project, args.since, args.kind),
    }
}

fn run_report(
    globals: &GlobalArgs,
    project: Option<PathBuf>,
    since: Option<String>,
    kind: Option<crate::cli::OverheadKind>,
) -> i32 {
    let project_path = resolve_project(project.as_deref());
    let opts = OverheadOptions {
        project: Some(project_path.clone()),
        since: since.clone(),
        kind: kind.map(Into::into),
        ledger_home: globals.ledger_path.clone(),
    };
    let result = match sdk_overhead(opts) {
        Ok(r) => r,
        Err(err) => return report_error(&err, globals),
    };

    if result.files.is_empty() {
        let msg = match kind {
            Some(k) => format!(
                "no {} overhead files found at {}\n",
                kind_to_str(k),
                project_path.display()
            ),
            None => format!(
                "no overhead files found at {} (looked for CLAUDE.md, .claude/CLAUDE.md, AGENTS.md)\n",
                project_path.display()
            ),
        };
        let _ = io::stderr().write_all(msg.as_bytes());
        return 1;
    }

    if globals.json {
        if let Err(err) = render_json_ts_compatible(&result) {
            return report_error(&err, globals);
        }
        return 0;
    }

    if let Err(err) = render_human_report(&result, since.as_deref()) {
        return report_error(&err, globals);
    }
    0
}

fn run_trim(
    globals: &GlobalArgs,
    project: Option<PathBuf>,
    since: Option<String>,
    kind: Option<crate::cli::OverheadKind>,
    top: Option<u64>,
) -> i32 {
    let project_path = resolve_project(project.as_deref());
    let opts = OverheadTrimOptions {
        project: Some(project_path.clone()),
        since: since.clone(),
        kind: kind.map(Into::into),
        ledger_home: globals.ledger_path.clone(),
        top,
        include_diff: None,
    };
    let result = match sdk_overhead_trim(opts) {
        Ok(r) => r,
        Err(err) => return report_error(&err, globals),
    };

    if result.summary.files_analyzed == 0 {
        let msg = match kind {
            Some(k) => format!(
                "no {} overhead files found at {}\n",
                kind_to_str(k),
                project_path.display()
            ),
            None => format!(
                "no overhead files found at {} (looked for CLAUDE.md, .claude/CLAUDE.md, AGENTS.md)\n",
                project_path.display()
            ),
        };
        let _ = io::stderr().write_all(msg.as_bytes());
        return 1;
    }

    if globals.json {
        if let Err(err) = render_json_ts_compatible(&result) {
            return report_error(&err, globals);
        }
        return 0;
    }

    if let Err(err) = render_human_trim(&result) {
        return report_error(&err, globals);
    }
    0
}

/// Serialize `value` as pretty-printed JSON with TS-compatible numeric output:
/// integer-valued `f64`s print as bare integers (`0` not `0.0`), matching
/// `JSON.stringify` semantics so the golden snapshots stay byte-equivalent.
fn render_json_ts_compatible<T: serde::Serialize + ?Sized>(value: &T) -> io::Result<()> {
    let mut json = serde_json::to_value(value).map_err(io::Error::other)?;
    coerce_integer_floats(&mut json);
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(&mut handle, &json).map_err(io::Error::other)?;
    handle.write_all(b"\n")?;
    handle.flush()
}

/// Walk a `serde_json::Value` tree and convert any `Number(f64)` whose value
/// is finite and exactly representable as `i64`/`u64` into the integer form.
/// Mirrors JavaScript's `JSON.stringify` numeric output, which always prints
/// `0` for `0.0_f64`.
///
/// `Number::as_f64()` is lossy for exact integers above `2^53`, so the
/// `is_f64()` guard short-circuits before we ever touch a Number that
/// already serializes as an integer. Without the guard a u64 like
/// `9_007_199_254_740_993` (2^53 + 1) would silently become
/// `9_007_199_254_740_992` after a `to_value` → coerce → `to_writer_pretty`
/// round-trip.
fn coerce_integer_floats(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Number(n) => {
            // Only coerce JSON Numbers that round-tripped as floats.
            // `is_f64()` returns false for exact-integer Numbers (i64/u64),
            // which already render without a decimal point and need no work.
            if !n.is_f64() {
                return;
            }
            if let Some(f) = n.as_f64() {
                if f.is_finite() && f.fract() == 0.0 {
                    if f >= 0.0 && f <= u64::MAX as f64 {
                        let u = f as u64;
                        if (u as f64) == f {
                            *n = serde_json::Number::from(u);
                        }
                    } else if f >= i64::MIN as f64 && f < 0.0 {
                        let i = f as i64;
                        if (i as f64) == f {
                            *n = serde_json::Number::from(i);
                        }
                    }
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                coerce_integer_floats(item);
            }
        }
        serde_json::Value::Object(obj) => {
            for (_k, val) in obj.iter_mut() {
                coerce_integer_floats(val);
            }
        }
        _ => {}
    }
}

fn resolve_project(project: Option<&Path>) -> PathBuf {
    match project {
        Some(p) => match std::fs::canonicalize(p) {
            Ok(c) => c,
            Err(_) => {
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    std::env::current_dir()
                        .map(|cwd| cwd.join(p))
                        .unwrap_or_else(|_| p.to_path_buf())
                }
            }
        },
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

fn kind_to_str(k: crate::cli::OverheadKind) -> &'static str {
    match k {
        crate::cli::OverheadKind::ClaudeMd => "claude-md",
        crate::cli::OverheadKind::AgentsMd => "agents-md",
    }
}

// ---------------------------------------------------------------------------
// Human renderers
// ---------------------------------------------------------------------------

fn render_human_report(result: &OverheadResult, since: Option<&str>) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    let since_label = since.unwrap_or("all time").to_string();

    // Build an array of lines and join with `\n` to mirror the TS sibling's
    // `out.join('\n')` semantics — the trailing-newline shape is sensitive to
    // this and the byte-equivalence golden snapshots compare on it.
    let mut lines: Vec<String> = Vec::new();
    lines.push(String::new());
    lines.push(format!("Overhead files in {}:", result.project));
    lines.push(String::new());

    for file_attr in &result.per_file {
        let parsed_file = result.files.iter().find(|f| f.path == file_attr.path);
        if let Some(pf) = parsed_file {
            push_file_block(pf, file_attr, &since_label, &mut lines);
            lines.push(String::new());
        }
    }

    lines.push(format!(
        "Grand total (all overhead files, {since_label}): {}",
        format_usd(result.grand_total),
    ));
    lines.push(String::new());

    let joined = lines.join("\n");
    handle.write_all(joined.as_bytes())?;
    handle.flush()?;
    Ok(())
}

fn push_file_block(
    parsed: &OverheadFileSummary,
    fa: &OverheadPerFileEntry,
    since_label: &str,
    lines: &mut Vec<String>,
) {
    let display = format_file_display(&parsed.path);
    let applies_to = describe_applies_to(&parsed.applies_to);
    lines.push(format!(
        "{display} — {} lines, ~{} tokens — applies to: {applies_to}",
        format_int(parsed.total_lines),
        format_tokens(parsed.tokens),
    ));
    if parsed.tokens == 0 {
        lines.push("  (empty file — no attribution)".to_string());
        return;
    }
    let attribution = &fa.attribution;
    if attribution.session_count == 0 {
        lines.push("  no matching sessions in window.".to_string());
        return;
    }
    lines.push(format!(
        "  Cost per session:   avg {}, p95 {}",
        format_usd(attribution.per_session_avg),
        format_usd(attribution.per_session_p95),
    ));
    lines.push(format!(
        "  Cost over {since_label}: {} across {} session{}",
        format_usd(attribution.total_cost),
        format_int(attribution.session_count),
        if attribution.session_count == 1 { "" } else { "s" },
    ));
    lines.push("  Sections ranked by cost:".to_string());
    if attribution.section_costs.is_empty() {
        lines.push("    (no sections)".to_string());
        return;
    }
    let table = render_section_table(&attribution.section_costs);
    // Indent each table line by four spaces, mirroring `indent(text, '    ')`
    // in the TS sibling.
    let indented = table
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    lines.push(indented);
}

fn format_file_display(path: &str) -> String {
    let p = Path::new(path);
    let basename = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let rel = match p.strip_prefix(&cwd) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
        _ => path.to_string(),
    };
    format!("{basename} ({rel})")
}

fn render_section_table(rows: &[OverheadSectionCost]) -> String {
    let mut data: Vec<Vec<String>> = Vec::with_capacity(rows.len() + 1);
    data.push(vec![
        "lines".to_string(),
        "heading".to_string(),
        "tokens".to_string(),
        "cost/session".to_string(),
        "%file".to_string(),
    ]);
    for r in rows {
        data.push(vec![
            format_line_range(r.section.start_line, r.section.end_line),
            r.section.heading.clone(),
            format_tokens(r.section.tokens),
            format_usd(r.cost_per_session),
            format!("{:.1}%", r.token_share * 100.0),
        ]);
    }
    table_rows(&data)
}

/// Mirror of the TS `format.ts::table` helper: pad each cell to the
/// widest cell in its column with two spaces between columns, trim
/// trailing whitespace per line.
fn table_rows(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if cell.chars().count() > widths[i] {
                widths[i] = cell.chars().count();
            }
        }
    }
    let mut out = String::new();
    for (ridx, row) in rows.iter().enumerate() {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            // Pad to width if not last cell (so trailing whitespace can be
            // trimmed away from the rightmost cell).
            let pad = widths[i].saturating_sub(cell.chars().count());
            if i < row.len() - 1 {
                for _ in 0..pad {
                    line.push(' ');
                }
            }
        }
        // Match TS `.trimEnd()` — strip trailing whitespace per row.
        let trimmed = line.trim_end();
        out.push_str(trimmed);
        if ridx + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

fn render_human_trim(result: &OverheadTrimResult) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();

    if result.recommendations.is_empty() {
        return handle.write_all("# no trim candidates — overhead files have no headed sections\n".as_bytes());
    }

    // Group by `file` while preserving insertion order.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<&relayburn_sdk::OverheadTrimRecommendation>> =
        std::collections::HashMap::new();
    for rec in &result.recommendations {
        groups
            .entry(rec.file.clone())
            .or_insert_with(|| {
                order.push(rec.file.clone());
                Vec::new()
            })
            .push(rec);
    }

    // Mirror the TS sibling: build an array of lines/blocks and join with `\n`
    // so the trailing blank-vs-no-blank semantics match byte-for-byte.
    let mut lines: Vec<String> = Vec::new();
    lines.push("# burn overhead trim — projected savings if trimmed".to_string());
    lines.push("# (recommendations only; burn never modifies your overhead files)".to_string());
    lines.push(String::new());
    for file in &order {
        let recs = &groups[file];
        let first = recs[0];
        let basename = Path::new(&first.file)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| first.file.clone());
        let applies_to = describe_applies_to(&first.applies_to);
        lines.push(format!("# === {basename} (applies to: {applies_to}) ==="));
        lines.push(String::new());
        for rec in recs {
            lines.push(rec.diff.clone().unwrap_or_default());
            lines.push(String::new());
        }
    }

    let joined = lines.join("\n");
    handle.write_all(joined.as_bytes())?;
    handle.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Number / line-range formatters — mirror packages/cli/src/format.ts.
// ---------------------------------------------------------------------------

fn format_usd(n: f64) -> String {
    if n == 0.0 {
        return "$0.00".to_string();
    }
    if n < 0.01 {
        return format!("${n:.4}");
    }
    if n < 1.0 {
        return format!("${n:.3}");
    }
    format!("${n:.2}")
}

fn format_int(n: u64) -> String {
    // en-US locale grouping: every three digits separated by `,`.
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1000 {
        let v = tokens as f64 / 1000.0;
        return format!("{v:.1}k");
    }
    tokens.to_string()
}

fn format_line_range(start: u64, end: u64) -> String {
    let s = format!("{start:>4}");
    let e = format!("{end:>4}");
    format!("{s}-{e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_usd_buckets_match_ts() {
        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(0.001), "$0.0010");
        assert_eq!(format_usd(0.5), "$0.500");
        assert_eq!(format_usd(12.345), "$12.35");
    }

    #[test]
    fn format_int_groups_thousands() {
        assert_eq!(format_int(0), "0");
        assert_eq!(format_int(999), "999");
        assert_eq!(format_int(1_000), "1,000");
        assert_eq!(format_int(1_234_567), "1,234,567");
    }

    #[test]
    fn format_tokens_collapses_kilos() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(2_500), "2.5k");
    }

    #[test]
    fn format_line_range_pads_to_four() {
        assert_eq!(format_line_range(7, 11), "   7-  11");
        assert_eq!(format_line_range(100, 200), " 100- 200");
    }

    #[test]
    fn table_rows_pads_columns() {
        let rendered = table_rows(&[
            vec!["a".into(), "bb".into()],
            vec!["ccc".into(), "d".into()],
        ]);
        assert_eq!(rendered, "a    bb\nccc  d");
    }

    // -- coerce_integer_floats ------------------------------------------------
    //
    // The helper walks a `serde_json::Value` tree and converts whole-valued
    // floats (e.g. `0.0`) into their integer forms so the pretty-printed JSON
    // matches `JSON.stringify`'s "0" not "0.0" rendering. The cases below
    // pin down its core invariants — the most important one being that it
    // never lossily coerces an exact-integer JSON Number that happens to be
    // larger than `2^53`.

    #[test]
    fn coerce_integer_floats_leaves_exact_integers_untouched() {
        // `Number::from(u64)` produces an exact-integer Number for which
        // `is_f64()` returns false. Mutation must short-circuit so a
        // round-trip through `to_value` is a no-op for integer fields.
        let mut v = serde_json::json!({ "n": 42_u64 });
        coerce_integer_floats(&mut v);
        assert_eq!(v, serde_json::json!({ "n": 42_u64 }));
    }

    #[test]
    fn coerce_integer_floats_preserves_large_u64_above_2_pow_53() {
        // `2^53 + 1 = 9_007_199_254_740_993` — exactly representable as u64
        // but NOT as f64. Without the `is_f64()` guard the value would round
        // down to 2^53 = 9_007_199_254_740_992 after a lossy `as_f64()` cast.
        let huge: u64 = (1u64 << 53) + 1;
        let mut v = serde_json::json!({ "n": huge });
        coerce_integer_floats(&mut v);
        // Round-trip through to_string to assert the byte representation
        // wasn't truncated by an unguarded f64 coercion.
        let s = serde_json::to_string(&v).expect("serialize");
        assert!(
            s.contains(&huge.to_string()),
            "expected `{huge}` in `{s}`; large u64 was lossily coerced",
        );
    }

    #[test]
    fn coerce_integer_floats_collapses_whole_valued_floats() {
        // `42.0_f64` parsed through `serde_json::Number::from_f64` is the
        // case the helper exists to handle. After coercion the Number must
        // serialize as the integer form, matching `JSON.stringify`.
        let n = serde_json::Number::from_f64(42.0).expect("finite");
        let mut v = serde_json::Value::Number(n);
        coerce_integer_floats(&mut v);
        assert_eq!(serde_json::to_string(&v).expect("serialize"), "42");
    }

    #[test]
    fn coerce_integer_floats_recurses_into_arrays_and_objects() {
        // Smoke-cover the recursion arms — a nested whole-valued float
        // inside an array inside an object should still flatten to its
        // integer form.
        let inner = serde_json::Number::from_f64(7.0).expect("finite");
        let mut v = serde_json::json!({ "outer": [serde_json::Value::Number(inner)] });
        coerce_integer_floats(&mut v);
        assert_eq!(serde_json::to_string(&v).expect("serialize"), r#"{"outer":[7]}"#);
    }
}
