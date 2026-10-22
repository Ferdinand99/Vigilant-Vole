//! PagerDuty Events API v2: trigger on problems, resolve on recovery, keyed
//! by a stable dedup key per monitor.

use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use super::http::{OutRequest, opt, req};
use super::message::{Severity, link, product, summary};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::notify::alert::{Alert, AlertKind};

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "pagerduty",
    label: "PagerDuty",
    icon: "alert",
    category: Category::Incident,
    description: "Opens and resolves incidents automatically",
    docs_url: "https://developer.pagerduty.com/docs/events-api-v2/overview/",
    fields: &[
        Field::secret("routing_key", "Integration key")
            .required()
            .help("Service → Integrations → Events API v2 → Integration key."),
        Field::select(
            "severity",
            "Severity when down",
            &[
                ("critical", "Critical"),
                ("error", "Error"),
                ("warning", "Warning"),
                ("info", "Info"),
            ],
        )
        .default("critical"),
        Field::select("region", "Region", &[("us", "US"), ("eu", "EU")]).default("us"),
    ],
    transport: Transport::Http(build),
    check: None,
};

/// Stable incident key: `vv-monitor-{id}` (expiry alerts get their own).
pub fn dedup_key(event: &Alert) -> String {
    let base = match event.subject_id {
        Some(id) => format!("{}-monitor-{id}", super::message::id_prefix()),
        None => format!("{}-test", super::message::id_prefix()),
    };
    match event.kind {
        // "cert_expiring" → "-cert", "domain_expiring" → "-domain".
        AlertKind::Expiring => format!(
            "{base}-{}",
            event
                .event
                .strip_suffix("_expiring")
                .unwrap_or(&event.event)
        ),
        _ => base,
    }
}

pub fn build(config: &Value, event: &Alert) -> Result<Vec<OutRequest>, String> {
    let routing_key = req(config, "routing_key")?;
    let url = match opt(config, "region") {
        Some("eu") => "https://events.eu.pagerduty.com/v2/enqueue",
        _ => "https://events.pagerduty.com/v2/enqueue",
    };
    let action = match event.kind {
        AlertKind::Recovered => "resolve",
        AlertKind::MaintenanceStarted | AlertKind::MaintenanceEnded => return Ok(Vec::new()),
        _ => "trigger",
    };
    let mut body = json!({
        "routing_key": routing_key,
        "event_action": action,
        "dedup_key": dedup_key(event),
        "client": product(),
    });
    if action == "trigger" {
        let sev = match event.level {
            Severity::Critical => opt(config, "severity").unwrap_or("critical"),
            Severity::Warning => "warning",
            _ => "info",
        };
        let mut summary = summary(event);
        if summary.chars().count() > 1000 {
            summary = summary.chars().take(1000).collect();
        }
        body["payload"] = json!({
            "summary": summary,
            "source": if event.target.is_empty() { product().to_string() } else { event.target.clone() },
            "severity": sev,
            "timestamp": event.occurred_at.format(&Rfc3339).unwrap_or_default(),
            "component": event.subject,
            "class": event.subject_type,
            "custom_details": {
                "event": event.event.as_str(),
                "message": event.message,
                "expires_in_days": event.expires_in_days,
            },
        });
        if let Some(href) = link(event) {
            body["links"] = json!([{ "href": href, "text": event.link_label.as_str() }]);
        }
        if let Some(url) = &event.url {
            body["client_url"] = json!(url);
        }
    }
    Ok(vec![OutRequest::post_json(url, body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::tests::event;

    #[test]
    fn triggers_on_down_and_resolves_on_up_with_same_key() {
        let config = json!({"routing_key": "R0UT1NG", "severity": "error"});
        let down = build(&config, &event(AlertKind::Problem)).unwrap();
        let body = down[0].json().unwrap();
        assert_eq!(down[0].url, "https://events.pagerduty.com/v2/enqueue");
        assert_eq!(body["event_action"], "trigger");
        assert_eq!(body["dedup_key"], "vv-monitor-7");
        assert_eq!(body["payload"]["severity"], "error");
        let up = build(&config, &event(AlertKind::Recovered)).unwrap();
        let body = up[0].json().unwrap();
        assert_eq!(body["event_action"], "resolve");
        assert_eq!(body["dedup_key"], "vv-monitor-7");
        assert!(body.get("payload").is_none());
    }

    #[test]
    fn maintenance_sends_nothing_and_eu_region_switches_host() {
        let config = json!({"routing_key": "R", "region": "eu"});
        assert!(
            build(&config, &event(AlertKind::MaintenanceStarted))
                .unwrap()
                .is_empty()
        );
        let cert = build(&config, &event(AlertKind::Expiring)).unwrap();
        assert_eq!(cert[0].url, "https://events.eu.pagerduty.com/v2/enqueue");
        assert_eq!(cert[0].json().unwrap()["dedup_key"], "vv-monitor-7-cert");
        assert_eq!(cert[0].json().unwrap()["payload"]["severity"], "warning");
    }
}
