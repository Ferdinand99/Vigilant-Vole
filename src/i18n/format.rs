//! Locale-aware number, duration and date formatting.
//!
//! English: `1,234`, `99.95%`, `Oct 19, 2026, 14:05`.
//! Norsk bokmål: `1 234`, `99,95 %`, `19. okt. 2026, 14:05` (non-breaking
//! spaces where a line break would split a value from its unit).

use jiff::Zoned;
use jiff::civil::Date;

use super::{Lang, current};

/// Non-breaking space (Norwegian thousands separator and `99,95 %`).
const NBSP: char = '\u{a0}';

const MONTHS_SHORT_EN: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS_LONG_EN: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
// Bokmål abbreviations as Apple/CLDR write them: the short ones take a period.
const MONTHS_SHORT_NB: [&str; 12] = [
    "jan.", "feb.", "mars", "apr.", "mai", "juni", "juli", "aug.", "sep.", "okt.", "nov.", "des.",
];
const MONTHS_LONG_NB: [&str; 12] = [
    "januar",
    "februar",
    "mars",
    "april",
    "mai",
    "juni",
    "juli",
    "august",
    "september",
    "oktober",
    "november",
    "desember",
];
const WEEKDAYS_SHORT_EN: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const WEEKDAYS_LONG_EN: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const WEEKDAYS_SHORT_NB: [&str; 7] = ["man.", "tir.", "ons.", "tor.", "fre.", "lør.", "søn."];
const WEEKDAYS_LONG_NB: [&str; 7] = ["mandag", "tirsdag", "onsdag", "torsdag", "fredag", "lørdag", "søndag"];

fn group_digits(digits: &str, sep: char) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

/// "1,234" / "1 234" (no grouping below 10 000 in Bokmål, as CLDR does).
pub fn fmt_int_in(lang: Lang, n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let grouped = match lang {
        Lang::En => group_digits(&digits, ','),
        Lang::Nb if digits.len() <= 4 => digits,
        Lang::Nb => group_digits(&digits, NBSP),
    };
    if n < 0 { format!("-{grouped}") } else { grouped }
}

pub fn fmt_int(n: i64) -> String {
    fmt_int_in(current(), n)
}

/// Fixed decimals with the locale's decimal mark and grouping.
pub fn fmt_dec_in(lang: Lang, value: f64, decimals: usize) -> String {
    if !value.is_finite() {
        return "—".into();
    }
    let s = format!("{:.*}", decimals, value.abs());
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let int: i64 = int.parse().unwrap_or(0);
    let mut out = String::new();
    // "-0.00" reads as zero.
    if value < 0.0 && s.chars().any(|c| matches!(c, '1'..='9')) {
        out.push('-');
    }
    out.push_str(&fmt_int_in(lang, int));
    if !frac.is_empty() {
        out.push(match lang {
            Lang::En => '.',
            Lang::Nb => ',',
        });
        out.push_str(frac);
    }
    out
}

pub fn fmt_dec(value: f64, decimals: usize) -> String {
    fmt_dec_in(current(), value, decimals)
}

/// A percentage that never rounds up to a misleading 100: "99.95%" /
/// "99,95 %", exactly 100 → "100%" / "100 %".
pub fn fmt_pct2_in(lang: Lang, p: f64) -> String {
    let number = if p >= 100.0 {
        "100".to_string()
    } else {
        fmt_dec_in(lang, (p * 100.0).floor() / 100.0, 2)
    };
    match lang {
        Lang::En => format!("{number}%"),
        Lang::Nb => format!("{number}{NBSP}%"),
    }
}

pub fn fmt_pct2(p: f64) -> String {
    fmt_pct2_in(current(), p)
}

/// "120 ms" ("1 234 ms" in Bokmål, grouped with a non-breaking space).
pub fn fmt_ms(ms: i64) -> String {
    format!("{} ms", fmt_int(ms))
}

/// Human duration with two units at most: "45 sec", "12 min", "3 h 4 min",
/// "2 d 5 h" / "45 sek", "12 min", "3 t 4 min", "2 d 5 t".
pub fn fmt_duration_in(lang: Lang, secs: i64) -> String {
    let secs = secs.max(0);
    let (sec, hour) = match lang {
        Lang::En => ("sec", "h"),
        Lang::Nb => ("sek", "t"),
    };
    let (d, h, m, s) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s} {sec}"),
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} {hour}"),
        (0, h, m) => format!("{h} {hour} {m} min"),
        (d, 0, _) => format!("{d} d"),
        (d, h, _) => format!("{d} d {h} {hour}"),
    }
}

pub fn fmt_duration(secs: i64) -> String {
    fmt_duration_in(current(), secs)
}

fn month_idx(date: Date) -> usize {
    usize::try_from(date.month() - 1).unwrap_or(0).min(11)
}

