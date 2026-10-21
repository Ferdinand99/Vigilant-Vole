//! Bark (iOS push) — `POST {server}/push`.

use serde_json::{Value, json};

use super::http::{OutRequest, join, opt, req};
use super::message::{Severity, body_text, product, severity, title};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::notify::alert::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "bark",
    label: "Bark",
    icon: "bell",
    category: Category::Push,
    description: "Simple iOS push",
    docs_url: "https://bark.day.app/#/en-us/tutorial",
    fields: &[
        Field::url("server", "Server")
            .default("https://api.day.app")
            .placeholder("https://api.day.app…"),
        Field::secret("device_key", "Device key")
            .required()
            .help("Shown in the Bark app."),
        Field::text("group", "Group").default("Vigilant Vole"),
        Field::text("sound", "Sound").placeholder("alarm…"),
        Field::boolean("critical", "Critical alerts when down")
            .help("Plays a sound even when the phone is muted."),
    ],
    transport: Transport::Http(build),
    check: None,
};

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let server = opt(config, "server").unwrap_or("https://api.day.app");
    let key = req(config, "device_key")?;
    let critical = severity(event.kind) == Severity::Critical;
    let level = match (critical, super::http::flag(config, "critical")) {
        (true, true) => "critical",
        (true, false) => "timeSensitive",
        _ => "active",
    };
    let mut body = json!({
        "device_key": key,
        "title": title(event),
        "body": body_text(event),
        "group": opt(config, "group").unwrap_or(product()),
        "level": level,
    });
    if let Some(url) = &event.url {
        body["url"] = json!(url);
    }
    if let Some(sound) = opt(config, "sound") {
        body["sound"] = json!(sound);
    }
    Ok(vec![OutRequest::post_json(join(server, "/push"), body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::EventKind;
    use crate::notify::tests::event;

    #[test]
    fn down_is_time_sensitive_or_critical() {
        let req = &build(&json!({"device_key": "k"}), &event(EventKind::Down)).unwrap()[0];
        assert_eq!(req.url, "https://api.day.app/push");
        assert_eq!(req.json().unwrap()["level"], "timeSensitive");
        let crit = &build(
            &json!({"device_key": "k", "critical": true}),
            &event(EventKind::Down),
        )
        .unwrap()[0];
        assert_eq!(crit.json().unwrap()["level"], "critical");
        let up = &build(
            &json!({"device_key": "k", "critical": true}),
            &event(EventKind::Up),
        )
        .unwrap()[0];
        assert_eq!(up.json().unwrap()["level"], "active");
    }
}
