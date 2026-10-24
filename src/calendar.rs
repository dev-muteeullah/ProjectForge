//! Working-calendar arithmetic.
//!
//! DESIGN: the scheduler never touches wall-clock time. It works on a *working-time axis*:
//! an integer count of working minutes since the timeline origin, where weekends/holidays do
//! not exist. A `Timeline` maps working-day indices <-> real dates. Because the axis is linear,
//! every CPM formula (ES + duration = EF, lags, floats) is plain integer arithmetic, and a
//! 1-day task starting Friday ends at "end of Friday" (= start of next working day), never Saturday.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Safety valve so a pathological calendar can never hang the UI in a scan loop.
const MAX_SCAN: usize = 366 * 50;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Calendar {
    pub name: String,
    /// Index 0 = Monday ... 6 = Sunday.
    pub work_week: [bool; 7],
    pub hours_per_day: u32,
    pub holidays: BTreeSet<NaiveDate>,
}

impl Default for Calendar {
    fn default() -> Self {
        Calendar {
            name: "Standard (Mon-Fri, 8h)".into(),
            work_week: [true, true, true, true, true, false, false],
            hours_per_day: 8,
            holidays: BTreeSet::new(),
        }
    }
}

pub fn next_day(d: NaiveDate) -> NaiveDate {
    d.succ_opt().unwrap_or(d)
}
pub fn prev_day(d: NaiveDate) -> NaiveDate {
    d.pred_opt().unwrap_or(d)
}
/// Fractional "calendar day" coordinate used by the Gantt (days since CE).
pub fn cal_of(d: NaiveDate) -> f64 {
    d.num_days_from_ce() as f64
}
pub fn date_of_cal(c: f64) -> NaiveDate {
    NaiveDate::from_num_days_from_ce_opt(c.floor() as i32).unwrap_or(NaiveDate::MIN)
}

impl Calendar {
    pub fn minutes_per_day(&self) -> i64 {
        self.hours_per_day as i64 * 60
    }

    pub fn is_valid(&self) -> bool {
        self.hours_per_day > 0 && self.hours_per_day <= 24 && self.work_week.iter().any(|w| *w)
    }

    pub fn is_working(&self, d: NaiveDate) -> bool {
        self.work_week[d.weekday().num_days_from_monday() as usize] && !self.holidays.contains(&d)
    }

    pub fn next_working_on_or_after(&self, d: NaiveDate) -> NaiveDate {
        let mut d = d;
        for _ in 0..MAX_SCAN {
            if self.is_working(d) {
                return d;
            }
            let n = next_day(d);
            if n == d {
                break;
            }
            d = n;
        }
        d
    }

    pub fn prev_working_on_or_before(&self, d: NaiveDate) -> NaiveDate {
        let mut d = d;
        for _ in 0..MAX_SCAN {
            if self.is_working(d) {
                return d;
            }
            let n = prev_day(d);
            if n == d {
                break;
            }
            d = n;
        }
        d
    }

    /// Step `n` working days from `d` (d is first snapped forward to a working day).
    pub fn add_working_days(&self, d: NaiveDate, n: i64) -> NaiveDate {
        let mut d = self.next_working_on_or_after(d);
        if n >= 0 {
            for _ in 0..n {
                d = self.next_working_on_or_after(next_day(d));
            }
        } else {
            for _ in 0..(-n) {
                d = self.prev_working_on_or_before(prev_day(d));
            }
        }
        d
    }

    /// Signed number of working-day steps needed to get from `a` to `b`.
    pub fn working_days_between(&self, a: NaiveDate, b: NaiveDate) -> i64 {
        if a == b {
            return 0;
        }
        let (lo, hi, sign) = if a < b { (a, b, 1) } else { (b, a, -1) };
        let mut count = 0i64;
        let mut d = lo;
        let mut guard = 0usize;
        while d < hi && guard < MAX_SCAN * 4 {
            d = next_day(d);
            if self.is_working(d) {
                count += 1;
            }
            guard += 1;
        }
        sign * count
    }
}

