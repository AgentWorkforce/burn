//! Time parsing helpers shared across the SDK.
//!
//! The single ISO-8601 parser and the canonical UTC formatter. Uses Howard
//! Hinnant's days-from-civil formulation; no `chrono`/`time` dependency on
//! the hot ingest path.

/// Parse an ISO-8601 / RFC-3339 timestamp into Unix milliseconds.
///
/// Accepts `YYYY-MM-DD(T|t| )HH:MM:SS[(.|,)f+][Z|z|±HH:MM]`. Fractional
/// seconds are truncated to milliseconds; an absent offset means UTC (the
/// ledger wire format is always UTC). Returns `None` — mirroring
/// `Date.parse` → `NaN` — for any other shape, out-of-range components, or
/// trailing input.
pub(crate) fn parse_iso_ms(s: &str) -> Option<i64> {
    parse_iso(s, DatePrecision::DateTime)
}

/// [`parse_iso_ms`], additionally accepting a bare `YYYY-MM-DD` as midnight
/// UTC.
pub(crate) fn parse_iso_date_or_datetime_ms(s: &str) -> Option<i64> {
    parse_iso(s, DatePrecision::DateOrDateTime)
}

#[derive(Clone, Copy, PartialEq)]
enum DatePrecision {
    DateTime,
    DateOrDateTime,
}

fn parse_iso(s: &str, precision: DatePrecision) -> Option<i64> {
    let mut cur = Cursor::new(s);
    let days = cur.date()?;
    if cur.at_end() && precision == DatePrecision::DateOrDateTime {
        return Some(days * 86_400_000);
    }
    cur.eat(|b| matches!(b, b'T' | b't' | b' '))?;
    let secs_of_day = cur.clock()?;
    let millis = cur.fraction_millis()?;
    let offset_secs = cur.offset_secs()?;
    if !cur.at_end() {
        return None;
    }
    let utc_secs = days * 86_400 + secs_of_day - offset_secs;
    Some(utc_secs * 1_000 + millis)
}

