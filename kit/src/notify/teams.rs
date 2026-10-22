//! Microsoft Teams via a Power Automate "workflow" webhook posting an
//! Adaptive Card (the replacement for retired Office 365 connectors).

use serde_json::{Value, json};

use super::http::{OutRequest, req};
use super::message::{Severity, detail, facts, headline};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::notify::alert::Alert;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "teams",
    label: "Microsoft Teams",
    icon: "megaphone",
    category: Category::Chat,
    description: "Adaptive Cards via a workflow webhook",
    docs_url: "https://support.microsoft.com/en-us/office/create-incoming-webhooks-with-workflows-for-microsoft-teams-8ae491c7-0394-4861-ba59-055e33f75498",
    fields: &[Field::url("url", "Workflow URL")
        .required()
        .sensitive()
        .placeholder("https://prod-00.westus.logic.azure.com/workflows/…")
        .help("In Teams: channel → Workflows → “Post to a channel when a webhook request is received”.")],
    transport: Transport::Http(build),
    check: None,
};

pub fn card(event: &Alert) -> Value {
    let color = match event.level {
        Severity::Critical => "Attention",
        Severity::Warning => "Warning",
        Severity::Ok => "Good",
        Severity::Info => "Accent",
    };
    let mut body = vec![json!({
        "type": "TextBlock", "text": format!("{} {}", event.emoji, headline(event)),
        "weight": "Bolder", "size": "Medium", "color": color, "wrap": true
    })];
    if let Some(d) = detail(event) {
        body.push(json!({ "type": "TextBlock", "text": d, "wrap": true, "spacing": "Small" }));
    }
    let fact_list: Vec<Value> = facts(event)
        .into_iter()
        .map(|(t, v)| json!({ "title": t, "value": v }))
        .collect();
    body.push(json!({ "type": "FactSet", "facts": fact_list }));
    let mut card = json!({
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "type": "AdaptiveCard",
        "version": "1.4",
        "body": body,
        "msteams": { "width": "Full" },
    });
    if let Some(url) = &event.url {
        card["actions"] =
            json!([{ "type": "Action.OpenUrl", "title": event.link_label.as_str(), "url": url }]);
    }
    card
}

pub fn build(config: &Value, event: &Alert) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    let body = json!({
        "type": "message",
        "attachments": [{
            "contentType": "application/vnd.microsoft.card.adaptive",
            "contentUrl": null,
            "content": card(event),
        }],
    });
    Ok(vec![OutRequest::post_json(url, body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::AlertKind;
    use crate::notify::tests::event;

    #[test]
    fn wraps_adaptive_card_in_message() {
        let mut e = event(AlertKind::Recovered);
        e.url = Some("https://vv.example/monitors/7".into());
        let req = &build(&json!({"url": "https://flow.example/x"}), &e).unwrap()[0];
        let body = req.json().unwrap();
        assert_eq!(body["type"], "message");
        let card = &body["attachments"][0]["content"];
        assert_eq!(card["type"], "AdaptiveCard");
        assert_eq!(card["body"][0]["color"], "Good");
        assert_eq!(card["actions"][0]["url"], "https://vv.example/monitors/7");
    }
}
