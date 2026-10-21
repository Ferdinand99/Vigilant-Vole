//! On-call and incident tools driven by webhooks: SIGNL4, Squadcast, Grafana
//! OnCall, GoAlert, Splunk On-Call and Keep. Like PagerDuty they open an alert
//! when something breaks and close it on recovery, keyed by a stable id per
//! monitor; maintenance events send nothing.

use serde_json::{Value, json};

use super::http::{OutRequest, join, opt, path_segment, req};
use super::message::{Severity, PRODUCT, body_text, detail, headline, link, severity, summary};
use super::pagerduty::dedup_key;
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::db::models::{EventKind, NotificationEvent};

/// `true` for the recovery that closes an alert.
fn is_recovery(event: &NotificationEvent) -> bool {
    event.kind == EventKind::Up
}

/// Maintenance start/end never page anyone.
fn is_quiet(event: &NotificationEvent) -> bool {
    matches!(event.kind, EventKind::MaintenanceStart | EventKind::MaintenanceEnd)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

fn https_url_with_path(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| u.scheme() == "https" && u.path().len() > 1)
}

// -- SIGNL4 -------------------------------------------------------------------

pub const SIGNL4: ProviderSchema = ProviderSchema {
    kind: "signl4",
    label: "SIGNL4",
    icon: "alert",
    category: Category::Incident,
    description: "Mobile alerting with push, SMS and voice",
    docs_url: "https://connect.signl4.com/webhook/docs/index.html",
    fields: &[Field::url("url", "Webhook URL")
        .required()
        .sensitive()
        .placeholder("https://connect.signl4.com/webhook/…")
        .help("Your team’s webhook address from the SIGNL4 portal, under Integrations.")],
    transport: Transport::Http(build_signl4),
    check: Some(check_signl4),
};

fn check_signl4(config: &Value) -> Vec<FieldError> {
    match opt(config, "url") {
        Some(u) if !https_url_with_path(u) => vec![field_error("url", crate::i18n::t("notif.err_signl4"))],
        _ => Vec::new(),
    }
}

pub fn build_signl4(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    if is_quiet(event) {
        return Ok(Vec::new());
    }
    let mut body = json!({
        "Title": headline(event),
        "Message": body_text(event),
        "X-S4-ExternalID": dedup_key(event),
        "X-S4-Status": if is_recovery(event) { "resolved" } else { "new" },
        "X-S4-SourceSystem": PRODUCT,
        "X-S4-Service": event.monitor_name,
    });
    if let Some(url) = link(event) {
        body["Link"] = json!(url);
    }
    Ok(vec![OutRequest::post_json(url, body)])
}

// -- Squadcast ----------------------------------------------------------------

pub const SQUADCAST: ProviderSchema = ProviderSchema {
    kind: "squadcast",
    label: "Squadcast",
    icon: "alert",
    category: Category::Incident,
    description: "Incidents with on-call schedules and escalations",
    docs_url: "https://support.squadcast.com/integrations/incident-webhook-incident-webhook-api",
    fields: &[Field::url("url", "Webhook URL")
        .required()
        .sensitive()
        .placeholder("https://api.squadcast.com/v3/incidents/api/…")
        .help("Service → Alert Sources → Incident Webhook (API) → the URL.")],
    transport: Transport::Http(build_squadcast),
    check: Some(check_squadcast),
};

fn check_squadcast(config: &Value) -> Vec<FieldError> {
    match opt(config, "url") {
        Some(u) if !https_url_with_path(u) => vec![field_error("url", crate::i18n::t("notif.err_https_webhook"))],
        _ => Vec::new(),
    }
}

pub fn build_squadcast(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    if is_quiet(event) {
        return Ok(Vec::new());
    }
    let mut tags = json!({ "monitor": { "value": event.monitor_name }, "severity": { "value": format!("{:?}", severity(event.kind)).to_lowercase() } });
    if !event.target.is_empty() {
        tags["AlertAddress"] = json!(super::message::public_target(&event.target));
    }
    let body = json!({
        "message": headline(event),
        "description": body_text(event),
        "status": if is_recovery(event) { "resolve" } else { "trigger" },
        "event_id": dedup_key(event),
        "source": PRODUCT,
        "tags": tags,
    });
    Ok(vec![OutRequest::post_json(url, body)])
}

