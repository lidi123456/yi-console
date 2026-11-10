//! Five-field cron expressions (minute hour day-of-month month day-of-week) in local time.
//! Supports `*`, lists, ranges and steps; day of week 0–7 (0 and 7 are Sunday). As in Vixie cron, when both day
//! fields are restricted a day matches if either does.

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDateTime, TimeZone, Timelike};

#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    minute: Vec<bool>,
    hour: Vec<bool>,
    dom: Vec<bool>,
    month: Vec<bool>,
    dow: Vec<bool>,
    dom_any: bool,
    dow_any: bool,
}

fn field(s: &str, min: u32, max: u32) -> Result<(Vec<bool>, bool)> {
    let mut set = vec![false; max as usize + 1];
    let any = s == "*";
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, st.parse::<u32>().map_err(|_| anyhow!("步长无效: {part}"))?),
            None => (part, 1),
        };
        if step == 0 {
            bail!("步长无效: {part}");
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().map_err(|_| anyhow!("范围无效: {part}"))?, b.parse().map_err(|_| anyhow!("范围无效: {part}"))?)
        } else {
            let v: u32 = range.parse().map_err(|_| anyhow!("取值无效: {part}"))?;
            (v, if part.contains('/') { max } else { v })
        };
        if lo < min || hi > max || lo > hi {
            bail!("超出范围: {part}");
        }
        let mut v = lo;
        while v <= hi {
            set[v as usize] = true;
            v += step;
        }
    }
    Ok((set, any))
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            bail!("cron 表达式需要 5 段（分 时 日 月 周）: {expr}");
        }
        let (minute, _) = field(parts[0], 0, 59)?;
        let (hour, _) = field(parts[1], 0, 23)?;
        let (dom, dom_any) = field(parts[2], 1, 31)?;
        let (month, _) = field(parts[3], 1, 12)?;
        let (mut dow, dow_any) = field(parts[4], 0, 7)?;
        if dow[7] {
            dow[0] = true;
            dow[7] = false;
        }
        Ok(Cron { minute, hour, dom, month, dow, dom_any, dow_any })
    }

    fn day_matches(&self, t: &NaiveDateTime) -> bool {
        let dom = self.dom[t.day() as usize];
        let dow = self.dow[t.weekday().num_days_from_sunday() as usize];
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }

    /// The first matching minute strictly after `after` (naive local time), within about four years
    pub fn next_naive(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        let mut t = after.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let limit = after + Duration::days(366 * 4);
        while t < limit {
            if !self.month[t.month() as usize] {
                let (y, m) = if t.month() == 12 { (t.year() + 1, 1) } else { (t.year(), t.month() + 1) };
                t = chrono::NaiveDate::from_ymd_opt(y, m, 1)?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.day_matches(&t) {
                t = (t.date() + Duration::days(1)).and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.hour[t.hour() as usize] {
                t = t.with_minute(0)? + Duration::hours(1);
                continue;
            }
            if !self.minute[t.minute() as usize] {
                t += Duration::minutes(1);
                continue;
            }
            return Some(t);
        }
        None
    }

    /// Next run in local time. A time skipped by a DST change moves to the next match
    pub fn next(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let mut t = after.naive_local();
        for _ in 0..8 {
            let n = self.next_naive(t)?;
            if let Some(local) = Local.from_local_datetime(&n).earliest() {
                return Some(local);
            }
            t = n;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn next_runs() {
        let c = Cron::parse("0 9 * * 1-5").unwrap();
        // 2026-11-07 is a Saturday
        assert_eq!(c.next_naive(at("2026-11-07 10:00")), Some(at("2026-11-09 09:00")));
        assert_eq!(c.next_naive(at("2026-11-09 08:59")), Some(at("2026-11-09 09:00")));
        assert_eq!(c.next_naive(at("2026-11-09 09:00")), Some(at("2026-11-10 09:00")));
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(c.next_naive(at("2026-11-09 10:07")), Some(at("2026-11-09 10:15")));
        let c = Cron::parse("30 8 1 * 0").unwrap();
        // Either the 1st or a Sunday: 2026-11-15 is a Sunday, 2026-12-01 the 1st
        assert_eq!(c.next_naive(at("2026-11-09 00:00")), Some(at("2026-11-15 08:30")));
        let c = Cron::parse("0 0 29 2 *").unwrap();
        assert_eq!(c.next_naive(at("2026-03-01 00:00")), Some(at("2028-02-29 00:00")));
        assert_eq!(Cron::parse("0 0 * * 7").unwrap(), Cron::parse("0 0 * * 0").unwrap());
    }

    #[test]
    fn bad_expressions() {
        for e in ["", "* * * *", "60 * * * *", "* * * * 8", "*/0 * * * *", "5-1 * * * *", "a * * * *"] {
            assert!(Cron::parse(e).is_err(), "{e}");
        }
    }
}
