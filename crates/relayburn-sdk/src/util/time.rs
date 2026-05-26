//! Time parsing helpers shared across the SDK.
//!
//! Centralizes a single ISO-8601 → Unix milliseconds parser so the four
//! historical copies (analyze::context_delta, query_verbs, reader::claude,
//! reader::codex) stay in sync. Uses Howard Hinnant's days-from-civil
//! formulation; no external `chrono`/`time` dependency.

/// Parse an ISO-8601 / RFC-3339 timestamp (with optional fractional
/// seconds, truncated to milliseconds) into Unix milliseconds.
///
/// Returns `None` for strings that don't match the expected
/// `YYYY-MM-DDTHH:MM:SS[.fff][Z]` shape. The trailing `Z` is optional —
/// callers passing offset-less ISO strings get the same answer as if `Z`
/// were present (the SDK's wire ledger format is always UTC).
pub(crate) fn parse_iso_ms(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if !(bytes[4] == b'-'
        && bytes[7] == b'-'
        && (bytes[10] == b'T' || bytes[10] == b' ')
        && bytes[13] == b':'
        && bytes[16] == b':')
    {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let minute: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let second: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;
    let mut millis: i64 = 0;
    let mut idx = 19;
    if idx < bytes.len() && bytes[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        let mut frac = std::str::from_utf8(&bytes[frac_start..idx])
            .ok()?
            .to_string();
        if frac.len() > 3 {
            frac.truncate(3);
        }
        while frac.len() < 3 {
            frac.push('0');
        }
        millis = frac.parse().ok()?;
    }
    let m = month as i64;
    let d = day as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + (d as u64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_from_epoch = era * 146_097 + (doe as i64) - 719_468;
    let secs =
        days_from_epoch * 86_400 + (hour as i64) * 3_600 + (minute as i64) * 60 + (second as i64);
    Some(secs * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_epoch() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
    }

    #[test]
    fn parse_with_fractional() {
        // 2026-01-01T00:00:00.500Z == 1767225600500
        assert_eq!(parse_iso_ms("2026-01-01T00:00:00.500Z"), Some(1_767_225_600_500));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_iso_ms("not a date"), None);
        assert_eq!(parse_iso_ms("short"), None);
    }
}
