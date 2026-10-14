//! The wall clock behind a provider, injectable so a replay runs under the instant it was recorded at.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, the unit of JS `Date.now()`.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
    })
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if month > 2 { month - 3 } else { month + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` (UTC), the `toISOString().slice(0, 10)` of JS.
pub fn utc_date(ms: i64) -> String {
    let (y, m, d) = civil_from_days(ms.div_euclid(86_400_000));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, the `toISOString()` of JS.
pub fn iso8601(ms: i64) -> String {
    let in_day = ms.rem_euclid(86_400_000);
    format!(
        "{}T{:02}:{:02}:{:02}.{:03}Z",
        utc_date(ms),
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    )
}

/// The inverse of [`iso8601`]; `None` for anything but that exact shape.
pub fn parse_iso8601(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() != 24
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'.'
        || b[23] != b'Z'
    {
        return None;
    }
    let n = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let days = days_from_civil(n(0..4)?, n(5..7)? as u32, n(8..10)? as u32);
    Some(
        days * 86_400_000
            + n(11..13)? * 3_600_000
            + n(14..16)? * 60_000
            + n(17..19)? * 1000
            + n(20..23)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_to_iso_string() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_773_100_798_123), "2026-03-09T23:59:58.123Z");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
    }

    #[test]
    fn round_trips_across_leap_days_and_before_the_epoch() {
        for ms in [
            -86_400_001,
            0,
            951_782_400_000,
            1_773_100_798_123,
            4_102_444_799_999,
        ] {
            assert_eq!(parse_iso8601(&iso8601(ms)), Some(ms), "{ms}");
        }
    }

    #[test]
    fn control_a_malformed_instant_is_rejected() {
        assert_eq!(parse_iso8601("2026-03-09"), None);
    }
}
