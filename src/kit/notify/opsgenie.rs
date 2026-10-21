//! Opsgenie Alert API: create on problems (alias `vv-monitor-{id}`), close on
//! recovery.

use serde_json::{Value, json};

use super::http::{OutRequest, list, opt, path_segment, req};
use super::message::{Severity, body_text, headline, product, severity};
use super::pagerduty::dedup_key;
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::kit::notify::alert::{EventKind, NotificationEvent};

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "opsgenie",
    label: "Opsgenie",
    icon: "alert",
    category: Category::Incident,
    description: "Creates and closes alerts",
    docs_url: "https://support.atlassian.com/opsgenie/docs/create-a-default-api-integration/",
    fields: &[
        Field::secret("api_key", "API key")
            .required()
            .help("Settings → Integrations → API integration."),
        Field::select("region", "Region", &[("us", "US"), ("eu", "EU")]).default("us"),
        Field::select(
            "priority",
            "Priority when down",
            &[
                ("P1", "P1 · Critical"),
                ("P2", "P2 · High"),
                ("P3", "P3 · Moderate"),
                ("P4", "P4 · Low"),
                ("P5", "P5 · Informational"),
            ],
        )
        .default("P1"),
        Field::text("tags", "Tags").placeholder("prod, payments…"),
    ],
    transport: Transport::Http(build),
    check: None,
};

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let key = req(config, "api_key")?;
    let base = match opt(config, "region") {
        Some("eu") => "https://api.eu.opsgenie.com/v2/alerts",
        _ => "https://api.opsgenie.com/v2/alerts",
    };
    let alias = dedup_key(event);
    let auth = format!("GenieKey {key}");
    let request = match event.kind {
        EventKind::MaintenanceStart | EventKind::MaintenanceEnd => return Ok(Vec::new()),
        EventKind::Up => OutRequest::post_json(
            format!("{base}/{}/close?identifierType=alias", path_segment(&alias)),
            json!({ "source": product(), "note": super::message::summary(event) }),
        ),
        _ => {
            let priority = match severity(event.kind) {
                Severity::Critical => opt(config, "priority").unwrap_or("P1"),
                Severity::Warning => "P3",
                _ => "P5",
            };
            let message: String = headline(event).chars().take(130).collect();
            let mut tags = vec![super::message::slug().to_string()];
            tags.extend(list(config, "tags"));
            let mut body = json!({
                "message": message,
                "alias": alias,
                "description": body_text(event),
                "priority": priority,
                "source": product(),
                "tags": tags,
                "entity": event.monitor_name,
                "details": { "event": event.kind.as_str(), "target": event.target },
            });
            if let Some(url) = &event.url {
                body["details"]["url"] = json!(url);
            }
            OutRequest::post_json(base, body)
        }
    };
    Ok(vec![request.header("Authorization", auth)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::notify::tests::event;

    #[test]
    fn creates_then_closes_by_alias() {
        let config = json!({"api_key": "k", "region": "eu", "tags": "prod"});
        let down = &build(&config, &event(EventKind::Down)).unwrap()[0];
        assert_eq!(down.url, "https://api.eu.opsgenie.com/v2/alerts");
        assert_eq!(down.header_value("authorization"), Some("GenieKey k"));
        let body = down.json().unwrap();
        assert_eq!(body["alias"], "vv-monitor-7");
        assert_eq!(body["priority"], "P1");
        assert_eq!(body["tags"], json!(["vigilant-vole", "prod"]));
        let up = &build(&config, &event(EventKind::Up)).unwrap()[0];
        assert_eq!(
            up.url,
            "https://api.eu.opsgenie.com/v2/alerts/vv-monitor-7/close?identifierType=alias"
        );
    }
}