/// Byte cursor over an ISO-8601 string. Every method consumes on success and
/// returns `None` on a shape mismatch.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn at_end(&self) -> bool {
        self.pos == self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, accept: impl Fn(u8) -> bool) -> Option<u8> {
        let b = self.peek().filter(|&b| accept(b))?;
        self.pos += 1;
        Some(b)
    }

    /// Exactly `n` ASCII digits as a number no greater than `max`.
    fn number(&mut self, n: usize, max: u32) -> Option<u32> {
        let digits = self.bytes.get(self.pos..self.pos + n)?;
        let value = digits.iter().try_fold(0u32, |acc, &b| {
            b.is_ascii_digit().then(|| acc * 10 + u32::from(b - b'0'))
        })?;
        self.pos += n;
        (value <= max).then_some(value)
    }

    /// `YYYY-MM-DD` → days from the Unix epoch. Day-of-month is bounded
    /// loosely (1..=31).
    fn date(&mut self) -> Option<i64> {
        let year = self.number(4, 9999)?;
        self.eat(|b| b == b'-')?;
        let month = self.number(2, 12).filter(|&m| m >= 1)?;
        self.eat(|b| b == b'-')?;
        let day = self.number(2, 31).filter(|&d| d >= 1)?;
        Some(ymd_to_days(i64::from(year), month, day))
    }

    /// `HH:MM:SS` → seconds into the day.
    fn clock(&mut self) -> Option<i64> {
        let hour = self.number(2, 23)?;
        self.eat(|b| b == b':')?;
        let minute = self.number(2, 59)?;
        self.eat(|b| b == b':')?;
        let second = self.number(2, 59)?;
        Some(i64::from(hour * 3_600 + minute * 60 + second))
    }

    /// Optional `.fff…` / `,fff…` → milliseconds (truncated, at least one
    /// digit required once the separator is present).
    fn fraction_millis(&mut self) -> Option<i64> {
        if self.eat(|b| b == b'.' || b == b',').is_none() {
            return Some(0);
        }
        let start = self.pos;
        while self.eat(|b| b.is_ascii_digit()).is_some() {}
        let digits = &self.bytes[start..self.pos];
        if digits.is_empty() {
            return None;
        }
        let millis = (0..3).fold(0i64, |acc, i| {
            acc * 10 + digits.get(i).map_or(0, |&b| i64::from(b - b'0'))
        });
        Some(millis)
    }

    /// Optional `Z` / `±HH:MM` → seconds east of UTC (absent = UTC).
    fn offset_secs(&mut self) -> Option<i64> {
        let Some(sign) = self.eat(|b| matches!(b, b'Z' | b'z' | b'+' | b'-')) else {
            return Some(0);
        };
        if sign == b'Z' || sign == b'z' {
            return Some(0);
        }
        let hours = self.number(2, 23)?;
        self.eat(|b| b == b':')?;
        let minutes = self.number(2, 59)?;
        let secs = i64::from(hours * 3_600 + minutes * 60);
        Some(if sign == b'-' { -secs } else { secs })
    }
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
        assert_eq!(parse_iso_ms(""), None);
    }

    #[test]
    fn parse_without_zone_is_utc() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01"), Some(1_000));
        assert_eq!(parse_iso_ms("1970-01-01 00:00:01z"), Some(1_000));
        assert_eq!(parse_iso_ms("1970-01-01t00:00:01Z"), Some(1_000));
    }

    #[test]
    fn parse_applies_offsets() {
        assert_eq!(parse_iso_ms("1970-01-01T01:30:00+01:30"), Some(0));
        assert_eq!(parse_iso_ms("1969-12-31T22:00:00-02:00"), Some(0));
    }

    #[test]
    fn parse_fraction_widths() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00,25Z"), Some(250));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.123456789Z"), Some(123));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.Z"), None);
    }

    #[test]
    fn rejects_out_of_range_components() {
        for bad in [
            "2026-00-20T00:00:00Z",
            "2026-13-20T00:00:00Z",
            "2026-04-00T00:00:00Z",
            "2026-04-32T00:00:00Z",
            "2026-04-20T24:00:00Z",
            "2026-04-20T23:60:00Z",
            "2026-04-20T23:59:60Z",
            "2026-04-20T00:00:00+24:00",
            "2026-04-20T00:00:00+01:60",
        ] {
            assert_eq!(parse_iso_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn rejects_malformed_shapes() {
        for bad in [
            "2026-04-20T00:00:00Zjunk",
            "2026-04-20T00:00:00+0100",
            "2026-04-20T00:00:00+01",
            "2026-04-20X00:00:00Z",
            "2026/04/20T00:00:00Z",
            "2026-04-20T00-00-00Z",
            "2026-4-20T00:00:00Z",
            "2026-04-20T00:00",
            "2026-04-20",
        ] {
            assert_eq!(parse_iso_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn date_only_is_midnight_utc_when_allowed() {
        assert_eq!(
            parse_iso_date_or_datetime_ms("2026-01-01"),
            Some(1_767_225_600_000)
        );
        assert_eq!(
            parse_iso_date_or_datetime_ms("2026-01-01T00:00:00.500Z"),
            Some(1_767_225_600_500)
        );
        assert_eq!(parse_iso_date_or_datetime_ms("2026-01-01T"), None);
    }

    #[test]
    fn ymd_days_round_trip() {
        for (y, m, d) in [(1970, 1, 1), (2026, 5, 6), (2000, 2, 29), (1999, 12, 31)] {
            assert_eq!(days_to_ymd(ymd_to_days(y, m, d)), (y, m, d));
        }
    }

    #[test]
    fn format_round_trips_parse() {
        let ms = 1_767_225_600_500;
        assert_eq!(format_iso_ms(ms), "2026-01-01T00:00:00.500Z");
        assert_eq!(parse_iso_ms(&format_iso_ms(ms)), Some(ms));
        assert_eq!(format_iso_ms(-1), "1969-12-31T23:59:59.999Z");
    }
}
