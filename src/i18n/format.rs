//! Locale-aware number, duration and date formatting.
//!
//! Number conventions (decimal mark, grouping, `%` spacing) come from the
//! language registry; everything a translator writes — month and weekday
//! names, date patterns, duration units, relative-time phrases — comes from
//! the `[fmt]` section of the language's locale file.
//!
//! English: `1,234`, `99.95%`, `Oct 19, 2026, 14:05`.
//! Norsk bokmål: `1234`, `12 345`, `99,95 %`, `19. okt. 2026, 14:05`
//! (non-breaking spaces where a line break would split a value from its unit).

use jiff::Zoned;
use jiff::civil::Date;

use super::{Lang, current, interpolate, lookup};

/// Writes the ASCII digits of `text` in the language's digit set (Persian
/// digits for `fa`); a no-op for languages that use Latin digits.
pub fn local_digits(lang: Lang, text: String) -> String {
    let zero = lang.info().numbers.zero;
    if zero == '0' {
        return text;
    }
    text.chars()
        .map(|c| match c.to_digit(10) {
            Some(d) if c.is_ascii_digit() => char::from_u32(u32::from(zero) + d).unwrap_or(c),
            _ => c,
        })
        .collect()
}

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

/// "1,234" / "1.234" / "1 234": grouped from the digit count the language
/// asks for (CLDR `minimumGroupingDigits`).
pub fn fmt_int_in(lang: Lang, n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let nf = lang.info().numbers;
    let grouped = if digits.len() < 3 + nf.min_grouping { digits } else { group_digits(&digits, nf.group) };
    local_digits(lang, if n < 0 { format!("-{grouped}") } else { grouped })
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
        out.push(lang.info().numbers.decimal);
        out.push_str(&local_digits(lang, frac.to_string()));
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
    let nf = lang.info().numbers;
    let sign = nf.percent_sign;
    let space = nf.percent_space.map(String::from).unwrap_or_default();
    if nf.percent_before { format!("{sign}{space}{number}") } else { format!("{number}{space}{sign}") }
}

pub fn fmt_pct2(p: f64) -> String {
    fmt_pct2_in(current(), p)
}

/// "120 ms" ("1 234 ms" where the language groups with a space).
pub fn fmt_ms(ms: i64) -> String {
    format!("{} ms", fmt_int(ms))
}

/// A `[fmt]` pattern with its `{placeholders}` filled in.
fn pattern(lang: Lang, key: &str, args: &[(&str, &str)]) -> String {
    local_digits(lang, interpolate(lookup(lang, &format!("fmt.{key}")).unwrap_or_default(), args))
}

/// The `i`-th entry of a `|`-separated `[fmt]` list (month and weekday names).
fn list_item(lang: Lang, key: &str, i: usize) -> &'static str {
    lookup(lang, &format!("fmt.{key}")).and_then(|list| list.split('|').nth(i)).unwrap_or_default()
}

/// Human duration with two units at most: "45 sec", "12 min", "3 h 4 min",
/// "2 d 5 h" (the unit patterns are the language's `fmt.dur_*`).
pub fn fmt_duration_in(lang: Lang, secs: i64) -> String {
    let secs = secs.max(0);
    let unit = |key: &str, n: i64| pattern(lang, key, &[("n", &n.to_string())]);
    let (d, h, m, s) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => unit("dur_sec", s),
        (0, 0, m) => unit("dur_min", m),
        (0, h, 0) => unit("dur_hour", h),
        (0, h, m) => format!("{} {}", unit("dur_hour", h), unit("dur_min", m)),
        (d, 0, _) => unit("dur_day", d),
        (d, h, _) => format!("{} {}", unit("dur_day", d), unit("dur_hour", h)),
    }
}

pub fn fmt_duration(secs: i64) -> String {
    fmt_duration_in(current(), secs)
}

/// Compact duration for notifications: "3d 4h", "2h 5m", "4m 10s", "12s"
/// (the unit patterns are the language's `fmt.compact_*`).
pub fn fmt_duration_compact_in(lang: Lang, secs: i64) -> String {
    let secs = secs.max(0);
    let unit = |key: &str, n: i64| pattern(lang, key, &[("n", &n.to_string())]);
    let (d, h, m, s) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => unit("compact_sec", s),
        (0, 0, m) => format!("{} {}", unit("compact_min", m), unit("compact_sec", s)),
        (0, h, m) => format!("{} {}", unit("compact_hour", h), unit("compact_min", m)),
        (d, h, _) => format!("{} {}", unit("compact_day", d), unit("compact_hour", h)),
    }
}

