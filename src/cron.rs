//! Cron schedules of five fields (minute, hour, day of month, month, day of week), as
//! Argo CD's sync windows and Kubernetes CronJobs write them, and the next time one fires.
//!
//! Each field takes `*` (or `?`), a value, a range `a-b`, a step `*/n`, `a/n` or `a-b/n`, and
//! lists of those; months and days take their English names (`JAN`, `MON`), and 7 is Sunday
//! too. The descriptors `@yearly`, `@monthly`, `@weekly`, `@daily` and `@hourly` are read. As
//! in Vixie cron (and the Go library Argo CD uses), a day matches when the day of month *or*
//! the day of week does, unless one of them is `*`: then both must.

use jiff::ToSpan as _;
use jiff::ZonedDateTime;
use jiff::civil::Date;

/// A schedule, read once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    /// Bits 1 to 31.
    days: u32,
    /// Bits 1 to 12.
    months: u16,
    /// Bits 0 (Sunday) to 6.
    weekdays: u8,
    /// The day-of-month or day-of-week field is `*` (or `?`).
    days_star: bool,
    weekdays_star: bool,
}

const MONTHS: [&str; 12] = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];
const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
/// How far ahead a next time is looked for.
const YEARS_AHEAD: i16 = 5;

/// A field's values as bits, from `min` to `max`; `names` are read as `min`, `min + 1`...
fn field(text: &str, min: u32, max: u32, names: &[&str]) -> Result<(u64, bool), String> {
    let value = |s: &str| -> Result<u32, String> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(min + i as u32);
        }
        s.parse::<u32>().map_err(|_| format!("`{s}` is not a value"))
    };
    let mut bits = 0u64;
    let star = text.starts_with('*') || text.starts_with('?');
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step = s.parse::<u32>().map_err(|_| format!("`{s}` is not a step"))?;
                if step == 0 {
                    return Err("a step of 0".into());
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = match range {
            "*" | "?" => (min, max),
            r => match r.split_once('-') {
                Some((a, b)) => (value(a)?, value(b)?),
                // `a/n` runs from a to the end of the field.
                None if step > 1 || part.contains('/') => (value(r)?, max),
                None => {
                    let v = value(r)?;
                    (v, v)
                }
            },
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("`{part}` is out of {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, star))
}

impl Cron {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let expanded = match text.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            _ if text.starts_with('@') => return Err(format!("`{text}` is not a schedule this reads")),
            _ => text,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(format!("`{text}` does not have five fields"));
        };
        let bad = |what: &str, e: String| format!("{what} of `{text}`: {e}");
        let (minutes, _) = field(minute, 0, 59, &[]).map_err(|e| bad("the minute", e))?;
        let (hours, _) = field(hour, 0, 23, &[]).map_err(|e| bad("the hour", e))?;
        let (days, days_star) = field(day, 1, 31, &[]).map_err(|e| bad("the day of month", e))?;
        let (months, _) = field(month, 1, 12, &MONTHS).map_err(|e| bad("the month", e))?;
        let (weekdays, weekdays_star) = field(weekday, 0, 7, &WEEKDAYS).map_err(|e| bad("the day of week", e))?;
        // 7 is Sunday too.
        let weekdays = (weekdays & 0x7f) | u64::from(weekdays & (1 << 7) != 0);
        Ok(Self {
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: weekdays as u8,
            days_star,
            weekdays_star,
        })
    }

    fn day_matches(&self, d: Date) -> bool {
        let dom = self.days & (1 << d.day()) != 0;
        let dow = self.weekdays & (1 << d.weekday().to_sunday_zero_offset()) != 0;
        if self.days_star || self.weekdays_star { dom && dow } else { dom || dow }
    }

    /// The first time it fires strictly after `after`, in `after`'s time zone. A time a
    /// clock change skips fires when the clock reaches it again (as jiff resolves the gap);
    /// None when it never fires within five years (`0 0 30 2 *`). It jumps to the next month,
    /// hour and minute the schedule names rather than stepping through the minutes.
    pub fn next_after(&self, after: &ZonedDateTime) -> Option<ZonedDateTime> {
        let tz = after.time_zone().clone();
        let start = after.datetime();
        let mut t = start.date().at(start.hour(), start.minute(), 0, 0).checked_add(1.minute()).ok()?;
        let last_year = start.year().saturating_add(YEARS_AHEAD);
        loop {
            if t.year() > last_year {
                return None;
            }
            let d = t.date();
            if u64::from(self.months) & (1 << t.month()) == 0 {
                t = match first_from(u64::from(self.months), t.month() + 1, 12) {
                    Some(m) => Date::new(d.year(), m, 1).ok()?.at(0, 0, 0, 0),
                    None => Date::new(d.year().checked_add(1)?, 1, 1).ok()?.at(0, 0, 0, 0),
                };
                continue;
            }
            if !self.day_matches(d) {
                t = d.tomorrow().ok()?.at(0, 0, 0, 0);
                continue;
            }
            let Some(h) = first_from(u64::from(self.hours), t.hour(), 23) else {
                t = d.tomorrow().ok()?.at(0, 0, 0, 0);
                continue;
            };
            if h != t.hour() {
                t = d.at(h, 0, 0, 0);
                continue;
            }
            let Some(m) = first_from(self.minutes, t.minute(), 59) else {
                t = d.at(h, 0, 0, 0).checked_add(1.hour()).ok()?;
                continue;
            };
            let at = tz.to_zoned_datetime(d.at(h, m, 0, 0)).ok()?;
            if at.timestamp() > after.timestamp() {
                return Some(at);
            }
            t = d.at(h, m, 0, 0).checked_add(1.minute()).ok()?;
        }
    }

    /// The last time it fired strictly before `before`, in `before`'s time zone: as
    /// `next_after`, going back (up to five years).
    pub fn prev_before(&self, before: &ZonedDateTime) -> Option<ZonedDateTime> {
        let tz = before.time_zone().clone();
        let start = before.datetime();
        let mut t = start.date().at(start.hour(), start.minute(), 0, 0);
        let first_year = start.year().saturating_sub(YEARS_AHEAD);
        loop {
            if t.year() < first_year {
                return None;
            }
            let d = t.date();
            let day_end = |d: Date| d.at(23, 59, 0, 0);
            if u64::from(self.months) & (1 << t.month()) == 0 {
                t = match last_from(u64::from(self.months), t.month() - 1, 1) {
                    Some(m) => day_end(Date::new(d.year(), m, 1).ok()?.last_of_month()),
                    None => day_end(Date::new(d.year().checked_sub(1)?, 12, 31).ok()?),
                };
                continue;
            }
            if !self.day_matches(d) {
                t = day_end(d.yesterday().ok()?);
                continue;
            }
            let Some(h) = last_from(u64::from(self.hours), t.hour(), 0) else {
                t = day_end(d.yesterday().ok()?);
                continue;
            };
            if h != t.hour() {
                t = d.at(h, 59, 0, 0);
                continue;
            }
            let Some(m) = last_from(self.minutes, t.minute(), 0) else {
                t = if h == 0 { day_end(d.yesterday().ok()?) } else { d.at(h - 1, 59, 0, 0) };
                continue;
            };
            let at = tz.to_zoned_datetime(d.at(h, m, 0, 0)).ok()?;
            if at.timestamp() < before.timestamp() {
                return Some(at);
            }
            t = d.at(h, m, 0, 0).checked_sub(1.minute()).ok()?;
        }
    }
}