// -- Grafana OnCall -------------------------------------------------------------

pub const GRAFANA_ONCALL: ProviderSchema = ProviderSchema {
    kind: "grafana_oncall",
    label: "Grafana OnCall",
    icon: "alert",
    category: Category::Incident,
    description: "Alerts into Grafana OnCall escalation chains",
    docs_url: "https://grafana.com/docs/oncall/latest/configure/integrations/references/webhook/",
    fields: &[Field::url("url", "Webhook URL")
        .required()
        .sensitive()
        .placeholder("https://oncall-prod-eu-west-0.grafana.net/oncall/integrations/v1/webhook/…")
        .help("Integrations → New integration → Webhook → HTTP endpoint.")],
    transport: Transport::Http(build_grafana_oncall),
    check: Some(check_squadcast),
};

pub fn build_grafana_oncall(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    if is_quiet(event) {
        return Ok(Vec::new());
    }
    let mut body = json!({
        "alert_uid": dedup_key(event),
        "title": headline(event),
        "state": if is_recovery(event) { "ok" } else { "alerting" },
        "message": body_text(event),
    });
    if let Some(url) = link(event) {
        body["link_to_upstream_details"] = json!(url);
    }
    Ok(vec![OutRequest::post_json(url, body)])
}

// -- GoAlert --------------------------------------------------------------------

pub const GOALERT: ProviderSchema = ProviderSchema {
    kind: "goalert",
    label: "GoAlert",
    icon: "alert",
    category: Category::Incident,
    description: "Open-source on-call scheduling and alerting",
    docs_url: "https://goalert.me/docs/integrations/generic",
    fields: &[
        Field::url("server", "GoAlert URL").required().placeholder("https://goalert.example.com…"),
        Field::secret("token", "Integration key")
            .required()
            .help("Service → Integration Keys → add a Generic API key."),
    ],
    transport: Transport::Http(build_goalert),
    check: None,
};

pub fn build_goalert(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let server = req(config, "server")?;
    let token = req(config, "token")?;
    if is_quiet(event) {
        return Ok(Vec::new());
    }
    // The key travels in the form body, not in the URL, so it stays out of access logs.
    let mut fields = vec![
        ("token".to_string(), token.to_string()),
        ("summary".to_string(), truncate(&headline(event), 130)),
        ("details".to_string(), truncate(&body_text(event), 6000)),
        ("dedup".to_string(), dedup_key(event)),
    ];
    if is_recovery(event) {
        fields.push(("action".into(), "close".into()));
    }
    Ok(vec![OutRequest::post_form(join(server, "/api/v2/generic/incoming"), fields)])
}

// -- Splunk On-Call (VictorOps) --------------------------------------------------

pub const SPLUNK: ProviderSchema = ProviderSchema {
    kind: "splunk_oncall",
    label: "Splunk On-Call",
    icon: "alert",
    category: Category::Incident,
    description: "Incidents in Splunk On-Call (VictorOps)",
    docs_url: "https://help.victorops.com/knowledge-base/rest-endpoint-integration-guide/",
    fields: &[
        Field::secret("api_key", "API key")
            .required()
            .help("Integrations → 3rd Party Integrations → REST – Generic: the key in the endpoint URL."),
        Field::text("routing_key", "Routing key")
            .required()
            .placeholder("everyone…")
            .help("Decides which team gets paged."),
        Field::select("severity", "Severity when down", &[("CRITICAL", "Critical"), ("WARNING", "Warning")])
            .default("CRITICAL"),
    ],
    transport: Transport::Http(build_splunk),
    check: None,
};

