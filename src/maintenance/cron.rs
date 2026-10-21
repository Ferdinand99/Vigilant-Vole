//! A small 5-field cron parser for maintenance windows (`minute hour
//! day-of-month month day-of-week`), with the usual Vixie semantics:
//! `*`, lists (`1,15`), ranges (`1-5`), steps (`*/10`, `8-18/2`), month and
//! weekday names (`jan`, `mon-fri`), `0` or `7` for Sunday, and the macros
//! `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`. When both
//! day-of-month and day-of-week are restricted, either one matches a day.

use jiff::civil::Date;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSpec {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    /// Bit 0 = Sunday … bit 6 = Saturday.
    weekdays: u8,
    days_restricted: bool,
    weekdays_restricted: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// One field: its bitmask plus whether it was a bare `*` (or `*/n`).
fn parse_field(
    src: &str,
    min: u32,
    max: u32,
    names: &[&str],
    name_base: u32,
    label: &str,
) -> Result<(u64, bool), String> {
    let value = |tok: &str| -> Result<u32, String> {
        if let Ok(n) = tok.parse::<u32>() {
            return Ok(n);
        }
        names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(tok))
            .map(|i| i as u32 + name_base)
            .ok_or_else(|| format!("“{tok}” isn’t valid in the {label} field."))
    };
    let mut bits = 0u64;
    let mut star = false;
    for part in src.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| format!("“{s}” isn’t a valid step in the {label} field."))?;
                if step == 0 {
                    return Err(format!("A step of 0 isn’t valid in the {label} field."));
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" || range == "?" {
            star |= part == range || range == "*";
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let v = value(range)?;
            // "5/15" means "from 5 to the end, every 15".
            (v, if step > 1 { max } else { v })
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!(
                "{lo}-{hi} is outside {min}-{max} in the {label} field."
            ));
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, star))
}

impl CronSpec {
    pub fn parse(src: &str) -> Result<Self, String> {
        let src = src.trim();
        let expanded = match src {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "Use five fields: minute hour day-of-month month day-of-week (got {}).",
                fields.len()
            ));
        }
        let (minutes, _) = parse_field(fields[0], 0, 59, &[], 0, "minute")?;
        let (hours, _) = parse_field(fields[1], 0, 23, &[], 0, "hour")?;
        let (days, days_star) = parse_field(fields[2], 1, 31, &[], 0, "day-of-month")?;
        let (months, _) = parse_field(fields[3], 1, 12, &MONTHS, 1, "month")?;
        let (mut weekdays, weekdays_star) =
            parse_field(fields[4], 0, 7, &WEEKDAYS, 0, "day-of-week")?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }
        Ok(CronSpec {
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: weekdays as u8,
            days_restricted: !days_star,
            weekdays_restricted: !weekdays_star,
        })
    }

    /// Whether the expression can fire on `date` at all.
    pub fn matches_day(&self, date: Date) -> bool {
        if self.months & (1 << date.month()) == 0 {
            return false;
        }
        let day_ok = self.days & (1 << date.day()) != 0;
        let weekday_ok = self.weekdays & (1 << date.weekday().to_sunday_zero_offset()) != 0;
        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => day_ok || weekday_ok,
            (true, false) => day_ok,
            (false, true) => weekday_ok,
            (false, false) => true,
        }
    }

    /// The `(hour, minute)` pairs at which it fires on a matching day, in order.
    pub fn times(&self) -> impl Iterator<Item = (i8, i8)> + '_ {
        (0..24i8)
            .filter(|h| self.hours & (1 << h) != 0)
            .flat_map(move |h| {
                (0..60i8)
                    .filter(move |m| self.minutes & (1 << m) != 0)
                    .map(move |m| (h, m))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn fires(spec: &str, d: Date) -> bool {
        CronSpec::parse(spec).unwrap().matches_day(d)
    }

    #[test]
    fn parses_lists_ranges_steps_and_names() {
        let c = CronSpec::parse("0,30 2-4 * * mon-fri").unwrap();
        assert_eq!(
            c.times().collect::<Vec<_>>(),
            vec![(2, 0), (2, 30), (3, 0), (3, 30), (4, 0), (4, 30)]
        );
        assert_eq!(
            CronSpec::parse("*/20 0 * * *")
                .unwrap()
                .times()
                .collect::<Vec<_>>(),
            vec![(0, 0), (0, 20), (0, 40)]
        );
        assert_eq!(
            CronSpec::parse("5/25 1 * * *")
                .unwrap()
                .times()
                .collect::<Vec<_>>(),
            vec![(1, 5), (1, 30), (1, 55)]
        );
        assert_eq!(
            CronSpec::parse("0 8-18/5 * * *")
                .unwrap()
                .times()
                .collect::<Vec<_>>(),
            vec![(8, 0), (13, 0), (18, 0)]
        );
        assert_eq!(
            CronSpec::parse("0 0 1 JAN,jul *").unwrap().times().count(),
            1
        );
    }

    #[test]
    fn macros_expand() {
        assert_eq!(
            CronSpec::parse("@daily").unwrap(),
            CronSpec::parse("0 0 * * *").unwrap()
        );
        assert_eq!(
            CronSpec::parse("@weekly").unwrap(),
            CronSpec::parse("0 0 * * 0").unwrap()
        );
        assert_eq!(
            CronSpec::parse("@monthly").unwrap(),
            CronSpec::parse("0 0 1 * *").unwrap()
        );
        assert_eq!(CronSpec::parse("@hourly").unwrap().times().count(), 24);
    }

    #[test]
    fn sunday_is_zero_or_seven() {
        let sunday = date(2026, 10, 18);
        assert!(
            fires("0 0 * * 0", sunday)
                && fires("0 0 * * 7", sunday)
                && fires("0 0 * * sun", sunday)
        );
        assert!(!fires("0 0 * * 1-5", sunday));
        assert!(fires("0 0 * * 5-7", sunday));
    }

    #[test]
    fn day_of_month_and_weekday_combine_with_or_when_both_are_set() {
        // The 13th, or any Friday.
        let spec = "0 0 13 * fri";
        assert!(fires(spec, date(2026, 10, 13))); // a Tuesday, the 13th
        assert!(fires(spec, date(2026, 10, 16))); // a Friday, the 16th
        assert!(!fires(spec, date(2026, 10, 14)));
        // Only one restricted: plain AND with the wildcard.
        assert!(!fires("0 0 13 * *", date(2026, 10, 16)));
        assert!(!fires("0 0 * * fri", date(2026, 10, 14)));
    }

    #[test]
    fn months_and_leap_days() {
        assert!(fires("0 0 29 2 *", date(2028, 2, 29)));
        assert!(!fires("0 0 29 2 *", date(2028, 3, 29)));
        assert!(fires("0 0 31 * *", date(2026, 1, 31)));
        assert!(!fires("0 0 31 * *", date(2026, 4, 30)));
    }

    #[test]
    fn rejects_malformed_expressions_with_a_reason() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "a * * * *",
            "5-1 * * * *",
            "* * * foo *",
            "1- * * * *",
        ] {
            assert!(CronSpec::parse(bad).is_err(), "{bad:?} should not parse");
        }
        assert!(CronSpec::parse("* * * * *").is_ok());
    }
}