/// The lowest value of `bits` from `from` up to `to`.
fn first_from(bits: u64, from: i8, to: i8) -> Option<i8> {
    (from.max(0)..=to).find(|v| bits & (1 << v) != 0)
}

/// The highest value of `bits` from `from` down to `to`.
fn last_from(bits: u64, from: i8, to: i8) -> Option<i8> {
    (to..=from).rev().find(|v| bits & (1 << v) != 0)
}

#[cfg(test)]
mod tests {
    use super::Cron;
    use jiff::civil::date;
    use jiff::tz::TimeZone;

    fn next(cron: &str, from: jiff::civil::DateTime) -> Option<jiff::civil::DateTime> {
        let at = TimeZone::UTC.to_zoned_datetime(from).unwrap();
        Cron::parse(cron).unwrap().next_after(&at).map(|z| z.datetime())
    }

    fn prev(cron: &str, from: jiff::civil::DateTime) -> Option<jiff::civil::DateTime> {
        let at = TimeZone::UTC.to_zoned_datetime(from).unwrap();
        Cron::parse(cron).unwrap().prev_before(&at).map(|z| z.datetime())
    }

    #[test]
    fn the_last_time_it_fired_reads_back() {
        let from = date(2026, 11, 24).at(10, 30, 0, 0); // a Tuesday
        // Strictly before: 10:30 itself doesn't count.
        assert_eq!(prev("* * * * *", from), Some(date(2026, 11, 24).at(10, 29, 0, 0)));
        assert_eq!(prev("0 22 * * *", from), Some(date(2026, 11, 23).at(22, 0, 0, 0)));
        assert_eq!(prev("*/15 * * * *", from), Some(date(2026, 11, 24).at(10, 15, 0, 0)));
        assert_eq!(prev("0 9 * * MON-FRI", date(2026, 11, 30).at(8, 0, 0, 0)), Some(date(2026, 11, 27).at(9, 0, 0, 0)));
        assert_eq!(prev("0 0 1 jan *", from), Some(date(2026, 1, 1).at(0, 0, 0, 0)));
        assert_eq!(prev("30 4 31 * *", from), Some(date(2026, 10, 31).at(4, 30, 0, 0)));
        assert_eq!(prev("0 0 30 2 *", from), None);
        // Going forward then back lands where it started, for any schedule.
        for cron in ["* * * * *", "5 */3 * * *", "0 9 * * 1-5", "0 0 1,15 * *", "@monthly"] {
            let at = TimeZone::UTC.to_zoned_datetime(from).unwrap();
            let c = Cron::parse(cron).unwrap();
            let next = c.next_after(&at).unwrap();
            let back = c.prev_before(&next).unwrap();
            assert!(back.timestamp() <= at.timestamp(), "{cron}");
            assert_eq!(c.next_after(&back).unwrap().timestamp(), next.timestamp(), "{cron}");
        }
    }

