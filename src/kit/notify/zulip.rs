//! Zulip: a stream message through the REST API, authenticated as a bot.

use serde_json::Value;

use super::http::{OutRequest, join, opt, req};
use super::message::{markdown_details, title};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::kit::notify::alert::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "zulip",
    label: "Zulip",
    icon: "megaphone",
    category: Category::Chat,
    description: "Messages in a Zulip stream",
    docs_url: "https://zulip.com/api/send-message",
    fields: &[
        Field::url("site", "Zulip URL")
            .required()
            .placeholder("https://yourorg.zulipchat.com…"),
        Field::text("bot_email", "Bot email")
            .required()
            .placeholder("vigilant-bot@yourorg.zulipchat.com…")
            .help("Settings → Bots → add a Generic bot."),
        Field::secret("api_key", "Bot API key").required(),
        Field::text("stream", "Stream")
            .required()
            .placeholder("monitoring…"),
        Field::text("topic", "Topic")
            .default("Vigilant Vole")
            .help("Topics keep one subject together inside a stream."),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    match opt(config, "bot_email") {
        Some(e) if !e.contains('@') => vec![field_error(
            "bot_email",
            crate::kit::i18n::t("notif.err_email_address"),
        )],
        _ => Vec::new(),
    }
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let site = req(config, "site")?;
    let email = req(config, "bot_email")?;
    let key = req(config, "api_key")?;
    let stream = req(config, "stream")?.trim_start_matches('#');
    let topic = opt(config, "topic").unwrap_or(super::message::product());
    let content = format!("**{}**\n{}", title(event), markdown_details(event));
    Ok(vec![
        OutRequest::post_form(
            join(site, "/api/v1/messages"),
            vec![
                ("type".into(), "stream".into()),
                ("to".into(), stream.into()),
                ("topic".into(), topic.into()),
                ("content".into(), content),
            ],
        )
        .basic_auth(email, key),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::notify::alert::EventKind;
    use crate::kit::notify::http::OutBody;
    use crate::kit::notify::tests::event;
    use serde_json::json;

    #[test]
    fn posts_a_markdown_stream_message_as_the_bot() {
        let mut e = event(EventKind::Down);
        e.url = Some("https://vv.example/monitors/7".into());
        let reqs = build(
            &json!({"site": "https://org.zulipchat.com/", "bot_email": "bot@org.zulipchat.com", "api_key": "KEY", "stream": "#ops"}),
            &e,
        )
        .unwrap();
        assert_eq!(reqs[0].url, "https://org.zulipchat.com/api/v1/messages");
        assert!(
            reqs[0]
                .header_value("Authorization")
                .unwrap()
                .starts_with("Basic ")
        );
        let OutBody::Form(fields) = &reqs[0].body else {
            panic!("form expected")
        };
        let get = |k: &str| {
            fields
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert_eq!(
            (get("type"), get("to"), get("topic")),
            ("stream", "ops", "Vigilant Vole")
        );
        assert!(
            get("content").contains("Payments API is down")
                && get("content").contains("[View monitor](https://vv.example/monitors/7)")
        );
    }

    #[test]
    fn the_bot_email_needs_an_at_sign() {
        assert!(
            check(&json!({"bot_email": "bot"}))
                .iter()
                .any(|(k, _)| k == "bot_email")
        );
        assert!(check(&json!({"bot_email": "bot@x.example"})).is_empty());
    }
}
