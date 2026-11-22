//! Recurring events: an RFC 5545 `RRULE` subset, expanded over a range of
//! dates. Enough for things that meet on a schedule: every Sunday, every
//! other Tuesday, the first Sunday of each month, every weekday until
//! June, ten lessons.
//!
//! Supported: `FREQ` (`DAILY`, `WEEKLY`, `MONTHLY`, `YEARLY`), `INTERVAL`,
//! `BYDAY` (`SU`, `MO,WE`; with an ordinal under `MONTHLY`: `1SU`, `-1FR`),
//! `BYMONTHDAY` (`15`, `-1`), `UNTIL` (a date, `20270601` or `2027-06-01`)
//! and `COUNT`. Weeks start on Monday. Times are not part of the rule: an
//! event's time is its own (`7:30 PM`), the rule says which days.

use std::fmt;

/// How often a rule repeats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Freq {
    Daily,
    #[default]
    Weekly,
    Monthly,
    Yearly,
}

/// Days of the week, Monday first.
pub const WEEKDAYS: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];

/// A recurrence rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rule {
    pub freq: Freq,
    /// Every `interval` days, weeks, months or years (at least 1).
    pub interval: u32,
    /// Weekdays (0 = Monday … 6 = Sunday), each with an ordinal under
    /// `MONTHLY` (1 = the first in the month, -1 = the last; 0 = every).
    pub by_day: Vec<(i8, u8)>,
    /// Days of the month (`-1`: the last), under `MONTHLY`.
    pub by_month_day: Vec<i8>,
    /// The last date it may fall on (`YYYY-MM-DD`), inclusive.
    pub until: Option<String>,
    /// How many times it happens in all, counted from its first date.
    pub count: Option<u32>,
}

impl Rule {
    /// Every week on these weekdays (0 = Monday … 6 = Sunday).
    pub fn weekly(days: &[u8]) -> Rule {
        Rule {
            freq: Freq::Weekly,
            interval: 1,
            by_day: days.iter().map(|d| (0, *d)).collect(),
            ..Rule::default()
        }
    }

