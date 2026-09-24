//! Calendar maths, without a date crate.
//!
//! The core must stay wasm-clean and dependency-light, and the only calendar questions
//! it ever asks are "what UTC day is this epoch" and "print this epoch". Both are a few
//! lines of Howard Hinnant's civil-from-days algorithm, valid across the whole
//! Gregorian range — including the century rule that 2100 is not a leap year.
//!
//! Nothing here reads a clock. `now` is always passed in.

/// `(year, month, day, hour, minute, second)` in UTC for an epoch in seconds.
pub fn civil(epoch: f64) -> (i64, i64, i64, i64, i64, i64) {
    let secs = epoch as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, h, mi, s)
}

/// `YYYY-MM-DD` in UTC. The key the sell policy uses to evaluate its trail once a day.
pub fn utc_day(epoch: f64) -> String {
    let (y, m, d, _, _, _) = civil(epoch);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DDTHH:MM:SS+00:00`.
pub fn iso8601(epoch: f64) -> String {
    let (y, m, d, h, mi, s) = civil(epoch);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
}

/// What Python's `datetime.fromtimestamp(epoch, timezone.utc).isoformat()` prints: whole
/// seconds as [`iso8601`], a fraction as `.ffffff` before the offset, rounded half to even
/// to the microsecond the way Python rounds it.
pub fn iso8601_micros(epoch: f64) -> String {
    let mut secs = epoch.trunc();
    let mut us = ((epoch - secs) * 1e6).round_ties_even();
    if us >= 1e6 {
        us -= 1e6;
        secs += 1.0;
    } else if us < 0.0 {
        us += 1e6;
        secs -= 1.0;
    }
    let whole = iso8601(secs);
    if us == 0.0 {
        whole
    } else {
        format!("{}.{:06}{}", &whole[..19], us as i64, &whole[19..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions_print_to_the_microsecond() {
        assert_eq!(iso8601_micros(1_700_000_000.0), "2023-11-14T22:13:20+00:00");
        assert_eq!(
            iso8601_micros(1_700_000_000.25),
            "2023-11-14T22:13:20.250000+00:00"
        );
        assert_eq!(
            iso8601_micros(1_700_000_000.9999996),
            "2023-11-14T22:13:21+00:00",
            "rounds up into the next second"
        );
    }

    #[test]
    fn known_timestamps() {
        assert_eq!(iso8601(0.0), "1970-01-01T00:00:00+00:00");
        assert_eq!(iso8601(1_700_000_000.0), "2023-11-14T22:13:20+00:00");
        assert_eq!(iso8601(1_789_919_316.0), "2026-09-20T15:48:36+00:00");
    }

    #[test]
    fn the_places_date_maths_breaks() {
        assert_eq!(
            iso8601(1_709_164_800.0),
            "2024-02-29T00:00:00+00:00",
            "a leap day"
        );
        assert_eq!(
            iso8601(1_767_225_599.0),
            "2025-12-31T23:59:59+00:00",
            "a year boundary"
        );
        assert_eq!(
            iso8601(4_107_542_400.0),
            "2100-03-01T00:00:00+00:00",
            "2100 is not a leap year"
        );
    }

    #[test]
    fn utc_day_rolls_at_midnight_not_at_local_noon() {
        assert_eq!(utc_day(1_700_000_000.0), "2023-11-14");
        assert_eq!(
            utc_day(1_700_000_000.0 + 6_400.0),
            "2023-11-15",
            "6400s later crosses midnight"
        );
        assert_eq!(utc_day(1_767_225_599.0), "2025-12-31");
        assert_eq!(utc_day(1_767_225_600.0), "2026-01-01");
    }
}
