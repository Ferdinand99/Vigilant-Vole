//! One voice for every provider: headline, detail, emoji, colour and severity
//! of an event, so Discord, email and PagerDuty all say the same thing.

use std::time::Duration;

use time::OffsetDateTime;
use time::macros::format_description;

use crate::kit::notify::alert::{EventKind, NotificationEvent};

/// The product's name, from its [`KitConfig`](crate::kit::config::KitConfig).
pub fn product() -> &'static str {
    crate::kit::config::get().product
}

/// The product's lower-case name (tags, sources).
pub fn slug() -> &'static str {
    crate::kit::config::get().slug
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Critical,
    Warning,
    Ok,
    Info,
}

pub fn severity(kind: EventKind) -> Severity {
    match kind {
        EventKind::Down | EventKind::Reminder | EventKind::Escalation => Severity::Critical,
        EventKind::CertExpiring | EventKind::DomainExpiring | EventKind::Flapping => {
            Severity::Warning
        }
        EventKind::Up => Severity::Ok,
        EventKind::MaintenanceStart | EventKind::MaintenanceEnd | EventKind::Test => Severity::Info,
    }
}

/// RGB colour (Apple system colours) for embeds/cards.
pub fn color(kind: EventKind) -> u32 {
    match severity(kind) {
        Severity::Critical => 0xFF3B30,
        Severity::Warning => 0xFF9500,
        Severity::Ok => 0x34C759,
        Severity::Info => 0x007AFF,
    }
}

pub fn color_hex(kind: EventKind) -> String {
    format!("#{:06X}", color(kind))
}

pub fn emoji(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Down | EventKind::Reminder => "\u{1f534}",
        EventKind::Escalation => "\u{1f6a8}",
        EventKind::Up => "\u{1f7e2}",
        EventKind::CertExpiring | EventKind::DomainExpiring => "\u{1f7e1}",
        EventKind::Flapping => "\u{1f7e0}",
        EventKind::MaintenanceStart | EventKind::MaintenanceEnd => "\u{1f535}",
        EventKind::Test => "\u{1f514}",
    }
}

/// "up" / "down" / … — the coarse status a receiver can switch on.
pub fn status_word(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Up => "up",
        EventKind::Down | EventKind::Reminder | EventKind::Escalation => "down",
        EventKind::MaintenanceStart => "maintenance",
        other => other.as_str(),
    }
}

fn expiry_phrase(days: Option<i64>) -> String {
    match days {
        Some(d) if d < 0 => crate::kit::i18n::t("msg.expired").into(),
        Some(0) => crate::kit::i18n::t("msg.expires_today").into(),
        Some(1) => crate::kit::i18n::t("msg.expires_tomorrow").into(),
        Some(d) => crate::kit::i18n::tf("msg.expires_in", &[("n", &d.to_string())]),
        None => crate::kit::i18n::t("msg.expiring_soon").into(),
    }
}

fn with_name(key: &'static str, name: &str) -> String {
    crate::kit::i18n::tf(key, &[("name", name)])
}

/// "Payments API is down" — no emoji, no detail.
pub fn headline(event: &NotificationEvent) -> String {
    let name = &event.monitor_name;
    match event.kind {
        EventKind::Down => with_name("msg.down", name),
        EventKind::Up if event.downtime.is_some() => with_name("msg.back_up", name),
        EventKind::Up => with_name("msg.up", name),
        EventKind::Reminder => with_name("msg.still_down", name),
        EventKind::Escalation => match event.downtime {
            Some(d) => crate::kit::i18n::tf(
                "msg.down_for_long",
                &[("name", name), ("duration", &format_duration(d))],
            ),
            None => with_name("msg.down_too_long", name),
        },
        EventKind::CertExpiring => crate::kit::i18n::tf(
            "msg.cert",
            &[
                ("name", name),
                ("phrase", &expiry_phrase(event.expires_in_days)),
            ],
        ),
        EventKind::DomainExpiring => crate::kit::i18n::tf(
            "msg.domain",
            &[
                ("name", name),
                ("phrase", &expiry_phrase(event.expires_in_days)),
            ],
        ),
        EventKind::MaintenanceStart => with_name("msg.maintenance", name),
        EventKind::MaintenanceEnd => with_name("msg.maintenance_ended", name),
        EventKind::Flapping => with_name("msg.flapping", name),
        EventKind::Test => with_name("msg.test", name),
    }
}