    #[test]
    fn a_clock_change_moves_the_time_it_fires() {
        // Europe/Paris springs forward at 02:00 on 2026-03-29: 02:00 is 03:00 that day.
        let Ok(paris) = TimeZone::get("Europe/Paris") else {
            eprintln!("no time zone database: skipped");
            return;
        };
        let c = Cron::parse("0 2 * * *").unwrap();
        let at = paris.to_zoned_datetime(date(2026, 3, 28).at(12, 0, 0, 0)).unwrap();
        let next = c.next_after(&at).unwrap();
        assert_eq!(next.timestamp(), "2026-03-29T01:00:00Z".parse::<jiff::Timestamp>().unwrap());
        let after = c.next_after(&next).unwrap();
        assert_eq!(after.timestamp(), "2026-03-30T00:00:00Z".parse::<jiff::Timestamp>().unwrap());
        // 09:00 in Paris is 08:00 UTC before the change and 07:00 after.
        let nine = Cron::parse("0 9 * * *").unwrap();
        let before = paris.to_zoned_datetime(date(2026, 3, 28).at(0, 0, 0, 0)).unwrap();
        assert_eq!(nine.next_after(&before).unwrap().timestamp().to_string(), "2026-03-28T08:00:00Z");
        let after = paris.to_zoned_datetime(date(2026, 3, 29).at(4, 0, 0, 0)).unwrap();
        assert_eq!(nine.next_after(&after).unwrap().timestamp().to_string(), "2026-03-29T07:00:00Z");
    }

    #[test]
    fn schedules_read_like_cron() {
        let from = date(2026, 11, 24).at(10, 30, 15, 0); // a Tuesday
        assert_eq!(next("* * * * *", from), Some(date(2026, 11, 24).at(10, 31, 0, 0)));
        assert_eq!(next("0 22 * * *", from), Some(date(2026, 11, 24).at(22, 0, 0, 0)));
        assert_eq!(next("0 9 * * *", from), Some(date(2026, 11, 25).at(9, 0, 0, 0)));
        assert_eq!(next("*/15 * * * *", from), Some(date(2026, 11, 24).at(10, 45, 0, 0)));
        assert_eq!(
            next("0 9 * * MON-FRI", date(2026, 11, 27).at(12, 0, 0, 0)),
            Some(date(2026, 11, 30).at(9, 0, 0, 0))
        );
        // 7 and SUN are Sunday.
        assert_eq!(next("0 0 * * 7", from), Some(date(2026, 11, 29).at(0, 0, 0, 0)));
        assert_eq!(next("0 0 * * sun", from), Some(date(2026, 11, 29).at(0, 0, 0, 0)));
        assert_eq!(next("0 0 1 jan *", from), Some(date(2027, 1, 1).at(0, 0, 0, 0)));
        assert_eq!(next("@monthly", from), Some(date(2026, 12, 1).at(0, 0, 0, 0)));
        assert_eq!(next("30 4 1-3/2 * *", from), Some(date(2026, 12, 1).at(4, 30, 0, 0)));
        // Day of month or day of week, when both are set: the 1st or a Friday.
        assert_eq!(next("0 0 1 * FRI", from), Some(date(2026, 11, 27).at(0, 0, 0, 0)));
        // Never: February has no 30th.
        assert_eq!(next("0 0 30 2 *", from), None);
        // Strictly after: the minute it is now doesn't count.
        assert_eq!(next("30 10 * * *", from), Some(date(2026, 11, 25).at(10, 30, 0, 0)));
    }

    #[test]
    fn a_schedule_that_does_not_read_says_why() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "*/0 * * * *",
            "a * * * *",
            "@reboot",
            "5-1 * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
        assert!(Cron::parse("0,30 8-18 ? * 1-5").is_ok());
    }
}
