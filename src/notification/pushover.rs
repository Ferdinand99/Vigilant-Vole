//! Pushover messages API, with emergency priority (retry until acknowledged)
//! available for down alerts.

use serde_json::{Value, json};

use super::http::{OutRequest, num, opt, req};
use super::message::{Severity, body_text, severity, title};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::db::models::NotificationEvent;

const DOWN_PRIORITIES: &[(&str, &str)] = &[
    ("0", "Normal"),
    ("1", "High"),
    ("2", "Emergency (repeats until acknowledged)"),
];
const OTHER_PRIORITIES: &[(&str, &str)] = &[
    ("-2", "Lowest"),
    ("-1", "Low"),
    ("0", "Normal"),
    ("1", "High"),
];

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "pushover",
    label: "Pushover",
    icon: "bell",
    category: Category::Push,
    description: "iOS, Android and desktop push",
    docs_url: "https://pushover.net/api",
    fields: &[
        Field::secret("user_key", "User key")
            .required()
            .help("On your Pushover dashboard."),
        Field::secret("app_token", "App token")
            .required()
            .help("Create an application to get one."),
        Field::text("device", "Device")
            .placeholder("iphone…")
            .help("Leave empty to notify all devices."),
        Field::select("down_priority", "Priority when down", DOWN_PRIORITIES).default("1"),
        Field::select("priority", "Priority for other alerts", OTHER_PRIORITIES).default("0"),
        Field::text("sound", "Sound").placeholder("pushover…"),
        Field::number("retry", "Retry every (seconds)")
            .default("60")
            .show_when("down_priority=2"),
        Field::number("expire", "Stop retrying after (seconds)")
            .default("3600")
            .show_when("down_priority=2"),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if num(config, "retry").is_some_and(|r| r < 30) {
        errors.push(field_error("retry", crate::i18n::t("notif.err_pushover_retry")));
    }
    if num(config, "expire").is_some_and(|e| !(1..=10_800).contains(&e)) {
        errors.push(field_error("expire", crate::i18n::t("notif.err_pushover_expire")));
    }
    errors
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let user = req(config, "user_key")?;
    let token = req(config, "app_token")?;
    let priority: i64 = if severity(event.kind) == Severity::Critical {
        opt(config, "down_priority")
            .and_then(|p| p.parse().ok())
            .unwrap_or(1)
    } else {
        opt(config, "priority")
            .and_then(|p| p.parse().ok())
            .unwrap_or(0)
    };
    let mut body = json!({
        "token": token,
        "user": user,
        "title": title(event),
        "message": body_text(event),
        "priority": priority,
        "timestamp": event.occurred_at.unix_timestamp(),
    });
    if priority == 2 {
        body["retry"] = json!(num(config, "retry").unwrap_or(60).max(30));
        body["expire"] = json!(num(config, "expire").unwrap_or(3600).clamp(30, 10_800));
    }
    if let Some(url) = &event.url {
        body["url"] = json!(url);
        body["url_title"] = json!(crate::i18n::t("msg.view_monitor"));
    }
    for key in ["device", "sound"] {
        if let Some(v) = opt(config, key) {
            body[key] = json!(v);
        }
    }
    Ok(vec![OutRequest::post_json(
        "https://api.pushover.net/1/messages.json",
        body,
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::EventKind;
    use crate::notification::tests::event;

    #[test]
    fn emergency_priority_adds_retry_and_expire_for_down_only() {
        let config = json!({"user_key": "u", "app_token": "t", "down_priority": "2", "priority": "-1", "retry": 10});
        let down = build(&config, &event(EventKind::Down)).unwrap();
        let body = down[0].json().unwrap();
        assert_eq!(body["priority"], 2);
        assert_eq!(body["retry"], 30, "clamped to Pushover's minimum");
        assert_eq!(body["expire"], 3600);
        let up = build(&config, &event(EventKind::Up)).unwrap();
        let body = up[0].json().unwrap();
        assert_eq!(body["priority"], -1);
        assert!(body.get("retry").is_none());
    }
}