    /// Read an `RRULE` (with or without the `RRULE:` prefix).
    pub fn parse(text: &str) -> Result<Rule, String> {
        let text = text.trim();
        let text = text.strip_prefix("RRULE:").unwrap_or(text);
        let mut r = Rule {
            interval: 1,
            ..Rule::default()
        };
        let mut freq = None;
        for part in text.split(';').filter(|p| !p.trim().is_empty()) {
            let (k, v) = part
                .split_once('=')
                .ok_or_else(|| format!("not KEY=VALUE: {part}"))?;
            let v = v.trim();
            match k.trim().to_ascii_uppercase().as_str() {
                "FREQ" => {
                    freq = Some(match v.to_ascii_uppercase().as_str() {
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        "YEARLY" => Freq::Yearly,
                        _ => return Err(format!("FREQ {v} is not supported")),
                    })
                }
                "INTERVAL" => {
                    r.interval = v
                        .parse()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| format!("INTERVAL {v}"))?;
                }
                "BYDAY" => {
                    for d in v.split(',') {
                        let d = d.trim().to_ascii_uppercase();
                        let at = d.len().checked_sub(2).ok_or_else(|| format!("BYDAY {d}"))?;
                        let (ord, day) = d.split_at(at);
                        let wd = WEEKDAYS
                            .iter()
                            .position(|w| *w == day)
                            .ok_or_else(|| format!("BYDAY {d}"))?;
                        let ord: i8 = if ord.is_empty() {
                            0
                        } else {
                            ord.trim_start_matches('+')
                                .parse()
                                .ok()
                                .filter(|n: &i8| (-5..=5).contains(n) && *n != 0)
                                .ok_or_else(|| format!("BYDAY {d}"))?
                        };
                        r.by_day.push((ord, wd as u8));
                    }
                }
                "BYMONTHDAY" => {
                    for d in v.split(',') {
                        let n: i8 = d
                            .trim()
                            .parse()
                            .ok()
                            .filter(|n: &i8| (-31..=31).contains(n) && *n != 0)
                            .ok_or_else(|| format!("BYMONTHDAY {d}"))?;
                        r.by_month_day.push(n);
                    }
                }
                "UNTIL" => {
                    let digits: String = v.chars().filter(char::is_ascii_digit).take(8).collect();
                    if digits.len() != 8 {
                        return Err(format!("UNTIL {v}"));
                    }
                    let iso = format!("{}-{}-{}", &digits[..4], &digits[4..6], &digits[6..]);
                    days(&iso).ok_or_else(|| format!("UNTIL {v}"))?;
                    r.until = Some(iso);
                }
                "COUNT" => {
                    r.count = Some(v.parse().map_err(|_| format!("COUNT {v}"))?);
                }
                "WKST" => {} // weeks start on Monday here
                other => return Err(format!("{other} is not supported")),
            }
        }
        r.freq = freq.ok_or("no FREQ")?;
        if r.freq != Freq::Monthly && r.by_day.iter().any(|(o, _)| *o != 0) {
            return Err("an ordinal BYDAY (1SU) needs FREQ=MONTHLY".into());
        }
        Ok(r)
    }

    /// The dates it falls on within `from..=to` (`YYYY-MM-DD`), starting on
    /// `start` (its first possible date: the event's own). Exceptions are
    /// the caller's to leave out; `COUNT` counts them, as RFC 5545 does.
    pub fn dates(&self, start: &str, from: &str, to: &str) -> Vec<String> {
        let (Some(s), Some(from), Some(to)) = (days(start), days(from), days(to)) else {
            return Vec::new();
        };
        let end = match self.until.as_deref().and_then(days) {
            Some(u) => u.min(to),
            None => to,
        };
        let step = i64::from(self.interval.max(1));
        let (sy, sm, sd) = civil(s);
        let mut out = Vec::new();
        let mut seen: u32 = 0;
        // Each period (a day, week, month or year) gives its dates in
        // order; stop past the end, or once COUNT is reached.
        for k in 0.. {
            let mut cands: Vec<i64> = match self.freq {
                Freq::Daily => {
                    let d = s + k * step;
                    if self.by_day.is_empty() || self.by_day.iter().any(|(_, w)| *w == weekday(d)) {
                        vec![d]
                    } else {
                        vec![]
                    }
                }
                Freq::Weekly => {
                    let monday = s - i64::from(weekday(s)) + 7 * k * step;
                    let wds: Vec<u8> = if self.by_day.is_empty() {
                        vec![weekday(s)]
                    } else {
                        self.by_day.iter().map(|(_, w)| *w).collect()
                    };
                    wds.into_iter().map(|w| monday + i64::from(w)).collect()
                }
                Freq::Monthly => {
                    let months = i64::from(sm) - 1 + k * step;
                    let (y, m) = (
                        sy + months.div_euclid(12),
                        (months.rem_euclid(12) + 1) as u32,
                    );
                    self.month_days(y, m, sd)
                }
                Freq::Yearly => {
                    let y = sy + k * step;
                    from_civil(y, sm, sd).into_iter().collect()
                }
            };
            let period_start = match self.freq {
                Freq::Daily => s + k * step,
                Freq::Weekly => s - i64::from(weekday(s)) + 7 * k * step,
                Freq::Monthly => {
                    let months = i64::from(sm) - 1 + k * step;
                    from_civil(
                        sy + months.div_euclid(12),
                        (months.rem_euclid(12) + 1) as u32,
                        1,
                    )
                    .unwrap_or(i64::MAX)
                }
                Freq::Yearly => from_civil(sy + k * step, 1, 1).unwrap_or(i64::MAX),
            };
            if period_start > end {
                break;
            }
            cands.sort_unstable();
            cands.dedup();
            for d in cands.into_iter().filter(|d| *d >= s) {
                if d > end || self.count.is_some_and(|c| seen >= c) {
                    return out;
                }
                seen += 1;
                if d >= from {
                    out.push(iso(d));
                }
            }
            if self.count.is_some_and(|c| seen >= c) {
                break;
            }
        }
        out
    }

    /// The days of month `m` of year `y` a monthly rule falls on.
    fn month_days(&self, y: i64, m: u32, start_day: u32) -> Vec<i64> {
        let Some(first) = from_civil(y, m, 1) else {
            return Vec::new();
        };
        let len = month_len(y, m) as i64;
        let mut out = Vec::new();
        if !self.by_day.is_empty() {
            for (ord, w) in &self.by_day {
                let all: Vec<i64> = (0..len)
                    .map(|i| first + i)
                    .filter(|d| weekday(*d) == *w)
                    .collect();
                match *ord {
                    0 => out.extend(all),
                    n if n > 0 => out.extend(all.get(n as usize - 1)),
                    n => out.extend(all.len().checked_sub((-n) as usize).map(|i| all[i])),
                }
            }
        } else {
            let wanted: Vec<i8> = if self.by_month_day.is_empty() {
                vec![start_day as i8]
            } else {
                self.by_month_day.clone()
            };
            for n in wanted {
                let day = if n > 0 {
                    i64::from(n)
                } else {
                    len + 1 + i64::from(n)
                };
                // A day the month does not have is skipped (31 in April).
                if (1..=len).contains(&day) {
                    out.push(first + day - 1);
                }
            }
        }
        out
    }
}

