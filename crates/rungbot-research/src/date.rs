//! Calendar dates for the research pipeline.
//!
//! The report and the search windows are dated by the **local** calendar day (a weekly
//! report run early on a Sunday is Sunday's report wherever it runs); unlock events are
//! dated in UTC. Only the local "today" needs a clock, and [`Date::today_local`] is the
//! one place that reads it.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

impl Date {
    pub fn new(year: i64, month: u32, day: u32) -> Self {
        Date { year, month, day }
    }

    /// Today on the local clock.
    pub fn today_local() -> Self {
        use chrono::Datelike;
        let d = chrono::Local::now().date_naive();
        Date::new(i64::from(d.year()), d.month(), d.day())
    }

    /// `YYYY-MM-DD`, strictly.
    pub fn parse(s: &str) -> Option<Date> {
        let mut it = s.split('-');
        let (y, m, d) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
            return None;
        }
        let date = Date::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
        (date.month >= 1
            && date.month <= 12
            && date.day >= 1
            && Date::from_days(date.days()) == date)
            .then_some(date)
    }

    /// Days since 1970-01-01.
    pub fn days(&self) -> i64 {
        let (m, d) = (i64::from(self.month), i64::from(self.day));
        let y = if m <= 2 { self.year - 1 } else { self.year };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    pub fn from_days(days: i64) -> Date {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let y = yoe + era * 400 + i64::from(m <= 2);
        Date::new(y, m, d)
    }

    pub fn minus_days(&self, n: i64) -> Date {
        Date::from_days(self.days() - n)
    }

    /// The UTC day of an epoch in seconds, rounding a fractional epoch down.
    pub fn from_epoch_utc(epoch: f64) -> Date {
        Date::from_days((epoch.floor() as i64).div_euclid(86_400))
    }

    /// The start of the day as the search API's `startPublishedDate`.
    pub fn search_start(&self) -> String {
        format!("{self}T00:00:00.000Z")
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// `[YYYY-MM-DDTHH:MM:SSZ]`-style UTC timestamp for log lines.
pub fn utc_stamp(epoch: i64) -> String {
    let d = Date::from_days(epoch.div_euclid(86_400));
    let rem = epoch.rem_euclid(86_400);
    format!(
        "{d}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Seconds since the epoch on the system clock.
pub fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_round_trip_across_leap_and_century_years() {
        for days in [-800_000, -1, 0, 1, 11_016, 20_000, 47_482, 200_000] {
            assert_eq!(Date::from_days(days).days(), days);
        }
        assert_eq!(Date::from_days(0), Date::new(1970, 1, 1));
        assert_eq!(Date::new(2100, 3, 1).minus_days(1), Date::new(2100, 2, 28));
        assert_eq!(Date::new(2024, 3, 1).minus_days(1), Date::new(2024, 2, 29));
    }

    #[test]
    fn search_windows_step_back_across_a_year_end() {
        let d = Date::new(2026, 1, 20);
        assert_eq!(d.minus_days(60).search_start(), "2025-11-21T00:00:00.000Z");
    }

    #[test]
    fn parse_is_strict() {
        assert_eq!(Date::parse("2026-09-27"), Some(Date::new(2026, 9, 27)));
        assert_eq!(Date::parse("2026-02-30"), None);
        assert_eq!(Date::parse("2026-9-27"), None);
    }

    #[test]
    fn utc_epochs_floor_to_their_day() {
        assert_eq!(Date::from_epoch_utc(-0.5).to_string(), "1969-12-31");
        assert_eq!(Date::from_epoch_utc(86_399.9).to_string(), "1970-01-01");
        assert_eq!(utc_stamp(1_790_000_000), "2026-09-21T14:13:20Z");
    }
}
