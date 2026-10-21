//! ntfy (ntfy.sh or self-hosted) via the JSON publish API.

use serde_json::{Value, json};

use super::http::{OutRequest, join, list, opt, req};
use super::message::{Severity, body_text, headline, link, severity};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::notify::alert::{EventKind, NotificationEvent};

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "ntfy",
    label: "ntfy",
    icon: "bell",
    category: Category::Push,
    description: "Push to phones with ntfy.sh or your own server",
    docs_url: "https://docs.ntfy.sh/publish/",
    fields: &[
        Field::url("server", "Server")
            .default("https://ntfy.sh")
            .placeholder("https://ntfy.sh…"),
        Field::text("topic", "Topic")
            .required()
            .placeholder("vigilant-alerts…")
            .help("Pick something hard to guess on public servers."),
        Field::select(
            "priority",
            "Priority",
            &[
                ("auto", "Automatic"),
                ("5", "Urgent"),
                ("4", "High"),
                ("3", "Default"),
                ("2", "Low"),
                ("1", "Min"),
            ],
        )
        .default("auto")
        .help("Automatic: urgent for down, default for up, low for the rest."),
        Field::text("tags", "Extra tags").placeholder("prod, payments…"),
        Field::secret("access_token", "Access token")
            .placeholder("tk_…")
            .help("Or use a username and password."),
        Field::text("username", "Username"),
        Field::secret("password", "Password"),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    match opt(config, "topic") {
        Some(t)
            if !t
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            vec![field_error("topic", crate::i18n::t("notif.err_ntfy_topic"))]
        }
        _ => Vec::new(),
    }
}

fn auto_priority(kind: EventKind) -> i64 {
    match (kind, severity(kind)) {
        (EventKind::Escalation, _) => 5,
        (_, Severity::Critical) => 5,
        (_, Severity::Warning) => 4,
        (_, Severity::Ok) => 3,
        (_, Severity::Info) => 2,
    }
}

fn emoji_tag(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Down | EventKind::Reminder => "red_circle",
        EventKind::Escalation => "rotating_light",
        EventKind::Up => "green_circle",
        EventKind::CertExpiring | EventKind::DomainExpiring => "warning",
        EventKind::Flapping => "orange_circle",
        EventKind::MaintenanceStart | EventKind::MaintenanceEnd => "wrench",
        EventKind::Test => "bell",
    }
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let server = opt(config, "server").unwrap_or("https://ntfy.sh");
    let topic = req(config, "topic")?;
    let priority = match opt(config, "priority") {
        Some(p) if p != "auto" => p.parse().unwrap_or(3),
        _ => auto_priority(event.kind),
    };
    let mut tags = vec![emoji_tag(event.kind).to_string()];
    tags.extend(list(config, "tags"));
    // The title carries the headline; the emoji comes from the tag.
    let mut body = json!({
        "topic": topic,
        "title": headline(event),
        "message": body_text(event),
        "priority": priority,
        "tags": tags,
    });
    if let Some(url) = link(event) {
        body["click"] = json!(url);
    }
    let mut request = OutRequest::post_json(join(server, "/"), body);
    if let Some(token) = opt(config, "access_token") {
        request = request.bearer(token);
    } else if let Some(user) = opt(config, "username") {
        request = request.basic_auth(user, opt(config, "password").unwrap_or(""));
    }
    Ok(vec![request])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::tests::event;

    #[test]
    fn publishes_json_with_auto_priority_and_tags() {
        let mut e = event(EventKind::Down);
        e.message = Some("HTTP 503".into());
        e.url = Some("https://vv.example/monitors/7".into());
        let reqs = build(
            &json!({"topic": "alerts", "tags": "prod", "access_token": "tk_x"}),
            &e,
        )
        .unwrap();
        assert_eq!(reqs[0].url, "https://ntfy.sh/");
        assert_eq!(reqs[0].header_value("authorization"), Some("Bearer tk_x"));
        let body = reqs[0].json().unwrap();
        assert_eq!(body["topic"], "alerts");
        assert_eq!(body["priority"], 5);
        assert_eq!(body["tags"], json!(["red_circle", "prod"]));
        assert_eq!(body["title"], "Payments API is down");
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .starts_with("HTTP 503\nTarget:")
        );
        assert_eq!(body["click"], "https://vv.example/monitors/7");
    }

    #[test]
    fn fixed_priority_and_basic_auth_on_custom_server() {
        let e = event(EventKind::Up);
        let config = json!({"server": "https://ntfy.example/", "topic": "t", "priority": "2", "username": "u", "password": "p"});
        let req = &build(&config, &e).unwrap()[0];
        assert_eq!(req.url, "https://ntfy.example/");
        assert_eq!(req.json().unwrap()["priority"], 2);
        assert_eq!(req.header_value("authorization"), Some("Basic dTpw"));
    }
}