impl fmt::Display for Rule {
    /// The rule as an `RRULE` value (`FREQ=WEEKLY;BYDAY=SU`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let freq = match self.freq {
            Freq::Daily => "DAILY",
            Freq::Weekly => "WEEKLY",
            Freq::Monthly => "MONTHLY",
            Freq::Yearly => "YEARLY",
        };
        write!(f, "FREQ={freq}")?;
        if self.interval > 1 {
            write!(f, ";INTERVAL={}", self.interval)?;
        }
        if !self.by_day.is_empty() {
            let days: Vec<String> = self
                .by_day
                .iter()
                .map(|(o, w)| {
                    let w = WEEKDAYS[usize::from(*w).min(6)];
                    if *o == 0 {
                        w.to_string()
                    } else {
                        format!("{o}{w}")
                    }
                })
                .collect();
            write!(f, ";BYDAY={}", days.join(","))?;
        }
        if !self.by_month_day.is_empty() {
            let d: Vec<String> = self.by_month_day.iter().map(i8::to_string).collect();
            write!(f, ";BYMONTHDAY={}", d.join(","))?;
        }
        if let Some(u) = &self.until {
            write!(f, ";UNTIL={}", u.replace('-', ""))?;
        }
        if let Some(c) = self.count {
            write!(f, ";COUNT={c}")?;
        }
        Ok(())
    }
}

// ── dates as days since 1970-01-01 (proleptic Gregorian) ───────────────

