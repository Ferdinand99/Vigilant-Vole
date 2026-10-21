//! WhatsApp through self-hosted gateways: WAHA and Evolution API.

use serde_json::{Value, json};

use super::http::{OutRequest, join, opt, path_segment, req};
use super::message::{link, summary};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::notify::alert::NotificationEvent;

pub const WAHA: ProviderSchema = ProviderSchema {
    kind: "waha",
    label: "WhatsApp (WAHA)",
    icon: "send",
    category: Category::Messaging,
    description: "WhatsApp messages through your WAHA server",
    docs_url: "https://waha.devlike.pro/docs/how-to/send-messages/",
    fields: &[
        Field::url("server", "WAHA URL")
            .required()
            .placeholder("http://waha:3000…"),
        Field::text("session", "Session").default("default"),
        Field::text("chat_id", "Chat")
            .required()
            .placeholder("4712345678@c.us…")
            .help("A phone number with country code, or a chat ID ending in @c.us or @g.us."),
        Field::secret("api_key", "API key").help("Only if WAHA runs with an API key."),
    ],
    transport: Transport::Http(build_waha),
    check: None,
};

pub const EVOLUTION: ProviderSchema = ProviderSchema {
    kind: "evolution",
    label: "WhatsApp (Evolution API)",
    icon: "send",
    category: Category::Messaging,
    description: "WhatsApp messages through your Evolution API server",
    docs_url: "https://doc.evolution-api.com/v2/api-reference/message-controller/send-text",
    fields: &[
        Field::url("server", "Evolution API URL")
            .required()
            .placeholder("https://evolution.example.com…"),
        Field::text("instance", "Instance")
            .required()
            .placeholder("vigilant…"),
        Field::secret("api_key", "API key").required(),
        Field::text("number", "Recipient")
            .required()
            .placeholder("4712345678…")
            .help("A phone number with country code, or a group ID."),
    ],
    transport: Transport::Http(build_evolution),
    check: Some(check_evolution),
};

/// A bare phone number becomes a WAHA chat ID; IDs with `@` stay as typed.
fn waha_chat_id(raw: &str) -> String {
    if raw.contains('@') {
        raw.to_string()
    } else {
        let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
        format!("{digits}@c.us")
    }
}

fn message(event: &NotificationEvent) -> String {
    let mut text = summary(event);
    if let Some(url) = link(event) {
        text.push('\n');
        text.push_str(&url);
    }
    text
}

fn check_evolution(config: &Value) -> Vec<FieldError> {
    match opt(config, "number") {
        Some(n) if !n.contains('@') && n.chars().filter(char::is_ascii_digit).count() < 7 => {
            vec![field_error(
                "number",
                crate::i18n::t("notif.err_phone_number"),
            )]
        }
        _ => Vec::new(),
    }
}

pub fn build_waha(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let server = req(config, "server")?;
    let chat = req(config, "chat_id")?;
    let session = opt(config, "session").unwrap_or("default");
    let mut request = OutRequest::post_json(
        join(server, "/api/sendText"),
        json!({"session": session, "chatId": waha_chat_id(chat), "text": message(event)}),
    );
    if let Some(key) = opt(config, "api_key") {
        request = request.header("X-Api-Key", key);
    }
    Ok(vec![request])
}

pub fn build_evolution(
    config: &Value,
    event: &NotificationEvent,
) -> Result<Vec<OutRequest>, String> {
    let server = req(config, "server")?;
    let instance = req(config, "instance")?;
    let key = req(config, "api_key")?;
    let number = req(config, "number")?;
    let number = if number.contains('@') {
        number.to_string()
    } else {
        number.chars().filter(char::is_ascii_digit).collect()
    };
    Ok(vec![
        OutRequest::post_json(
            join(
                server,
                &format!("/message/sendText/{}", path_segment(instance)),
            ),
            json!({"number": number, "text": message(event)}),
        )
        .header("apikey", key),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::EventKind;
    use crate::notify::tests::event;

    #[test]
    fn waha_posts_to_send_text_with_a_chat_id() {
        let mut e = event(EventKind::Down);
        e.url = Some("https://vv.example/monitors/7".into());
        let r = &build_waha(
            &json!({"server": "http://waha:3000/", "chat_id": "+47 123 45 678", "api_key": "K"}),
            &e,
        )
        .unwrap()[0];
        assert_eq!(r.url, "http://waha:3000/api/sendText");
        assert_eq!(r.header_value("X-Api-Key"), Some("K"));
        let body = r.json().unwrap();
        assert_eq!(
            (body["session"].as_str(), body["chatId"].as_str()),
            (Some("default"), Some("4712345678@c.us"))
        );
        assert!(
            body["text"]
                .as_str()
                .unwrap()
                .contains("Payments API is down")
                && body["text"].as_str().unwrap().ends_with("monitors/7")
        );
        let group = &build_waha(
            &json!({"server": "http://waha:3000", "chat_id": "123@g.us", "session": "work"}),
            &e,
        )
        .unwrap()[0];
        assert_eq!(group.json().unwrap()["chatId"], "123@g.us");
        assert_eq!(group.json().unwrap()["session"], "work");
        assert!(group.header_value("X-Api-Key").is_none());
    }

    #[test]
    fn evolution_posts_to_the_instance_with_an_api_key() {
        let r = &build_evolution(
            &json!({"server": "https://evo.example/", "instance": "my inst", "api_key": "K", "number": "+47 123 45 678"}),
            &event(EventKind::Up),
        )
        .unwrap()[0];
        assert_eq!(r.url, "https://evo.example/message/sendText/my%20inst");
        assert_eq!(r.header_value("apikey"), Some("K"));
        assert_eq!(r.json().unwrap()["number"], "4712345678");
        assert!(
            check_evolution(&json!({"number": "12"}))
                .iter()
                .any(|(k, _)| k == "number")
        );
        assert!(check_evolution(&json!({"number": "123456789"})).is_empty());
        assert!(check_evolution(&json!({"number": "123@g.us"})).is_empty());
    }
}