pub fn build_splunk(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let key = req(config, "api_key")?;
    let routing = req(config, "routing_key")?;
    if is_quiet(event) {
        return Ok(Vec::new());
    }
    let message_type = match (event.kind, severity(event.kind)) {
        (EventKind::Up, _) => "RECOVERY",
        (EventKind::Test, _) => "INFO",
        (_, Severity::Critical) => opt(config, "severity").filter(|s| *s == "WARNING").unwrap_or("CRITICAL"),
        (_, Severity::Warning) => "WARNING",
        _ => "INFO",
    };
    let mut body = json!({
        "message_type": message_type,
        "entity_id": dedup_key(event),
        "entity_display_name": headline(event),
        "state_message": summary(event),
        "monitoring_tool": PRODUCT,
        "state_start_time": event.occurred_at.unix_timestamp(),
    });
    if let Some(url) = link(event) {
        body["vo_annotate.u.Monitor"] = json!(url);
    }
    if let Some(d) = detail(event) {
        body["state_message"] = json!(format!("{} — {d}", headline(event)));
    }
    let url = format!(
        "https://alert.victorops.com/integrations/generic/20131114/alert/{}/{}",
        path_segment(key),
        path_segment(routing)
    );
    Ok(vec![OutRequest::post_json(url, body)])
}

// -- Keep -----------------------------------------------------------------------

pub const KEEP: ProviderSchema = ProviderSchema {
    kind: "keep",
    label: "Keep",
    icon: "alert",
    category: Category::Incident,
    description: "Alerts into the Keep AIOps platform",
    docs_url: "https://docs.keephq.dev/alerts/sending-alerts",
    fields: &[
        Field::url("server", "Keep API URL")
            .required()
            .placeholder("https://api.keephq.dev…")
            .help("Your own Keep backend, or the cloud API."),
        Field::secret("api_key", "API key").required().help("Settings → API Keys."),
    ],
    transport: Transport::Http(build_keep),
    check: None,
};