/// The part after the dash: the check message, how long it was down, …
pub fn detail(event: &NotificationEvent) -> Option<String> {
    let message = event
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    match event.kind {
        EventKind::Up => match event.downtime {
            Some(d) => Some(crate::kit::i18n::tf(
                "msg.down_for",
                &[("duration", &format_duration(d))],
            )),
            None => message.map(str::to_string),
        },
        EventKind::Reminder => {
            let down_for = event.downtime.map(|d| {
                crate::kit::i18n::tf("msg.down_for", &[("duration", &format_duration(d))])
            });
            match (message, down_for) {
                (Some(m), Some(d)) => Some(format!("{m} · {d}")),
                (Some(m), None) => Some(m.to_string()),
                (None, d) => d,
            }
        }
        EventKind::Flapping => Some(message.map_or_else(
            || crate::kit::i18n::t("msg.flap_paused").to_string(),
            str::to_string,
        )),
        _ => message.map(str::to_string),
    }
}

/// "🔴 Payments API is down — HTTP 503".
pub fn summary(event: &NotificationEvent) -> String {
    let mut text = format!("{} {}", emoji(event.kind), headline(event));
    if let Some(d) = detail(event) {
        text.push_str(" — ");
        text.push_str(&d);
    }
    text
}

/// "🔴 Payments API is down" (subjects, titles).
pub fn title(event: &NotificationEvent) -> String {
    format!("{} {}", emoji(event.kind), headline(event))
}

/// Facts worth showing as fields: `(label, value)`.
pub fn facts(event: &NotificationEvent) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if !event.target.is_empty() {
        out.push((crate::kit::i18n::t("msg.fact_target"), event.target.clone()));
    }
    if !event.monitor_type.is_empty() {
        out.push((
            crate::kit::i18n::t("msg.fact_type"),
            monitor_type_label(&event.monitor_type),
        ));
    }
    if let (EventKind::Up | EventKind::Reminder | EventKind::Escalation, Some(d)) =
        (event.kind, event.downtime)
    {
        out.push((crate::kit::i18n::t("msg.fact_downtime"), format_duration(d)));
    }
    out.push((
        crate::kit::i18n::t("msg.fact_time"),
        format_time(event.occurred_at),
    ));
    out
}

fn monitor_type_label(t: &str) -> String {
    (crate::kit::config::get().subject_type_label)(t)
}

/// Plain-text body: the summary, facts and the deep link, one per line.
pub fn text_body(event: &NotificationEvent) -> String {
    let mut lines = vec![summary(event)];
    for (label, value) in facts(event) {
        lines.push(format!("{label}: {value}"));
    }
    if let Some(url) = &event.url {
        lines.push(url.clone());
    }
    lines.join("\n")
}

/// Body for title + body providers (push apps): detail, facts and link,
/// one per line — everything except the headline.
pub fn body_text(event: &NotificationEvent) -> String {
    let mut lines = Vec::new();
    if let Some(d) = detail(event) {
        lines.push(d);
    }
    for (label, value) in facts(event) {
        lines.push(format!("{label}: {value}"));
    }
    if let Some(url) = &event.url {
        lines.push(url.clone());
    }
    lines.join(
        "
",
    )
}

/// The deep link, else an http(s) target.
pub fn link(event: &NotificationEvent) -> Option<String> {
    event.url.clone().or_else(|| {
        (event.target.starts_with("http://") || event.target.starts_with("https://"))
            .then(|| event.target.clone())
    })
}

