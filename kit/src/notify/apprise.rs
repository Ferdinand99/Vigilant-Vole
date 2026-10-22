//! Apprise API (caronc/apprise-api): one integration, 90+ services.

use serde_json::{Value, json};

use super::http::{OutRequest, join, opt, path_segment, req};
use super::message::{Severity, body_text, title};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::notify::alert::Alert;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "apprise",
    label: "Apprise",
    icon: "plug",
    category: Category::Automation,
    description: "90+ services through an Apprise API server",
    docs_url: "https://github.com/caronc/apprise-api",
    fields: &[
        Field::url("url", "Apprise API URL")
            .required()
            .placeholder("http://apprise:8000…"),
        Field::text("config_key", "Config key")
            .placeholder("vigilant…")
            .help("Use a configuration saved on the server…"),
        Field::textarea("urls", "Apprise URLs")
            .sensitive()
            .placeholder("tgram://bottoken/ChatID, mailto://user:pass@gmail.com…")
            .help("…or list notification URLs here, separated by commas or new lines."),
        Field::text("tag", "Tag")
            .placeholder("devops…")
            .help("Only notify services with this tag (config key mode)."),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    if opt(config, "config_key").is_none() && opt(config, "urls").is_none() {
        vec![field_error(
            "config_key",
            crate::i18n::t("notif.err_apprise"),
        )]
    } else {
        Vec::new()
    }
}

pub fn build(config: &Value, event: &Alert) -> Result<Vec<OutRequest>, String> {
    let server = req(config, "url")?;
    let kind = match event.level {
        Severity::Critical => "failure",
        Severity::Warning => "warning",
        Severity::Ok => "success",
        Severity::Info => "info",
    };
    let mut body = json!({ "title": title(event), "body": body_text(event), "type": kind });
    let url = if let Some(key) = opt(config, "config_key") {
        if let Some(tag) = opt(config, "tag") {
            body["tag"] = json!(tag);
        }
        join(server, &format!("/notify/{}", path_segment(key)))
    } else {
        let urls = req(config, "urls")?
            .split([',', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(",");
        body["urls"] = json!(urls);
        join(server, "/notify")
    };
    Ok(vec![OutRequest::post_json(url, body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::AlertKind;
    use crate::notify::tests::event;

    #[test]
    fn stateless_and_keyed_modes() {
        let e = event(AlertKind::Problem);
        let stateless = &build(
            &json!({"url": "http://a:8000", "urls": "tgram://x/y\nmailto://z"}),
            &e,
        )
        .unwrap()[0];
        assert_eq!(stateless.url, "http://a:8000/notify");
        assert_eq!(stateless.json().unwrap()["urls"], "tgram://x/y,mailto://z");
        assert_eq!(stateless.json().unwrap()["type"], "failure");
        let keyed = &build(
            &json!({"url": "http://a:8000/", "config_key": "vv", "tag": "ops"}),
            &e,
        )
        .unwrap()[0];
        assert_eq!(keyed.url, "http://a:8000/notify/vv");
        assert_eq!(keyed.json().unwrap()["tag"], "ops");
        assert_eq!(check(&json!({"url": "http://a"})).len(), 1);
    }
}