pub fn build_keep(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let server = req(config, "server")?;
    let key = req(config, "api_key")?;
    if is_quiet(event) {
        return Ok(Vec::new());
    }
    let level = match severity(event.kind) {
        Severity::Critical => "critical",
        Severity::Warning => "warning",
        Severity::Ok | Severity::Info => "info",
    };
    let mut body = json!({
        "name": headline(event),
        "status": if is_recovery(event) { "resolved" } else { "firing" },
        "severity": level,
        "description": body_text(event),
        "source": ["vigilant-vole"],
        "service": event.monitor_name,
        "fingerprint": dedup_key(event),
    });
    if let Some(url) = link(event) {
        body["url"] = json!(url);
    }
    Ok(vec![OutRequest::post_json(join(server, "/alerts/event"), body).header("X-API-KEY", key)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::http::OutBody;
    use crate::notification::tests::event;

    #[test]
    fn signl4_opens_and_resolves_by_external_id() {
        let c = json!({"url": "https://connect.signl4.com/webhook/team-secret"});
        let down = &build_signl4(&c, &event(EventKind::Down)).unwrap()[0];
        let body = down.json().unwrap();
        assert_eq!((body["X-S4-Status"].as_str(), body["X-S4-ExternalID"].as_str()), (Some("new"), Some("vv-monitor-7")));
        assert_eq!(body["Title"], "Payments API is down");
        let up = &build_signl4(&c, &event(EventKind::Up)).unwrap()[0];
        assert_eq!(up.json().unwrap()["X-S4-Status"], "resolved");
        assert_eq!(up.json().unwrap()["X-S4-ExternalID"], "vv-monitor-7");
        assert!(build_signl4(&c, &event(EventKind::MaintenanceStart)).unwrap().is_empty());
        assert!(!check_signl4(&json!({"url": "http://connect.signl4.com/webhook/x"})).is_empty());
        assert!(check_signl4(&c).is_empty());
    }

    #[test]
    fn squadcast_and_grafana_oncall_trigger_and_resolve() {
        let sq = json!({"url": "https://api.squadcast.com/v3/incidents/api/abc"});
        let t = build_squadcast(&sq, &event(EventKind::Down)).unwrap().remove(0);
        assert_eq!((t.json().unwrap()["status"].as_str(), t.json().unwrap()["event_id"].as_str()), (Some("trigger"), Some("vv-monitor-7")));
        assert_eq!(build_squadcast(&sq, &event(EventKind::Up)).unwrap()[0].json().unwrap()["status"], "resolve");
        let gr = json!({"url": "https://oncall.example/integrations/v1/webhook/abc/"});
        let a = build_grafana_oncall(&gr, &event(EventKind::Down)).unwrap().remove(0);
        assert_eq!((a.json().unwrap()["state"].as_str(), a.json().unwrap()["alert_uid"].as_str()), (Some("alerting"), Some("vv-monitor-7")));
        assert_eq!(build_grafana_oncall(&gr, &event(EventKind::Up)).unwrap()[0].json().unwrap()["state"], "ok");
        assert!(build_grafana_oncall(&gr, &event(EventKind::MaintenanceEnd)).unwrap().is_empty());
        assert!(!check_squadcast(&json!({"url": "https://x.example"})).is_empty(), "a bare host has no token");
    }

    #[test]
    fn goalert_sends_a_form_with_the_key_in_the_body() {
        let c = json!({"server": "https://goalert.example/", "token": "KEY-123"});
        let r = &build_goalert(&c, &event(EventKind::Down)).unwrap()[0];
        assert_eq!(r.url, "https://goalert.example/api/v2/generic/incoming");
        assert!(!r.url.contains("KEY-123"));
        let OutBody::Form(f) = &r.body else { panic!("form") };
        let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!((get("token"), get("dedup"), get("action")), (Some("KEY-123"), Some("vv-monitor-7"), None));
        assert!(get("summary").unwrap().chars().count() <= 130);
        let OutBody::Form(up) = &build_goalert(&c, &event(EventKind::Up)).unwrap()[0].body else { panic!("form") };
        assert!(up.iter().any(|(k, v)| k == "action" && v == "close"));
        assert_eq!(truncate(&"x".repeat(200), 130).chars().count(), 130);
    }

    #[test]
    fn splunk_maps_severity_and_recovery() {
        let c = json!({"api_key": "KEY", "routing_key": "ops team"});
        let r = &build_splunk(&c, &event(EventKind::Down)).unwrap()[0];
        assert_eq!(r.url, "https://alert.victorops.com/integrations/generic/20131114/alert/KEY/ops%20team");
        assert_eq!(r.json().unwrap()["message_type"], "CRITICAL");
        assert_eq!(r.json().unwrap()["entity_id"], "vv-monitor-7");
        let warn = json!({"api_key": "KEY", "routing_key": "r", "severity": "WARNING"});
        assert_eq!(build_splunk(&warn, &event(EventKind::Down)).unwrap()[0].json().unwrap()["message_type"], "WARNING");
        assert_eq!(build_splunk(&c, &event(EventKind::Up)).unwrap()[0].json().unwrap()["message_type"], "RECOVERY");
        assert_eq!(build_splunk(&c, &event(EventKind::CertExpiring)).unwrap()[0].json().unwrap()["message_type"], "WARNING");
        assert_eq!(build_splunk(&c, &event(EventKind::Test)).unwrap()[0].json().unwrap()["message_type"], "INFO");
        assert!(build_splunk(&c, &event(EventKind::MaintenanceStart)).unwrap().is_empty());
    }

    #[test]
    fn keep_posts_firing_and_resolved_alerts_with_the_key_header() {
        let c = json!({"server": "https://keep.example/", "api_key": "K"});
        let mut e = event(EventKind::Down);
        e.url = Some("https://vv.example/monitors/7".into());
        let r = &build_keep(&c, &e).unwrap()[0];
        assert_eq!(r.url, "https://keep.example/alerts/event");
        assert_eq!(r.header_value("X-API-KEY"), Some("K"));
        let b = r.json().unwrap();
        assert_eq!((b["status"].as_str(), b["severity"].as_str(), b["fingerprint"].as_str()), (Some("firing"), Some("critical"), Some("vv-monitor-7")));
        assert_eq!(b["source"], json!(["vigilant-vole"]));
        assert_eq!(b["url"], "https://vv.example/monitors/7");
        assert_eq!(build_keep(&c, &event(EventKind::Up)).unwrap()[0].json().unwrap()["status"], "resolved");
    }
}