/// A monitor target that is safe to put in a notification: URLs lose their
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
    crate::kit::i18n::fmt_duration_compact(i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
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
pub fn markdown_body(event: &NotificationEvent) -> String {
    format!("**{}**\n{}", title(event), markdown_details(event))
}

/// [`markdown_body`] without the title line (for providers with a title field).
pub fn markdown_details(event: &NotificationEvent) -> String {
    let mut lines = Vec::new();
    if let Some(d) = detail(event) {
        lines.push(d);
        lines.push(String::new());
    }
    lines.extend(facts(event).into_iter().map(|(l, v)| format!("{l}: {v}")));
    if let Some(url) = &event.url {
        lines.push(String::new());
        lines.push(format!(
            "[{}]({url})",
            crate::kit::i18n::t("msg.view_monitor")
        ));
    }
    lines.join("\n")
}

/// A test event for "Send test".
pub fn test_event(site_name: &str, base_url: Option<&str>) -> NotificationEvent {
    NotificationEvent {
        kind: EventKind::Test,
        monitor_id: None,
        monitor_name: site_name.to_string(),
        monitor_type: String::new(),
        target: String::new(),
        message: Some(crate::kit::i18n::t("msg.test_body").into()),
        downtime: None,
        expires_in_days: None,
        url: base_url
            .map(|b| b.trim_end_matches('/').to_string())
            .filter(|b| !b.is_empty()),
        occurred_at: OffsetDateTime::now_utc(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::notify::tests::event;

    #[test]
    fn down_summary_reads_naturally() {
        let mut e = event(EventKind::Down);
        e.message = Some("HTTP 503 (after 3 retries)".into());
        assert_eq!(
            summary(&e),
            "\u{1f534} Payments API is down — HTTP 503 (after 3 retries)"
        );
    }

    #[test]
    fn up_summary_says_how_long_it_was_down() {
        let mut e = event(EventKind::Up);
        e.message = Some("200 OK".into());
        e.downtime = Some(Duration::from_secs(12 * 60 + 4));
        assert_eq!(
            summary(&e),
            "\u{1f7e2} Payments API is back up — down for 12m 4s"
        );
        e.downtime = None;
        assert_eq!(summary(&e), "\u{1f7e2} Payments API is up — 200 OK");
    }

    #[test]
    fn expiry_wording_handles_edges() {
        let mut e = event(EventKind::CertExpiring);
        for (days, want) in [
            (Some(7), "expires in 7 days"),
            (Some(1), "expires tomorrow"),
            (Some(0), "expires today"),
            (Some(-2), "has expired"),
        ] {
            e.expires_in_days = days;
            assert_eq!(
                headline(&e),
                format!("TLS certificate for Payments API {want}")
            );
        }
        let mut d = event(EventKind::DomainExpiring);
        d.expires_in_days = Some(14);
        assert_eq!(headline(&d), "Domain for Payments API expires in 14 days");
    }

    #[test]
    fn reminder_and_escalation_include_duration() {
        let mut e = event(EventKind::Reminder);
        e.message = Some("timeout".into());
        e.downtime = Some(Duration::from_secs(3725));
        assert_eq!(
            summary(&e),
            "\u{1f534} Payments API is still down — timeout · down for 1h 2m"
        );
        e.kind = EventKind::Escalation;
        assert_eq!(headline(&e), "Payments API has been down for 1h 2m");
    }

    #[test]
    fn format_duration_picks_two_units() {
        assert_eq!(format_duration(Duration::from_secs(12)), "12s");
        assert_eq!(format_duration(Duration::from_secs(250)), "4m 10s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h 2m");
        assert_eq!(format_duration(Duration::from_secs(90_000)), "1d 1h");
    }

    #[test]
    fn text_body_has_facts_and_link() {
        let mut e = event(EventKind::Down);
        e.url = Some("https://vv.example/monitors/7".into());
        let body = text_body(&e);
        assert!(body.starts_with("\u{1f534} Payments API is down\n"));
        assert!(body.contains("Target: https://pay.example/health"));
        assert!(body.contains("Type: HTTP(S)"));
        assert!(body.contains("Time: 2026-10-19 14:03:12 UTC"));
        assert!(body.ends_with("https://vv.example/monitors/7"));
    }

    #[test]
    fn escape_html_escapes_markup() {
        assert_eq!(escape_html("<b>&\"'"), "&lt;b&gt;&amp;&quot;&#39;");
    }
}
