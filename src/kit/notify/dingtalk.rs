//! DingTalk custom robot (markdown) with optional "additional signature".

use base64::Engine as _;
use serde_json::{Value, json};

use super::http::{OutRequest, hmac_sha256, opt, req, urlencode};
use super::message::{markdown_body, title};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::kit::notify::alert::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "dingtalk",
    label: "DingTalk",
    icon: "megaphone",
    category: Category::Chat,
    description: "Markdown messages from a custom robot",
    docs_url: "https://open.dingtalk.com/document/robots/custom-robot-access",
    fields: &[
        Field::url("url", "Webhook URL")
            .required()
            .sensitive()
            .placeholder("https://oapi.dingtalk.com/robot/send?access_token=…"),
        Field::secret("secret", "Signing secret")
            .placeholder("SEC…")
            .help("If the robot uses “additional signature” security."),
    ],
    transport: Transport::Http(build),
    check: None,
};

/// `urlencode(base64(hmac_sha256(key = secret, msg = "{timestamp_ms}\n{secret}")))`.
pub fn sign(timestamp_ms: i64, secret: &str) -> String {
    let msg = format!("{timestamp_ms}\n{secret}");
    urlencode(
        &base64::engine::general_purpose::STANDARD
            .encode(hmac_sha256(secret.as_bytes(), msg.as_bytes())),
    )
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let mut url = req(config, "url")?.to_string();
    if let Some(secret) = opt(config, "secret") {
        let ts = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&format!("timestamp={ts}&sign={}", sign(ts, secret)));
    }
    let body = json!({ "msgtype": "markdown", "markdown": { "title": title(event), "text": markdown_body(event) } });
    Ok(vec![OutRequest::post_json(url, body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::notify::alert::EventKind;
    use crate::kit::notify::tests::event;

    #[test]
    fn appends_signature_to_url() {
        let req = &build(&json!({"url": "https://oapi.dingtalk.com/robot/send?access_token=a", "secret": "SECx"}), &event(EventKind::Down)).unwrap()[0];
        assert!(
            req.url
                .starts_with("https://oapi.dingtalk.com/robot/send?access_token=a&timestamp=")
        );
        assert!(req.url.contains("&sign="));
        assert_eq!(req.json().unwrap()["msgtype"], "markdown");
    }
}
