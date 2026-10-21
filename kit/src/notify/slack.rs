//! Slack incoming webhook with Block Kit (inside a coloured attachment).

use serde_json::{Value, json};

use super::http::{OutRequest, opt, req};
use super::message::{color_hex, detail, facts, summary, title};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::notify::alert::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "slack",
    label: "Slack",
    icon: "megaphone",
    category: Category::Chat,
    description: "Block Kit messages via an incoming webhook",
    docs_url: "https://api.slack.com/messaging/webhooks",
    fields: &[
        Field::url("url", "Webhook URL")
            .required()
            .sensitive()
            .placeholder("https://hooks.slack.com/services/…")
            .help("Create a Slack app, enable incoming webhooks and pick a channel."),
        Field::text("channel", "Channel override")
            .placeholder("#alerts…")
            .help("Only works with legacy webhooks."),
        Field::text("username", "Bot name").placeholder("Vigilant Vole…"),
        Field::text("icon_emoji", "Icon emoji").placeholder(":rotating_light:…"),
    ],
    transport: Transport::Http(build),
    check: None,
};

/// Slack mrkdwn escaping.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    let mut text = format!("*{}*", esc(&title(event)));
    if let Some(d) = detail(event) {
        text.push('\n');
        text.push_str(&esc(&d));
    }
    let mut blocks = vec![json!({ "type": "section", "text": { "type": "mrkdwn", "text": text } })];
    let fields: Vec<Value> = facts(event)
        .into_iter()
        .filter(|(label, _)| *label != "Time")
        .map(|(label, value)| json!({ "type": "mrkdwn", "text": format!("*{label}*\n{}", esc(&value)) }))
        .collect();
    if !fields.is_empty() {
        blocks.push(json!({ "type": "section", "fields": fields }));
    }
    let ts = event.occurred_at.unix_timestamp();
    blocks.push(json!({ "type": "context", "elements": [
        { "type": "mrkdwn", "text": format!("{} · <!date^{ts}^{{date_short_pretty}} at {{time}}|{}>", super::message::product(), super::message::format_time(event.occurred_at)) }
    ]}));
    if let Some(url) = &event.url {
        blocks.push(json!({ "type": "actions", "elements": [
            { "type": "button", "text": { "type": "plain_text", "text": crate::i18n::t("msg.view_monitor") }, "url": url }
        ]}));
    }
    let mut body = json!({
        "text": summary(event),
        "attachments": [{ "color": color_hex(event.kind), "blocks": blocks }],
    });
    for key in ["channel", "username", "icon_emoji"] {
        if let Some(v) = opt(config, key) {
            body[key] = json!(v);
        }
    }
    Ok(vec![OutRequest::post_json(url, body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::EventKind;
    use crate::notify::tests::event;

    #[test]
    fn builds_blocks_with_fallback_text_and_button() {
        let mut e = event(EventKind::Down);
        e.message = Some("<timeout>".into());
        e.url = Some("https://vv.example/monitors/7".into());
        let reqs = build(
            &json!({"url": "https://hooks.slack.com/services/x", "channel": "#ops"}),
            &e,
        )
        .unwrap();
        let body = reqs[0].json().unwrap();
        assert_eq!(body["text"], "\u{1f534} Payments API is down — <timeout>");
        assert_eq!(body["channel"], "#ops");
        let att = &body["attachments"][0];
        assert_eq!(att["color"], "#FF3B30");
        assert_eq!(
            att["blocks"][0]["text"]["text"],
            "*\u{1f534} Payments API is down*\n&lt;timeout&gt;"
        );
        let last = att["blocks"].as_array().unwrap().last().unwrap();
        assert_eq!(last["elements"][0]["url"], "https://vv.example/monitors/7");
    }
}
