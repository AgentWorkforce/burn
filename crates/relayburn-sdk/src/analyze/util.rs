//! Shared formatting helpers for the analyze module.

/// Format a USD amount to 4 decimal places (`$0.1234`), matching the TS
/// finding adapters' money formatting.
pub(crate) fn fmt_usd(n: f64) -> String {
    format!("${n:.4}")
}

/// Format an integer with thousands separators, matching JS
/// `Number.prototype.toLocaleString()` output for the en-US locale.
pub(crate) fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}
