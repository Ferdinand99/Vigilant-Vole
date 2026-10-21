//! Home Assistant `notify.*` service call (e.g. the mobile app).

use serde_json::{Value, json};

use super::http::{OutRequest, join, opt, path_segment, req};
use super::message::{body_text, title};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::notify::alert::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "homeassistant",
    label: "Home Assistant",
    icon: "server",
    category: Category::Automation,
    description: "Any notify service, like the mobile app",
    docs_url: "https://www.home-assistant.io/integrations/notify/",
    fields: &[
        Field::url("url", "Home Assistant URL")
            .required()
            .placeholder("http://homeassistant.local:8123…"),
        Field::secret("token", "Long-lived access token")
            .required()
            .help("Profile → Security → Long-lived access tokens."),
        Field::text("service", "Notify service")
            .default("notify")
            .placeholder("mobile_app_iphone…")
            .help("The part after “notify.”, e.g. mobile_app_iphone."),
    ],
    transport: Transport::Http(build),
    check: None,
};

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let base = req(config, "url")?;
    let token = req(config, "token")?;
    let service = opt(config, "service").unwrap_or("notify");
    let service = service.strip_prefix("notify.").unwrap_or(service);
    let mut body = json!({ "title": title(event), "message": body_text(event) });
    if let Some(url) = &event.url {
        // iOS uses `url`, Android `clickAction`.
        body["data"] = json!({ "url": url, "clickAction": url });
    }
    Ok(vec![
        OutRequest::post_json(
            join(
                base,
                &format!("/api/services/notify/{}", path_segment(service)),
            ),
            body,
        )
        .bearer(token),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::EventKind;
    use crate::notify::tests::event;

    #[test]
    fn calls_named_service() {
        let config =
            json!({"url": "http://ha:8123/", "token": "t", "service": "notify.mobile_app_x"});
        let req = &build(&config, &event(EventKind::Down)).unwrap()[0];
        assert_eq!(req.url, "http://ha:8123/api/services/notify/mobile_app_x");
        assert_eq!(req.header_value("authorization"), Some("Bearer t"));
    }
}
