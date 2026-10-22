//! Mattermost incoming webhook (Slack-style attachment with a colour bar).

use serde_json::{Value, json};

use super::http::{OutRequest, opt, req};
use super::message::{color_hex, detail, facts, link, summary, title};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::notify::alert::Alert;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "mattermost",
    label: "Mattermost",
    icon: "megaphone",
    category: Category::Chat,
    description: "Incoming webhook with colour-coded attachments",
    docs_url: "https://developers.mattermost.com/integrate/webhooks/incoming/",
    fields: &[
        Field::url("url", "Webhook URL")
            .required()
            .sensitive()
            .placeholder("https://chat.example.com/hooks/…"),
        Field::text("channel", "Channel override").placeholder("town-square…"),
        Field::text("username", "Bot name").placeholder("{product}…"),
        Field::url("icon_url", "Icon URL")
            .cosmetic()
            .placeholder("https://example.com/icon.png…"),
    ],
    transport: Transport::Http(build),
    check: None,
};

/// Attachment shared with Rocket.Chat (same Slack-legacy shape).
pub fn attachment(event: &Alert) -> Value {
    let fields: Vec<Value> = facts(event)
        .into_iter()
        .map(|(t, v)| json!({ "title": t, "value": v, "short": t != "Target" }))
        .collect();
    let mut att = json!({
        "fallback": summary(event),
        "color": color_hex(event),
        "title": title(event),
        "fields": fields,
    });
    if let Some(d) = detail(event) {
        att["text"] = json!(d);
    }
    if let Some(url) = link(event) {
        att["title_link"] = json!(url);
    }
    att
}

pub fn build(config: &Value, event: &Alert) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    let mut body = json!({ "attachments": [attachment(event)] });
    for key in ["channel", "username", "icon_url"] {
        if let Some(v) = opt(config, key) {
            body[key] = json!(v);
        }
    }
    Ok(vec![OutRequest::post_json(url, body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::AlertKind;
    use crate::notify::tests::event;

    #[test]
    fn sends_attachment_and_overrides() {
        let req = &build(
            &json!({"url": "https://mm/hooks/x", "username": "Vole"}),
            &event(AlertKind::Problem),
        )
        .unwrap()[0];
        let body = req.json().unwrap();
        assert_eq!(body["username"], "Vole");
        assert_eq!(body["attachments"][0]["color"], "#FF3B30");
        assert_eq!(
            body["attachments"][0]["title_link"],
            "https://pay.example/health"
        );
    }
}
