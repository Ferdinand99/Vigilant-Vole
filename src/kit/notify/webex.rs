//! Webex messages API (bot token, markdown).

use serde_json::{Value, json};

use super::http::{OutRequest, opt, req};
use super::message::markdown_body;
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::kit::notify::alert::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "webex",
    label: "Webex",
    icon: "megaphone",
    category: Category::Chat,
    description: "Markdown messages from a Webex bot",
    docs_url: "https://developer.webex.com/docs/bots",
    fields: &[
        Field::secret("token", "Bot access token").required(),
        Field::text("room_id", "Space ID")
            .placeholder("Y2lzY29zcGFyazovL…")
            .help("The bot must be a member of the space…"),
        Field::text("person_email", "Person email")
            .placeholder("oncall@example.com…")
            .help("…or send a direct message instead."),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    if opt(config, "room_id").is_none() && opt(config, "person_email").is_none() {
        vec![field_error(
            "room_id",
            crate::kit::i18n::t("notif.err_webex_room"),
        )]
    } else {
        Vec::new()
    }
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let token = req(config, "token")?;
    let mut body = json!({ "markdown": markdown_body(event) });
    if let Some(room) = opt(config, "room_id") {
        body["roomId"] = json!(room);
    } else {
        body["toPersonEmail"] = json!(req(config, "person_email")?);
    }
    Ok(vec![
        OutRequest::post_json("https://webexapis.com/v1/messages", body).bearer(token),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::notify::alert::EventKind;
    use crate::kit::notify::tests::event;

    #[test]
    fn posts_markdown_to_room_or_person() {
        let room = &build(
            &json!({"token": "t", "room_id": "r"}),
            &event(EventKind::Down),
        )
        .unwrap()[0];
        assert_eq!(room.json().unwrap()["roomId"], "r");
        let dm = &build(
            &json!({"token": "t", "person_email": "a@b.c"}),
            &event(EventKind::Down),
        )
        .unwrap()[0];
        assert_eq!(dm.json().unwrap()["toPersonEmail"], "a@b.c");
        assert!(
            dm.json().unwrap()["markdown"]
                .as_str()
                .unwrap()
                .starts_with("**")
        );
    }
}
