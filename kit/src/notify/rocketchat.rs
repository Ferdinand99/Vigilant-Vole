//! Rocket.Chat incoming webhook.

use serde_json::{Value, json};

use super::http::{OutRequest, opt, req};
use super::mattermost::attachment;
use super::message::summary;
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::notify::alert::Alert;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "rocketchat",
    label: "Rocket.Chat",
    icon: "megaphone",
    category: Category::Chat,
    description: "Incoming webhook integration",
    docs_url: "https://docs.rocket.chat/docs/integrations",
    fields: &[
        Field::url("url", "Webhook URL")
            .required()
            .sensitive()
            .placeholder("https://chat.example.com/hooks/…"),
        Field::text("channel", "Channel override").placeholder("#alerts…"),
        Field::text("username", "Bot name").placeholder("Vigilant Vole…"),
        Field::text("emoji", "Avatar emoji").placeholder(":rotating_light:…"),
    ],
    transport: Transport::Http(build),
    check: None,
};

pub fn build(config: &Value, event: &Alert) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    let mut body = json!({ "text": summary(event), "attachments": [attachment(event)] });
    if let Some(v) = opt(config, "channel") {
        body["channel"] = json!(v);
    }
    if let Some(v) = opt(config, "username") {
        body["alias"] = json!(v);
    }
    if let Some(v) = opt(config, "emoji") {
        body["emoji"] = json!(v);
    }
    Ok(vec![OutRequest::post_json(url, body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::AlertKind;
    use crate::notify::tests::event;

    #[test]
    fn uses_alias_for_username() {
        let req = &build(
            &json!({"url": "https://rc/hooks/x", "username": "Vole"}),
            &event(AlertKind::Recovered),
        )
        .unwrap()[0];
        let body = req.json().unwrap();
        assert_eq!(body["alias"], "Vole");
        assert_eq!(body["attachments"][0]["color"], "#34C759");
    }
}