fn weekday_idx(date: Date) -> usize {
    usize::try_from(date.weekday().to_monday_zero_offset())
        .unwrap_or(0)
        .min(6)
}

/// "Oct 3" / "3. okt."
pub fn fmt_day_month_in(lang: Lang, date: Date) -> String {
    let m = month_idx(date);
    match lang {
        Lang::En => format!("{} {}", MONTHS_SHORT_EN[m], date.day()),
        Lang::Nb => format!("{}. {}", date.day(), MONTHS_SHORT_NB[m]),
    }
}

pub fn fmt_day_month(date: Date) -> String {
    fmt_day_month_in(current(), date)
}

/// "October 3" / "3. oktober"
pub fn fmt_day_month_long_in(lang: Lang, date: Date) -> String {
    let m = month_idx(date);
    match lang {
        Lang::En => format!("{} {}", MONTHS_LONG_EN[m], date.day()),
        Lang::Nb => format!("{}. {}", date.day(), MONTHS_LONG_NB[m]),
    }
}

pub fn fmt_day_month_long(date: Date) -> String {
    fmt_day_month_long_in(current(), date)
}

/// "Oct 3, 2026" / "3. okt. 2026"
pub fn fmt_date_in(lang: Lang, date: Date) -> String {
    match lang {
        Lang::En => format!("{}, {}", fmt_day_month_in(lang, date), date.year()),
        Lang::Nb => format!("{} {}", fmt_day_month_in(lang, date), date.year()),
    }
}

pub fn fmt_date(date: Date) -> String {
    fmt_date_in(current(), date)
}

/// "Oct 3, 14:05" / "3. okt., 14:05"
pub fn fmt_short_datetime_in(lang: Lang, z: &Zoned) -> String {
    format!("{}, {}", fmt_day_month_in(lang, z.date()), z.strftime("%H:%M"))
}

pub fn fmt_short_datetime(z: &Zoned) -> String {
    fmt_short_datetime_in(current(), z)
}

/// "Oct 3, 2026, 14:05" / "3. okt. 2026, 14:05"
pub fn fmt_datetime_in(lang: Lang, z: &Zoned) -> String {
    format!("{}, {}", fmt_date_in(lang, z.date()), z.strftime("%H:%M"))
}

pub fn fmt_datetime(z: &Zoned) -> String {
    fmt_datetime_in(current(), z)
}

/// Weekday name; `iso` is 1 (Monday) … 7 (Sunday).
pub fn weekday_name_in(lang: Lang, iso: i8, long: bool) -> &'static str {
    let i = usize::try_from(iso.clamp(1, 7) - 1).unwrap_or(0);
    match (lang, long) {
        (Lang::En, false) => WEEKDAYS_SHORT_EN[i],
        (Lang::En, true) => WEEKDAYS_LONG_EN[i],
        (Lang::Nb, false) => WEEKDAYS_SHORT_NB[i],
        (Lang::Nb, true) => WEEKDAYS_LONG_NB[i],
    }
}

pub fn weekday_name(iso: i8, long: bool) -> &'static str {
    weekday_name_in(current(), iso, long)
}

/// "Mon, Oct 3" / "man. 3. okt."
pub fn fmt_weekday_day_month_in(lang: Lang, date: Date) -> String {
    let w = weekday_idx(date);
    match lang {
        Lang::En => format!("{}, {}", WEEKDAYS_SHORT_EN[w], fmt_day_month_in(lang, date)),
        Lang::Nb => format!("{} {}", WEEKDAYS_SHORT_NB[w], fmt_day_month_in(lang, date)),
    }
}

/// Coarse relative time from a number of seconds in the past (negative =
/// future): "just now", "3 min ago", "in 2 h" / "akkurat nå", "for 3 min
/// siden", "om 2 t".
pub fn fmt_relative_in(lang: Lang, secs_ago: i64) -> String {
    if secs_ago.abs() < 45 {
        return match lang {
            Lang::En => "just now".into(),
            Lang::Nb => "akkurat nå".into(),
        };
    }
    let span = fmt_duration_in(lang, round_span(secs_ago.abs()));
    match (lang, secs_ago > 0) {
        (Lang::En, true) => format!("{span} ago"),
        (Lang::En, false) => format!("in {span}"),
        (Lang::Nb, true) => format!("for {span} siden"),
        (Lang::Nb, false) => format!("om {span}"),
    }
}

pub fn fmt_relative(secs_ago: i64) -> String {
    fmt_relative_in(current(), secs_ago)
}

/// Rounds to the largest unit so relative times read "3 h", not "3 h 4 min".
fn round_span(secs: i64) -> i64 {
    match secs {
        s if s < 3_600 => (s + 30) / 60 * 60,
        s if s < 86_400 => (s + 1_800) / 3_600 * 3_600,
        s => (s + 43_200) / 86_400 * 86_400,
    }
}