/// Lazily-built list of working dates starting at the (snapped) origin.
/// `dates[i]` is the date of working-day index `i`.
pub struct Timeline<'a> {
    cal: &'a Calendar,
    dates: Vec<NaiveDate>,
}

impl<'a> Timeline<'a> {
    pub fn new(cal: &'a Calendar, origin: NaiveDate) -> Self {
        Timeline { cal, dates: vec![cal.next_working_on_or_after(origin)] }
    }

    fn extend_one(&mut self) -> bool {
        let last = *self.dates.last().expect("timeline never empty");
        let n = self.cal.next_working_on_or_after(next_day(last));
        if n <= last {
            return false; // hit end of the representable calendar
        }
        self.dates.push(n);
        true
    }

    pub fn date_of(&mut self, idx: i64) -> NaiveDate {
        let i = idx.max(0) as usize;
        while self.dates.len() <= i {
            if !self.extend_one() {
                break;
            }
        }
        self.dates[i.min(self.dates.len() - 1)]
    }

    /// Working-day index of `date`, snapping non-working dates forward. Clamps at 0.
    pub fn day_index(&mut self, date: NaiveDate) -> i64 {
        if date <= self.dates[0] {
            return 0;
        }
        while *self.dates.last().expect("non-empty") < date {
            if !self.extend_one() {
                break;
            }
        }
        self.dates.partition_point(|d| *d < date) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn weekend_is_not_working() {
        let c = Calendar::default();
        assert!(c.is_working(d(2026, 10, 23))); // Friday
        assert!(!c.is_working(d(2026, 10, 24))); // Saturday
        assert!(!c.is_working(d(2026, 10, 25))); // Sunday
        assert!(c.is_working(d(2026, 10, 26))); // Monday
    }

    #[test]
    fn add_working_days_skips_weekends() {
        let c = Calendar::default();
        // Friday + 1 working day = Monday
        assert_eq!(c.add_working_days(d(2026, 10, 23), 1), d(2026, 10, 26));
        // Monday + 4 = Friday
        assert_eq!(c.add_working_days(d(2026, 10, 26), 4), d(2026, 10, 30));
        // Monday - 1 = previous Friday
        assert_eq!(c.add_working_days(d(2026, 10, 26), -1), d(2026, 10, 23));
        // Saturday snaps to Monday first
        assert_eq!(c.add_working_days(d(2026, 10, 24), 0), d(2026, 10, 26));
    }

    #[test]
    fn holidays_are_skipped() {
        let mut c = Calendar::default();
        c.holidays.insert(d(2026, 10, 27)); // Tuesday
        assert_eq!(c.add_working_days(d(2026, 10, 26), 1), d(2026, 10, 28));
        assert_eq!(c.working_days_between(d(2026, 10, 26), d(2026, 10, 28)), 1);
    }

    #[test]
    fn working_days_between_is_signed() {
        let c = Calendar::default();
        assert_eq!(c.working_days_between(d(2026, 10, 23), d(2026, 10, 26)), 1);
        assert_eq!(c.working_days_between(d(2026, 10, 26), d(2026, 10, 23)), -1);
        assert_eq!(c.working_days_between(d(2026, 10, 26), d(2026, 11, 2)), 5);
    }

    #[test]
    fn timeline_maps_index_and_date() {
        let c = Calendar::default();
        let mut t = Timeline::new(&c, d(2026, 10, 24)); // Saturday -> Monday 26th
        assert_eq!(t.date_of(0), d(2026, 10, 26));
        assert_eq!(t.date_of(4), d(2026, 10, 30));
        assert_eq!(t.date_of(5), d(2026, 11, 2));
        assert_eq!(t.day_index(d(2026, 11, 2)), 5);
        // weekend date snaps forward to next working day
        assert_eq!(t.day_index(d(2026, 10, 31)), 5);
        // before origin clamps
        assert_eq!(t.day_index(d(2020, 1, 1)), 0);
    }

    #[test]
    fn calendar_validity() {
        let mut c = Calendar::default();
        assert!(c.is_valid());
        c.work_week = [false; 7];
        assert!(!c.is_valid());
    }
}
