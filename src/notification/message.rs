//! How Vigilant Vole words its alerts: the headline, detail and facts of a
//! monitor event, and the adapter that turns a [`NotificationEvent`] into
//! the kit's [`Alert`]. The layout (summary, bodies, Markdown) is the kit's
//! [`crate::kit::notify::message`], re-exported here.

use crate::db::models::{EventKind, MonitorType, NotificationEvent};
use crate::kit::notify::alert::{Alert, AlertKind};
pub use crate::kit::notify::message::*;

/// The product's name.
pub const PRODUCT: &str = crate::product::NAME;

/// The kit's kind of alert for a monitor event.
pub fn alert_kind(kind: EventKind) -> AlertKind {
    match kind {
        EventKind::Down => AlertKind::Problem,
        EventKind::Up => AlertKind::Recovered,
        EventKind::Reminder => AlertKind::Reminder,
        EventKind::Escalation => AlertKind::Escalation,
        EventKind::CertExpiring | EventKind::DomainExpiring => AlertKind::Expiring,
        EventKind::MaintenanceStart => AlertKind::MaintenanceStarted,
        EventKind::MaintenanceEnd => AlertKind::MaintenanceEnded,
        EventKind::Flapping => AlertKind::Unstable,
        EventKind::Test => AlertKind::Test,
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

/// The finished alert for `event`, in the current language.
pub fn alert(event: &NotificationEvent) -> Alert {
    let kind = alert_kind(event.kind);
    let level = kind.level();
    Alert {
        kind,
        event: event.kind.as_str().to_string(),
        event_label: event.kind.label().to_string(),
        status: status_word(event.kind).to_string(),
        level,
        colour: level.colour(),
        emoji: kind.emoji().to_string(),
        title: headline(event),
        detail: detail(event),
        facts: facts(event)
            .into_iter()
            .map(|(label, value)| (label.to_string(), value))
            .collect(),
        link_label: crate::i18n::t("msg.view_monitor").to_string(),
        subject_id: event.monitor_id,
        subject: event.monitor_name.clone(),
        subject_type: event.monitor_type.clone(),
        target: event.target.clone(),
        message: event.message.clone(),
        duration: event.downtime,
        expires_in_days: event.expires_in_days,
        url: event.url.clone(),
        occurred_at: event.occurred_at,
    }
}

/// "🔴 Payments API is down — HTTP 503".
pub fn summary(event: &NotificationEvent) -> String {
    crate::kit::notify::message::summary(&alert(event))
}

/// Plain-text body: the summary, facts and the deep link, one per line.
pub fn text_body(event: &NotificationEvent) -> String {
    crate::kit::notify::message::text_body(&alert(event))
}

fn expiry_phrase(days: Option<i64>) -> String {
    match days {
        Some(d) if d < 0 => crate::i18n::t("msg.expired").into(),
        Some(0) => crate::i18n::t("msg.expires_today").into(),
        Some(1) => crate::i18n::t("msg.expires_tomorrow").into(),
        Some(d) => crate::i18n::tf("msg.expires_in", &[("n", &d.to_string())]),
        None => crate::i18n::t("msg.expiring_soon").into(),
    }
}

fn with_name(key: &'static str, name: &str) -> String {
    crate::i18n::tf(key, &[("name", name)])
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
            Some(d) => crate::i18n::tf(
                "msg.down_for_long",
                &[("name", name), ("duration", &format_duration(d))],
            ),
            None => with_name("msg.down_too_long", name),
        },
        EventKind::CertExpiring => crate::i18n::tf(
            "msg.cert",
            &[
                ("name", name),
                ("phrase", &expiry_phrase(event.expires_in_days)),
            ],
        ),
        EventKind::DomainExpiring => crate::i18n::tf(
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
            Some(d) => Some(crate::i18n::tf(
                "msg.down_for",
                &[("duration", &format_duration(d))],
            )),
            None => message.map(str::to_string),
        },
        EventKind::Reminder => {
            let down_for = event
                .downtime
                .map(|d| crate::i18n::tf("msg.down_for", &[("duration", &format_duration(d))]));
            match (message, down_for) {
                (Some(m), Some(d)) => Some(format!("{m} · {d}")),
                (Some(m), None) => Some(m.to_string()),
                (None, d) => d,
            }
        }
        EventKind::Flapping => Some(message.map_or_else(
            || crate::i18n::t("msg.flap_paused").to_string(),
            str::to_string,
        )),
        _ => message.map(str::to_string),
    }
}

/// Facts worth showing as fields: `(label, value)`.
pub fn facts(event: &NotificationEvent) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if !event.target.is_empty() {
        out.push((crate::i18n::t("msg.fact_target"), event.target.clone()));
    }
    if !event.monitor_type.is_empty() {
        out.push((
            crate::i18n::t("msg.fact_type"),
            monitor_type_label(&event.monitor_type),
        ));
    }
    if let (EventKind::Up | EventKind::Reminder | EventKind::Escalation, Some(d)) =
        (event.kind, event.downtime)
    {
        out.push((crate::i18n::t("msg.fact_downtime"), format_duration(d)));
    }
    out.push((
        crate::i18n::t("msg.fact_time"),
        format_time(event.occurred_at),
    ));
    out
}

fn monitor_type_label(t: &str) -> String {
    MonitorType::parse(t).map_or_else(|| t.to_string(), |m| m.label().to_string())
}

/// A test event for "Send test".
pub fn test_event(site_name: &str, base_url: Option<&str>) -> NotificationEvent {
    NotificationEvent {
        kind: EventKind::Test,
        monitor_id: None,
        monitor_name: site_name.to_string(),
        monitor_type: String::new(),
        target: String::new(),
        message: Some(crate::i18n::t("msg.test_body").into()),
        downtime: None,
        expires_in_days: None,
        url: base_url
            .map(|b| b.trim_end_matches('/').to_string())
            .filter(|b| !b.is_empty()),
        occurred_at: time::OffsetDateTime::now_utc(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::notification::tests_support::event;

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