/// `YYYY-MM-DD` as a day number.
pub fn days(date: &str) -> Option<i64> {
    let mut it = date.trim().splitn(3, '-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    from_civil(y, m, d)
}

/// A day number as `YYYY-MM-DD`.
pub fn iso(days: i64) -> String {
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The weekday of a day number: 0 = Monday … 6 = Sunday.
pub fn weekday(days: i64) -> u8 {
    // 1970-01-01 was a Thursday.
    (days + 3).rem_euclid(7) as u8
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn month_len(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

fn from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || d == 0 || d > month_len(y, m) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dates(rule: &str, start: &str, from: &str, to: &str) -> Vec<String> {
        Rule::parse(rule).unwrap().dates(start, from, to)
    }

    #[test]
    fn calendar_arithmetic() {
        for d in [
            "1970-01-01",
            "2000-02-29",
            "2026-11-22",
            "1899-12-31",
            "2400-03-01",
        ] {
            assert_eq!(iso(days(d).unwrap()), d);
        }
        assert_eq!(weekday(days("2026-11-22").unwrap()), 6, "a Sunday");
        assert_eq!(weekday(days("2026-11-24").unwrap()), 1, "a Tuesday");
        assert!(days("2026-02-29").is_none());
        assert!(days("2026-13-01").is_none());
    }

    #[test]
    fn every_sunday_and_every_other_tuesday() {
        assert_eq!(
            dates(
                "FREQ=WEEKLY;BYDAY=SU",
                "2026-11-01",
                "2026-11-20",
                "2026-12-10"
            ),
            ["2026-11-22", "2026-11-29", "2026-12-06"]
        );
        // Without BYDAY: the start's weekday.
        assert_eq!(
            dates(
                "FREQ=WEEKLY;INTERVAL=2",
                "2026-11-03",
                "2026-11-01",
                "2026-12-15"
            ),
            ["2026-11-03", "2026-11-17", "2026-12-01", "2026-12-15"]
        );
        // Two weekdays, starting mid-week: nothing before the start.
        assert_eq!(
            dates(
                "FREQ=WEEKLY;BYDAY=TU,TH",
                "2026-11-26",
                "2026-11-01",
                "2026-12-04"
            ),
            ["2026-11-26", "2026-12-01", "2026-12-03"]
        );
    }

    #[test]
    fn monthly_by_weekday_or_day() {
        // The first Sunday, and the last Friday.
        assert_eq!(
            dates(
                "FREQ=MONTHLY;BYDAY=1SU",
                "2026-11-01",
                "2026-11-01",
                "2027-02-28"
            ),
            ["2026-11-01", "2026-12-06", "2027-01-03", "2027-02-07"]
        );
        assert_eq!(
            dates(
                "FREQ=MONTHLY;BYDAY=-1FR",
                "2026-11-01",
                "2026-11-01",
                "2026-12-31"
            ),
            ["2026-11-27", "2026-12-25"]
        );
        // The 31st: months without one are skipped; -1 is the last day.
        assert_eq!(
            dates("FREQ=MONTHLY", "2026-01-31", "2026-01-01", "2026-05-31"),
            ["2026-01-31", "2026-03-31", "2026-05-31"]
        );
        assert_eq!(
            dates(
                "FREQ=MONTHLY;BYMONTHDAY=-1",
                "2026-01-01",
                "2026-01-01",
                "2026-03-31"
            ),
            ["2026-01-31", "2026-02-28", "2026-03-31"]
        );
    }

    #[test]
    fn until_count_daily_and_yearly() {
        assert_eq!(
            dates(
                "FREQ=WEEKLY;BYDAY=SU;UNTIL=20261206",
                "2026-11-22",
                "2026-01-01",
                "2027-12-31"
            ),
            ["2026-11-22", "2026-11-29", "2026-12-06"]
        );
        // COUNT counts from the first date, even before the range.
        assert_eq!(
            dates(
                "FREQ=WEEKLY;BYDAY=TH;COUNT=4",
                "2026-11-05",
                "2026-11-20",
                "2027-12-31"
            ),
            ["2026-11-26"]
        );
        assert_eq!(
            dates(
                "FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR",
                "2026-11-20",
                "2026-11-20",
                "2026-11-25"
            ),
            ["2026-11-20", "2026-11-23", "2026-11-24", "2026-11-25"]
        );
        assert_eq!(
            dates("FREQ=YEARLY", "2024-02-29", "2024-01-01", "2028-12-31"),
            ["2024-02-29", "2028-02-29"]
        );
        // A rule from far back still expands quickly over a near range.
        assert_eq!(
            dates("FREQ=DAILY", "1990-01-01", "2026-11-22", "2026-11-23"),
            ["2026-11-22", "2026-11-23"]
        );
    }

    #[test]
    fn rules_read_and_write() {
        for r in [
            "FREQ=WEEKLY;BYDAY=SU",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,TH;UNTIL=20270601",
            "FREQ=MONTHLY;BYDAY=1SU,-1FR",
            "FREQ=MONTHLY;BYMONTHDAY=15;COUNT=6",
            "FREQ=DAILY",
        ] {
            assert_eq!(Rule::parse(r).unwrap().to_string(), r);
        }
        assert_eq!(Rule::weekly(&[6]).to_string(), "FREQ=WEEKLY;BYDAY=SU");
        assert_eq!(
            Rule::parse("RRULE:FREQ=WEEKLY;UNTIL=2027-06-01")
                .unwrap()
                .until
                .as_deref(),
            Some("2027-06-01")
        );
        for bad in [
            "",
            "BYDAY=SU",
            "FREQ=HOURLY",
            "FREQ=WEEKLY;BYDAY=1SU",
            "FREQ=DAILY;INTERVAL=0",
            "FREQ=WEEKLY;BYSETPOS=1",
        ] {
            assert!(Rule::parse(bad).is_err(), "{bad}");
        }
    }
}
