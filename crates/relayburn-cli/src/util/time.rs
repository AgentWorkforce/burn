//! Tiny time helpers shared by the harness adapter ([`super::super::harnesses::claude`])
//! and the `burn run` driver ([`super::super::commands::run`]).
//!
//! Both call sites need the same two operations:
//!
//! - [`iso_now`] — current wall-clock in `YYYY-MM-DDTHH:MM:SS.mmmZ` format,
//!   matching `new Date().toISOString()` in the TS sibling.
//! - [`civil_from_days`] — Howard Hinnant's days-since-epoch → (Y, M, D)
//!   conversion. Pulled out so we don't pay a `chrono` / `time` dependency
//!   for two tiny call sites.
//!
//! Until D5 / D6 these were duplicated across `harnesses/claude.rs` and
//! `commands/run.rs` with a `keep them in sync` comment. CodeRabbit caught
//! the duplication during PR #318 review; this module is the resolution.

/// Build an ISO-8601 UTC timestamp suitable for `Stamp::ts` / the
/// `burnSpawnTs` enrichment tag. Mirrors `new Date().toISOString()` in
/// the TS sibling.
pub fn iso_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let total_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    iso_from_ms(total_ms)
}

/// Format an absolute `total_ms` (millis since the Unix epoch) as an
/// ISO-8601 UTC string. Split out so callers that already captured a
/// `SystemTime` (e.g. the driver's `spawn_start_ts`) can format it without
/// re-reading the clock.
pub fn iso_from_system_time(t: std::time::SystemTime) -> String {
    use std::time::UNIX_EPOCH;
    let total_ms = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    iso_from_ms(total_ms)
}

fn iso_from_ms(total_ms: i64) -> String {
    let total_secs = total_ms.div_euclid(1000);
    let ms = total_ms.rem_euclid(1000) as u32;
    // Civil-date conversion (Howard Hinnant's algorithm). Sufficient for
    // the y2038-and-beyond range we care about.
    let z = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(z);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{ms:03}Z")
}

/// Days-since-epoch → (year, month, day). Hinnant 2014.
///
/// `z` is days since 1970-01-01; negative values are pre-epoch. Returns
/// the proleptic Gregorian (year, 1-12 month, 1-31 day).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + (if m <= 2 { 1 } else { 0 }), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_now_is_zulu_iso8601_shape() {
        let s = iso_now();
        assert_eq!(s.len(), "1970-01-01T00:00:00.000Z".len());
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
        assert_eq!(&s[19..20], ".");
    }

    #[test]
    fn civil_from_days_round_trips_known_dates() {
        // 1970-01-01 = day 0
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-01-01 = day 10957
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        // 2024-02-29 (leap) = day 19782
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn iso_from_system_time_uses_unix_epoch() {
        use std::time::{Duration, UNIX_EPOCH};
        let t = UNIX_EPOCH + Duration::from_millis(0);
        assert_eq!(iso_from_system_time(t), "1970-01-01T00:00:00.000Z");
    }
}
