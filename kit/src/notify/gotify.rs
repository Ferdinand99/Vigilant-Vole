//! Gotify `POST /message` with markdown and a click URL.

use serde_json::{Value, json};

use super::http::{OutRequest, join, num, req};
use super::message::{Severity, markdown_details, title};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::notify::alert::Alert;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "gotify",
    label: "Gotify",
    icon: "bell",
    category: Category::Push,
    description: "Self-hosted push server",
    docs_url: "https://gotify.net/docs/pushmsg",
    fields: &[
        Field::url("server", "Server")
            .required()
            .placeholder("https://gotify.example.com…"),
        Field::secret("app_token", "App token")
            .required()
            .help("Apps → Create application → copy the token."),
        Field::number("priority", "Priority")
            .placeholder("Automatic…")
            .help("0–10. Leave empty: 8 for down, 5 for up, 4 otherwise."),
    ],
    transport: Transport::Http(build),
    check: None,
};

pub fn build(config: &Value, event: &Alert) -> Result<Vec<OutRequest>, String> {
    let server = req(config, "server")?;
    let token = req(config, "app_token")?;
    let priority = num(config, "priority").unwrap_or(match event.level {
        Severity::Critical => 8,
        Severity::Ok => 5,
        _ => 4,
    });
    let mut extras = json!({ "client::display": { "contentType": "text/markdown" } });
    if let Some(url) = &event.url {
        extras["client::notification"] = json!({ "click": { "url": url } });
    }
    let body = json!({
        "title": title(event),
        "message": markdown_details(event),
        "priority": priority,
        "extras": extras,
    });
    Ok(vec![
        OutRequest::post_json(join(server, "/message"), body).header("X-Gotify-Key", token),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::AlertKind;
    use crate::notify::tests::event;

    #[test]
    fn posts_markdown_with_key_header() {
        let mut e = event(AlertKind::Problem);
        e.url = Some("https://vv.example/monitors/7".into());
        let req = &build(
            &json!({"server": "https://g.example/", "app_token": "AbC"}),
            &e,
        )
        .unwrap()[0];
        assert_eq!(req.url, "https://g.example/message");
        assert_eq!(req.header_value("X-Gotify-Key"), Some("AbC"));
        let body = req.json().unwrap();
        assert_eq!(body["priority"], 8);
        assert_eq!(body["title"], "\u{1f534} Payments API is down");
        assert_eq!(
            body["extras"]["client::notification"]["click"]["url"],
            "https://vv.example/monitors/7"
        );
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("[View monitor](https://vv.example/monitors/7)")
        );
    }
}
