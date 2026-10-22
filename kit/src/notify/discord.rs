//! Discord webhook with a rich, colour-coded embed.

use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use super::http::{OutRequest, opt, req};
use super::message::{detail, facts, headline, link};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::notify::alert::Alert;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "discord",
    label: "Discord",
    icon: "megaphone",
    category: Category::Chat,
    description: "Rich embeds in any channel",
    docs_url: "https://support.discord.com/hc/en-us/articles/228383668-Intro-to-Webhooks",
    fields: &[
        Field::url("url", "Webhook URL")
            .required()
            .sensitive()
            .placeholder("https://discord.com/api/webhooks/…")
            .help("Channel settings → Integrations → Webhooks → Copy webhook URL."),
        Field::text("username", "Bot name")
            .placeholder("{product}…")
            .help("Overrides the webhook’s name."),
        Field::url("avatar_url", "Avatar URL")
            .cosmetic()
            .placeholder("https://example.com/avatar.png…"),
        Field::text("thread_id", "Thread ID")
            .placeholder("123456789012345678…")
            .help("Post into a thread or forum post."),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    match opt(config, "url") {
        Some(u) if !u.contains("/api/webhooks/") => {
            vec![field_error("url", crate::i18n::t("notif.err_discord"))]
        }
        _ => Vec::new(),
    }
}

pub fn build(config: &Value, event: &Alert) -> Result<Vec<OutRequest>, String> {
    let mut url = req(config, "url")?.to_string();
    if let Some(thread) = opt(config, "thread_id") {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&format!("thread_id={}", super::http::urlencode(thread)));
    }
    let mut body = json!({ "embeds": [embed(event)] });
    if let Some(name) = opt(config, "username") {
        body["username"] = json!(name);
    }
    if let Some(avatar) = opt(config, "avatar_url") {
        body["avatar_url"] = json!(avatar);
    }
    Ok(vec![OutRequest::post_json(url, body)])
}

/// Title = emoji + headline (links to the monitor), description = detail,
/// inline fields for target/type/downtime.
fn embed(event: &Alert) -> Value {
    let fields: Vec<Value> = facts(event)
        .into_iter()
        .filter(|(label, _)| *label != "Time")
        .map(|(label, value)| json!({ "name": label, "value": truncate(&value, 1000), "inline": label != "Target" }))
        .collect();
    let mut embed = json!({
        "title": truncate(&format!("{} {}", event.emoji, headline(event)), 250),
        "color": event.colour,
        "fields": fields,
        "footer": { "text": super::message::product() },
        "timestamp": event.occurred_at.format(&Rfc3339).unwrap_or_default(),
    });
    if let Some(link) = link(event) {
        embed["url"] = json!(link);
    }
    if let Some(d) = detail(event) {
        embed["description"] = json!(truncate(&d, 4000));
    }
    embed
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::AlertKind;
    use crate::notify::tests::alert;

    #[test]
    fn builds_embed_with_overrides() {
        let mut e = alert(AlertKind::Problem, Some("HTTP 503"), None);
        e.url = Some("https://vv.example/monitors/7".into());
        let config = json!({"url": "https://discord.com/api/webhooks/1/abc", "username": "Vole", "avatar_url": "https://a/x.png", "thread_id": "99"});
        let reqs = build(&config, &e).unwrap();
        assert_eq!(
            reqs[0].url,
            "https://discord.com/api/webhooks/1/abc?thread_id=99"
        );
        let body = reqs[0].json().unwrap();
        assert_eq!(body["username"], "Vole");
        assert_eq!(body["avatar_url"], "https://a/x.png");
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"], "\u{1f534} Payments API is down");
        assert_eq!(embed["description"], "HTTP 503");
        assert_eq!(embed["color"], 0xFF3B30);
        assert_eq!(embed["url"], "https://vv.example/monitors/7");
        assert_eq!(embed["fields"][0]["name"], "Target");
    }

    #[test]
    fn rejects_non_discord_urls() {
        assert_eq!(check(&json!({"url": "https://example.com/hook"})).len(), 1);
        assert!(check(&json!({"url": "https://discord.com/api/webhooks/1/x"})).is_empty());
    }
}
