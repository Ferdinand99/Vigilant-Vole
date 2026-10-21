//! Telegram bot `sendMessage` with HTML formatting.

use serde_json::{Value, json};

use super::http::{OutRequest, flag, join, num, opt, req};
use super::message::{detail, escape_html, facts, title};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::kit::notify::alert::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "telegram",
    label: "Telegram",
    icon: "send",
    category: Category::Messaging,
    description: "Messages from your own bot",
    docs_url: "https://core.telegram.org/bots#how-do-i-create-a-bot",
    fields: &[
        Field::secret("bot_token", "Bot token")
            .required()
            .placeholder("123456:ABC-DEF…")
            .help("Create a bot with @BotFather."),
        Field::text("chat_id", "Chat ID")
            .required()
            .placeholder("-1001234567890…")
            .help("A user, group or channel ID. Message @userinfobot to find yours."),
        Field::number("message_thread_id", "Topic ID")
            .placeholder("42…")
            .help("For forum groups: the topic to post in."),
        Field::boolean("silent", "Send silently").help("Deliver without a sound."),
        Field::url("server_url", "Bot API server")
            .placeholder("https://api.telegram.org…")
            .help("Only for a self-hosted Bot API server."),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    match opt(config, "bot_token") {
        Some(t) if !t.contains(':') => vec![field_error(
            "bot_token",
            crate::kit::i18n::t("notif.err_telegram_token"),
        )],
        _ => Vec::new(),
    }
}

/// HTML message: bold title, detail, facts, link.
pub fn html(event: &NotificationEvent) -> String {
    let mut out = format!("<b>{}</b>", escape_html(&title(event)));
    if let Some(d) = detail(event) {
        out.push('\n');
        out.push_str(&escape_html(&d));
    }
    for (label, value) in facts(event) {
        out.push_str(&format!("\n<i>{label}:</i> {}", escape_html(&value)));
    }
    if let Some(url) = &event.url {
        out.push_str(&format!(
            "\n\n<a href=\"{}\">{}</a>",
            escape_html(url),
            escape_html(crate::kit::i18n::t("msg.view_monitor"))
        ));
    }
    out
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let token = req(config, "bot_token")?;
    let chat_id = req(config, "chat_id")?;
    let server = opt(config, "server_url").unwrap_or("https://api.telegram.org");
    let url = join(server, &format!("/bot{token}/sendMessage"));
    let mut body = json!({
        "chat_id": chat_id,
        "text": html(event),
        "parse_mode": "HTML",
        "link_preview_options": { "is_disabled": true },
        "disable_notification": flag(config, "silent"),
    });
    if let Some(thread) = num(config, "message_thread_id") {
        body["message_thread_id"] = json!(thread);
    }
    Ok(vec![OutRequest::post_json(url, body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::notify::alert::EventKind;
    use crate::kit::notify::tests::event;

    #[test]
    fn builds_html_message_with_topic_and_silence() {
        let mut e = event(EventKind::Down);
        e.message = Some("<503>".into());
        e.url = Some("https://vv.example/monitors/7".into());
        let config = json!({"bot_token": "1:abc", "chat_id": "-100", "message_thread_id": 5, "silent": true});
        let reqs = build(&config, &e).unwrap();
        assert_eq!(reqs[0].url, "https://api.telegram.org/bot1:abc/sendMessage");
        let body = reqs[0].json().unwrap();
        assert_eq!(body["parse_mode"], "HTML");
        assert_eq!(body["message_thread_id"], 5);
        assert_eq!(body["disable_notification"], true);
        let text = body["text"].as_str().unwrap();
        assert!(text.starts_with("<b>\u{1f534} Payments API is down</b>\n&lt;503&gt;"));
        assert!(text.ends_with("<a href=\"https://vv.example/monitors/7\">View monitor</a>"));
    }

    #[test]
    fn validates_token_shape() {
        assert_eq!(check(&json!({"bot_token": "nocolon"})).len(), 1);
    }
}
