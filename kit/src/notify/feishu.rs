//! Feishu / Lark custom bot with an interactive card and optional signing.

use base64::Engine as _;
use serde_json::{Value, json};

use super::http::{OutRequest, hmac_sha256, opt, req};
use super::message::{Severity, markdown_details, title};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::notify::alert::Alert;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "feishu",
    label: "Feishu / Lark",
    icon: "megaphone",
    category: Category::Chat,
    description: "Interactive cards from a custom bot",
    docs_url: "https://open.feishu.cn/document/client-docs/bot-v3/add-custom-bot",
    fields: &[
        Field::url("url", "Webhook URL")
            .required()
            .sensitive()
            .placeholder("https://open.feishu.cn/open-apis/bot/v2/hook/…"),
        Field::secret("secret", "Signing secret").help("If the bot has signature verification on."),
    ],
    transport: Transport::Http(build),
    check: None,
};

/// `base64(hmac_sha256(key = "{timestamp}\n{secret}", msg = ""))`.
pub fn sign(timestamp: i64, secret: &str) -> String {
    let key = format!("{timestamp}\n{secret}");
    base64::engine::general_purpose::STANDARD.encode(hmac_sha256(key.as_bytes(), b""))
}

pub fn build(config: &Value, event: &Alert) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    let template = match event.level {
        Severity::Critical => "red",
        Severity::Warning => "orange",
        Severity::Ok => "green",
        Severity::Info => "blue",
    };
    let mut elements = vec![
        json!({ "tag": "div", "text": { "tag": "lark_md", "content": markdown_details(event) } }),
    ];
    if let Some(link) = &event.url {
        elements.push(json!({ "tag": "action", "actions": [{
            "tag": "button", "text": { "tag": "plain_text", "content": event.link_label.as_str() }, "url": link, "type": "primary"
        }]}));
    }
    let mut body = json!({
        "msg_type": "interactive",
        "card": {
            "config": { "wide_screen_mode": true },
            "header": { "title": { "tag": "plain_text", "content": title(event) }, "template": template },
            "elements": elements,
        },
    });
    if let Some(secret) = opt(config, "secret") {
        let ts = time::OffsetDateTime::now_utc().unix_timestamp();
        body["timestamp"] = json!(ts.to_string());
        body["sign"] = json!(sign(ts, secret));
    }
    Ok(vec![OutRequest::post_json(url, body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::AlertKind;
    use crate::notify::tests::event;

    #[test]
    fn card_with_template_colour_and_signature() {
        let req = &build(
            &json!({"url": "https://open.feishu.cn/x", "secret": "s"}),
            &event(AlertKind::Problem),
        )
        .unwrap()[0];
        let body = req.json().unwrap();
        assert_eq!(body["card"]["header"]["template"], "red");
        let ts: i64 = body["timestamp"].as_str().unwrap().parse().unwrap();
        assert_eq!(body["sign"], sign(ts, "s"));
    }

    #[test]
    fn signature_is_hmac_of_empty_message() {
        // key = "1700000000\nsecret", message = "" (Feishu's documented scheme)
        let expected = base64::engine::general_purpose::STANDARD
            .encode(hmac_sha256(b"1700000000\nsecret", b""));
        assert_eq!(sign(1_700_000_000, "secret"), expected);
    }
}
