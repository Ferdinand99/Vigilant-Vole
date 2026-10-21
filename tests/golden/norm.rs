//! Normalisation of volatile output, so a snapshot only changes when the
//! behaviour does. Every rule here hides one specific kind of noise:
//!
//! - CRLF line endings: a Windows checkout (`core.autocrlf`) embeds templates,
//!   static files and migrations with CRLF; CI checks out LF.
//! - The package version (bumped by every release) → `<VERSION>`.
//! - Signed cookie values (HMAC with a per-instance random key, timestamps)
//!   → `<signed>`; cookie `Expires` dates → `<date>`.
//! - Dates and times derived from "now" → `<DAY-n>` (n days ago, in the
//!   page's language and format), `<DATETIME>`, `<DATE>`, `<TIME>`.

use std::sync::LazyLock;

use axum::http::HeaderMap;
use regex::Regex;
use sha2::{Digest, Sha256};
use vigilant_vole::i18n::{self, Lang};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// CRLF → LF, the package version → `<VERSION>`.
pub fn text(s: &str) -> String {
    s.replace("\r\n", "\n").replace(VERSION, "<VERSION>")
}

pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Bytes of a text file with LF line endings (see the module docs).
pub fn lf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// `name=value; attrs` with the value and any expiry date hidden. An empty
/// value (a removal cookie) stays visibly empty.
pub fn set_cookie(raw: &str) -> String {
    static EXPIRES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Expires=[^;]+").unwrap());
    let (pair, attrs) = raw.split_once(';').unwrap_or((raw, ""));
    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
    let value = if value.is_empty() { "" } else { "<signed>" };
    let attrs = EXPIRES.replace_all(attrs, "Expires=<date>");
    format!("{name}={value};{attrs}")
}

/// Response headers, sorted by name, one per line. `date` is dropped,
/// `content-length` (which follows the CRLF/LF size of templates and static
/// files) is reduced to `<n>`, ETags (content hashes) to their shape, and
/// cookies are normalised with [`set_cookie`].
pub fn headers(h: &HeaderMap) -> String {
    static ETAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^(W/)?"[0-9a-f]+"$"#).unwrap());
    let mut names: Vec<&str> = h.keys().map(|k| k.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    let mut out = String::new();
    for name in names {
        let mut lines = Vec::new();
        for value in h.get_all(name) {
            let value = value.to_str().unwrap_or("<binary>");
            let shown = match name {
                "date" => continue,
                "content-length" => "<n>".to_string(),
                "etag" if ETAG.is_match(value) => "\"<hash>\"".to_string(),
                "set-cookie" => set_cookie(value),
                _ => text(value),
            };
            lines.push(format!("{name}: {shown}\n"));
        }
        // The cookie jar emits its changes in no fixed order.
        if name == "set-cookie" {
            lines.sort();
        }
        out.extend(lines);
    }
    out
}

/// A rendered page: [`text`], then everything derived from the current
/// date and time.
pub fn page(html: &str, lang: Lang) -> String {
    times(&dates(&text(html), lang))
}

/// Dates within the last 120 days (and the next 7), in every format the
/// interface uses for `lang`, become `<DAY-n>` (`<DAY+n>` for the future);
/// ISO timestamps become `<DATETIME>` / `<DATE>`.
pub fn dates(s: &str, lang: Lang) -> String {
    static ISO_DATETIME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2}| UTC)?")
            .unwrap()
    });
    static ISO_DATE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b\d{4}-\d{2}-\d{2}\b").unwrap());
    let s = ISO_DATETIME.replace_all(s, "<DATETIME>");
    let s = ISO_DATE.replace_all(&s, "<DATE>").into_owned();

    let today = jiff::Zoned::now()
        .with_time_zone(jiff::tz::TimeZone::UTC)
        .date();
    let mut forms: Vec<(String, String)> = Vec::new();
    for offset in -7i32..=120 {
        let Ok(date) = today.checked_sub(jiff::Span::new().days(offset)) else {
            continue;
        };
        let label = if offset >= 0 {
            format!("<DAY-{offset}>")
        } else {
            format!("<DAY+{}>", -offset)
        };
        for form in [
            i18n::fmt_weekday_day_month_in(lang, date),
            i18n::fmt_date_in(lang, date),
            i18n::fmt_day_month_long_in(lang, date),
            i18n::fmt_day_month_in(lang, date),
        ] {
            forms.push((form, label.clone()));
        }
    }
    // Longest first, so "Oct 1" never eats the start of "Oct 12".
    forms.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
    let mut out = s;
    for (form, label) in forms {
        if out.contains(&form) {
            out = out.replace(&form, &label);
        }
    }
    out
}

/// Clock times (`14:05`, `14:05:09`) → `<TIME>`.
pub fn times(s: &str) -> String {
    static TIME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b([01]\d|2[0-3]):[0-5]\d(:[0-5]\d)?\b").unwrap());
    TIME.replace_all(s, "<TIME>").into_owned()
}