pub fn fmt_duration_compact(secs: i64) -> String {
    fmt_duration_compact_in(current(), secs)
}

fn month_idx(date: Date) -> usize {
    usize::try_from(date.month() - 1).unwrap_or(0).min(11)
}

fn weekday_idx(date: Date) -> usize {
    usize::try_from(date.weekday().to_monday_zero_offset())
        .unwrap_or(0)
        .min(6)
}

fn day_month_with(lang: Lang, date: Date, key: &str, months: &str) -> String {
    pattern(
        lang,
        key,
        &[("day", &date.day().to_string()), ("month", list_item(lang, months, month_idx(date)))],
    )
}

/// "Oct 3" / "3. okt."
pub fn fmt_day_month_in(lang: Lang, date: Date) -> String {
    day_month_with(lang, date, "day_month", "months_short")
}

pub fn fmt_day_month(date: Date) -> String {
    fmt_day_month_in(current(), date)
}

/// "October 3" / "3. oktober"
pub fn fmt_day_month_long_in(lang: Lang, date: Date) -> String {
    day_month_with(lang, date, "day_month_long", "months_long")
}

pub fn fmt_day_month_long(date: Date) -> String {
    fmt_day_month_long_in(current(), date)
}

/// "Oct 3, 2026" / "3. okt. 2026"
pub fn fmt_date_in(lang: Lang, date: Date) -> String {
    pattern(
        lang,
        "date",
        &[("day_month", &fmt_day_month_in(lang, date)), ("year", &date.year().to_string())],
    )
}

pub fn fmt_date(date: Date) -> String {
    fmt_date_in(current(), date)
}

/// "Oct 3, 14:05" / "3. okt., 14:05"
pub fn fmt_short_datetime_in(lang: Lang, z: &Zoned) -> String {
    pattern(
        lang,
        "short_datetime",
        &[("day_month", &fmt_day_month_in(lang, z.date())), ("time", &z.strftime("%H:%M").to_string())],
    )
}

pub fn fmt_short_datetime(z: &Zoned) -> String {
    fmt_short_datetime_in(current(), z)
}

/// "Oct 3, 2026, 14:05" / "3. okt. 2026, 14:05"
pub fn fmt_datetime_in(lang: Lang, z: &Zoned) -> String {
    pattern(
        lang,
        "datetime",
        &[("date", &fmt_date_in(lang, z.date())), ("time", &z.strftime("%H:%M").to_string())],
    )
}

pub fn fmt_datetime(z: &Zoned) -> String {
    fmt_datetime_in(current(), z)
}

/// Weekday name; `iso` is 1 (Monday) … 7 (Sunday).
pub fn weekday_name_in(lang: Lang, iso: i8, long: bool) -> &'static str {
    let i = usize::try_from(iso.clamp(1, 7) - 1).unwrap_or(0);
    list_item(lang, if long { "weekdays_long" } else { "weekdays_short" }, i)
}

pub fn weekday_name(iso: i8, long: bool) -> &'static str {
    weekday_name_in(current(), iso, long)
}

/// "Mon, Oct 3" / "man. 3. okt."
pub fn fmt_weekday_day_month_in(lang: Lang, date: Date) -> String {
    pattern(
        lang,
        "weekday_day_month",
        &[
            ("weekday", list_item(lang, "weekdays_short", weekday_idx(date))),
            ("day_month", &fmt_day_month_in(lang, date)),
        ],
    )
}

/// Coarse relative time from a number of seconds in the past (negative =
/// future): "just now", "3 min ago", "in 2 h" / "akkurat nå", "for 3 min
/// siden", "om 2 t".
pub fn fmt_relative_in(lang: Lang, secs_ago: i64) -> String {
    if secs_ago.abs() < 45 {
        return pattern(lang, "just_now", &[]);
    }
    let span = fmt_duration_in(lang, round_span(secs_ago.abs()));
    pattern(lang, if secs_ago > 0 { "ago" } else { "future" }, &[("span", &span)])
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
