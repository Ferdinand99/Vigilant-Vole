//! Pushbullet pushes (a link push when there is a deep link).

use serde_json::{Value, json};

use super::http::{OutRequest, opt, req};
use super::message::{body_text, title};
use super::schema::{Category, Field, ProviderSchema, Transport};
use crate::db::models::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "pushbullet",
    label: "Pushbullet",
    icon: "bell",
    category: Category::Push,
    description: "Pushes to your phone and browser",
    docs_url: "https://docs.pushbullet.com/#create-push",
    fields: &[
        Field::secret("access_token", "Access token")
            .required()
            .help("Settings → Account → Create access token."),
        Field::text("device_iden", "Device ID").help("Leave empty to push to all devices."),
        Field::text("channel_tag", "Channel tag").help("Push to a channel’s subscribers instead."),
    ],
    transport: Transport::Http(build),
    check: None,
};

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let token = req(config, "access_token")?;
    let mut body = json!({ "type": "note", "title": title(event), "body": body_text(event) });
    if let Some(url) = &event.url {
        body["type"] = json!("link");
        body["url"] = json!(url);
    }
    if let Some(device) = opt(config, "device_iden") {
        body["device_iden"] = json!(device);
    } else if let Some(tag) = opt(config, "channel_tag") {
        body["channel_tag"] = json!(tag);
    }
    Ok(vec![
        OutRequest::post_json("https://api.pushbullet.com/v2/pushes", body)
            .header("Access-Token", token),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::EventKind;
    use crate::notification::tests::event;

    #[test]
    fn link_push_when_deep_link_exists() {
        let mut e = event(EventKind::Down);
        e.url = Some("https://vv.example/monitors/7".into());
        let req = &build(&json!({"access_token": "o.x", "device_iden": "d1"}), &e).unwrap()[0];
        assert_eq!(req.header_value("Access-Token"), Some("o.x"));
        let body = req.json().unwrap();
        assert_eq!(body["type"], "link");
        assert_eq!(body["device_iden"], "d1");
        let note = &build(&json!({"access_token": "o.x"}), &event(EventKind::Down)).unwrap()[0];
        assert_eq!(note.json().unwrap()["type"], "note");
    }
}
