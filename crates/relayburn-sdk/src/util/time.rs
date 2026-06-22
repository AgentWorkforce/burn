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
    let days_from_epoch = ymd_to_days(year, month, day);
    let secs =
        days_from_epoch * 86_400 + (hour as i64) * 3_600 + (minute as i64) * 60 + (second as i64);
    Some(secs * 1_000 + millis)
}

/// Civil date → days from the Unix epoch (Howard Hinnant's proleptic-Gregorian
/// `days_from_civil`). Inverse of [`days_to_ymd`]. Does **not** range-check its
/// inputs — callers that accept untrusted month/day values guard the range
/// themselves before calling.
pub(crate) fn ymd_to_days(year: i64, month: u32, day: u32) -> i64 {
    let m = month as i64;
    let d = day as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + (d as u64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

/// Format Unix milliseconds as a canonical UTC ISO-8601 string
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`), matching JS `new Date(ms).toISOString()`.
/// The single source of truth for the SDK's wire timestamp format.
pub(crate) fn format_iso_ms(ms: i64) -> String {
    const MS_PER_DAY: i64 = 86_400_000;
    let total_days = ms.div_euclid(MS_PER_DAY);
    let ms_in_day = ms.rem_euclid(MS_PER_DAY);
    let (year, month, day) = days_to_ymd(total_days);
    let hour = ms_in_day / 3_600_000;
    let minute = (ms_in_day / 60_000) % 60;
    let second = (ms_in_day / 1_000) % 60;
    let millis = ms_in_day % 1_000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days from the Unix epoch → `(year, month, day)` (Howard Hinnant's
/// `civil_from_days`). Inverse of [`ymd_to_days`].
pub(crate) fn days_to_ymd(days_from_epoch: i64) -> (i64, u32, u32) {
    let z = days_from_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
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
        assert_eq!(
            parse_iso_ms("2026-01-01T00:00:00.500Z"),
            Some(1_767_225_600_500)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_iso_ms("not a date"), None);
        assert_eq!(parse_iso_ms("short"), None);
    }
}
