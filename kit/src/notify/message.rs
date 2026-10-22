//! One voice for every provider: how an [`Alert`] reads as a summary, a
//! title, a plain-text or Markdown body, so Discord, email and PagerDuty all
//! say the same thing. The texts themselves come finished in the alert.

use std::time::Duration;

use time::OffsetDateTime;
use time::macros::format_description;

use crate::notify::alert::Alert;
pub use crate::notify::alert::Level as Severity;

/// The product's name, from its [`KitConfig`](crate::config::KitConfig).
pub fn product() -> &'static str {
    crate::config::get().product
}

/// The product's lower-case name (tags, sources).
pub fn slug() -> &'static str {
    crate::config::get().slug
}

/// Short lower-case prefix for ids the product sends out (incident keys,
/// card and transaction ids): the environment prefix, "vv" for `VV`.
pub fn id_prefix() -> String {
    crate::config::get().env_prefix.to_ascii_lowercase()
}

/// `#RRGGBB` of the alert's colour.
pub fn color_hex(alert: &Alert) -> String {
    format!("#{:06X}", alert.colour)
}

/// "Payments API is down" — no emoji, no detail.
pub fn headline(alert: &Alert) -> String {
    alert.title.clone()
}

/// The part after the dash: the check message, how long it was down, …
pub fn detail(alert: &Alert) -> Option<String> {
    alert.detail.clone()
}

/// Facts worth showing as fields: `(label, value)`.
pub fn facts(alert: &Alert) -> Vec<(String, String)> {
    alert.facts.clone()
}

/// "🔴 Payments API is down — HTTP 503".
pub fn summary(alert: &Alert) -> String {
    let mut text = format!("{} {}", alert.emoji, alert.title);
    if let Some(d) = &alert.detail {
        text.push_str(" — ");
        text.push_str(d);
    }
    text
}

/// "🔴 Payments API is down" (subjects, titles).
pub fn title(alert: &Alert) -> String {
    format!("{} {}", alert.emoji, alert.title)
}

/// Plain-text body: the summary, facts and the deep link, one per line.
pub fn text_body(alert: &Alert) -> String {
    let mut lines = vec![summary(alert)];
    for (label, value) in &alert.facts {
        lines.push(format!("{label}: {value}"));
    }
    if let Some(url) = &alert.url {
        lines.push(url.clone());
    }
    lines.join("\n")
}

/// Body for title + body providers (push apps): detail, facts and link,
/// one per line — everything except the headline.
pub fn body_text(alert: &Alert) -> String {
    let mut lines = Vec::new();
    if let Some(d) = &alert.detail {
        lines.push(d.clone());
    }
    for (label, value) in &alert.facts {
        lines.push(format!("{label}: {value}"));
    }
    if let Some(url) = &alert.url {
        lines.push(url.clone());
    }
    lines.join("\n")
}

/// The deep link, else an http(s) target.
pub fn link(alert: &Alert) -> Option<String> {
    alert.url.clone().or_else(|| {
        (alert.target.starts_with("http://") || alert.target.starts_with("https://"))
            .then(|| alert.target.clone())
    })
}

/// A target that is safe to put in a notification: URLs lose their
/// username, password, query string and fragment (where tokens tend to live).
/// Anything that isn't a URL with a host (`host:port`, container names) is
/// returned unchanged.
pub fn public_target(target: &str) -> String {
    match url::Url::parse(target.trim()) {
        Ok(mut u) if u.has_host() => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        _ => target.to_string(),
    }
}

/// "2026-10-19 14:03:12 UTC".
pub fn format_time(t: OffsetDateTime) -> String {
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let utc = t.to_offset(time::UtcOffset::UTC);
    utc.format(&fmt)
        .map(|s| format!("{s} UTC"))
        .unwrap_or_default()
}

/// "3d 4h", "2h 5m", "4m 10s", "12s".
pub fn format_duration(d: Duration) -> String {
    // The unit patterns are the language's `fmt.compact_*` ("3d 4h", "3d 4t").
    crate::i18n::fmt_duration_compact(i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Escapes `& < > " '` for HTML bodies (Telegram, Matrix, email).
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Markdown body (Mattermost, Rocket.Chat, DingTalk, Webex, Gotify):
/// bold title, detail, facts, link.
pub fn markdown_body(alert: &Alert) -> String {
    format!("**{}**\n{}", title(alert), markdown_details(alert))
}

/// [`markdown_body`] without the title line (for providers with a title field).
pub fn markdown_details(alert: &Alert) -> String {
    let mut lines = Vec::new();
    if let Some(d) = &alert.detail {
        lines.push(d.clone());
        lines.push(String::new());
    }
    lines.extend(alert.facts.iter().map(|(l, v)| format!("{l}: {v}")));
    if let Some(url) = &alert.url {
        lines.push(String::new());
        lines.push(format!("[{}]({url})", alert.link_label));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::AlertKind;
    use crate::notify::tests::event;

    #[test]
    fn summary_joins_emoji_title_and_detail() {
        let mut e = event(AlertKind::Problem);
        e.detail = Some("HTTP 503 (after 3 retries)".into());
        assert_eq!(
            summary(&e),
            "\u{1f534} Payments API is down — HTTP 503 (after 3 retries)"
        );
        e.detail = None;
        assert_eq!(summary(&e), "\u{1f534} Payments API is down");
    }

    #[test]
    fn text_body_has_facts_and_link() {
        let mut e = event(AlertKind::Problem);
        e.url = Some("https://vv.example/monitors/7".into());
        let body = text_body(&e);
        assert!(body.starts_with("\u{1f534} Payments API is down\n"));
        assert!(body.contains("Target: https://pay.example/health"));
        assert!(body.contains("Type: HTTP(S)"));
        assert!(body.contains("Time: 2026-10-19 14:03:12 UTC"));
        assert!(body.ends_with("https://vv.example/monitors/7"));
    }

    #[test]
    fn markdown_links_with_the_alert_label() {
        let mut e = event(AlertKind::Problem);
        e.url = Some("https://vv.example/monitors/7".into());
        assert!(
            markdown_details(&e).ends_with("\n\n[View monitor](https://vv.example/monitors/7)")
        );
    }

    #[test]
    fn format_duration_picks_two_units() {
        assert_eq!(format_duration(Duration::from_secs(12)), "12s");
        assert_eq!(format_duration(Duration::from_secs(250)), "4m 10s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h 2m");
        assert_eq!(format_duration(Duration::from_secs(90_000)), "1d 1h");
    }

    #[test]
    fn escape_html_escapes_markup() {
        assert_eq!(escape_html("<b>&\"'"), "&lt;b&gt;&amp;&quot;&#39;");
    }
}
