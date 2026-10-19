//! Google Chat space webhook with a Cards v2 card.

use serde_json::{Value, json};

use super::http::{OutRequest, opt, req};
use super::message::{PRODUCT, detail, escape_html, facts, title};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::db::models::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "googlechat",
    label: "Google Chat",
    icon: "megaphone",
    category: Category::Chat,
    description: "Cards in a Google Chat space",
    docs_url: "https://developers.google.com/workspace/chat/quickstart/webhooks",
    fields: &[Field::url("url", "Webhook URL")
        .required()
        .sensitive()
        .placeholder("https://chat.googleapis.com/v1/spaces/…")
        .help("Space → Apps & integrations → Webhooks.")],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    match opt(config, "url") {
        Some(u) if !u.starts_with("https://chat.googleapis.com/") => {
            vec![field_error(
                "url",
                crate::i18n::t("notif.err_googlechat"),
            )]
        }
        _ => Vec::new(),
    }
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    let mut widgets: Vec<Value> = Vec::new();
    if let Some(d) = detail(event) {
        widgets.push(json!({ "textParagraph": { "text": escape_html(&d) } }));
    }
    for (label, value) in facts(event) {
        widgets.push(json!({ "decoratedText": { "topLabel": label, "text": escape_html(&value), "wrapText": true } }));
    }
    if let Some(link) = &event.url {
        widgets.push(json!({ "buttonList": { "buttons": [
            { "text": crate::i18n::t("msg.view_monitor"), "onClick": { "openLink": { "url": link } } }
        ]}}));
    }
    let body = json!({
        "cardsV2": [{
            "cardId": format!("vv-{}", event.kind.as_str()),
            "card": {
                "header": { "title": title(event), "subtitle": PRODUCT },
                "sections": [{ "widgets": widgets }],
            },
        }],
    });
    Ok(vec![OutRequest::post_json(url, body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::EventKind;
    use crate::notification::tests::event;

    #[test]
    fn builds_cards_v2() {
        let mut e = event(EventKind::Down);
        e.url = Some("https://vv.example/monitors/7".into());
        let req = &build(
            &json!({"url": "https://chat.googleapis.com/v1/spaces/x"}),
            &e,
        )
        .unwrap()[0];
        let card = &req.json().unwrap()["cardsV2"][0]["card"];
        assert_eq!(card["header"]["title"], "\u{1f534} Payments API is down");
        let widgets = card["sections"][0]["widgets"].as_array().unwrap();
        assert_eq!(
            widgets.last().unwrap()["buttonList"]["buttons"][0]["onClick"]["openLink"]["url"],
            "https://vv.example/monitors/7"
        );
    }
}
