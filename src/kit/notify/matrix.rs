//! Matrix client-server API: `m.room.message` with an HTML formatted body.

use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::Method;
use serde_json::{Value, json};

use super::http::{OutBody, OutRequest, join, opt, path_segment, req};
use super::message::{detail, escape_html, facts, text_body, title};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::kit::notify::alert::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "matrix",
    label: "Matrix",
    icon: "megaphone",
    category: Category::Chat,
    description: "Element and any Matrix room",
    docs_url: "https://spec.matrix.org/latest/client-server-api/#put_matrixclientv3roomsroomidsendeventtypetxnid",
    fields: &[
        Field::url("homeserver", "Homeserver")
            .required()
            .placeholder("https://matrix.org…"),
        Field::text("room_id", "Room ID")
            .required()
            .placeholder("!abcdef:matrix.org…")
            .help("Room settings → Advanced. The bot must already be in the room."),
        Field::secret("access_token", "Access token").required(),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    match opt(config, "room_id") {
        Some(r) if !(r.starts_with('!') && r.contains(':')) => {
            vec![field_error(
                "room_id",
                crate::kit::i18n::t("notif.err_matrix_room"),
            )]
        }
        _ => Vec::new(),
    }
}

static TXN: AtomicU64 = AtomicU64::new(0);

/// Unique per process and event; Matrix de-duplicates retries by it.
fn txn_id(event: &NotificationEvent) -> String {
    format!(
        "vv{}-{}",
        event.occurred_at.unix_timestamp_nanos(),
        TXN.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn html(event: &NotificationEvent) -> String {
    let mut out = format!("<strong>{}</strong>", escape_html(&title(event)));
    if let Some(d) = detail(event) {
        out.push_str(&format!("<br>{}", escape_html(&d)));
    }
    let items: String = facts(event)
        .into_iter()
        .map(|(l, v)| format!("<li>{}: {}</li>", escape_html(l), escape_html(&v)))
        .collect();
    out.push_str(&format!("<ul>{items}</ul>"));
    if let Some(url) = &event.url {
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            escape_html(url),
            escape_html(crate::kit::i18n::t("msg.view_monitor"))
        ));
    }
    out
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let homeserver = req(config, "homeserver")?;
    let room = req(config, "room_id")?;
    let token = req(config, "access_token")?;
    let url = join(
        homeserver,
        &format!(
            "/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            path_segment(room),
            txn_id(event)
        ),
    );
    let body = json!({
        "msgtype": "m.text",
        "body": text_body(event),
        "format": "org.matrix.custom.html",
        "formatted_body": html(event),
    });
    Ok(vec![
        OutRequest {
            method: Method::PUT,
            url,
            headers: Vec::new(),
            body: OutBody::Json(body),
        }
        .bearer(token),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::notify::alert::EventKind;
    use crate::kit::notify::tests::event;

    #[test]
    fn puts_formatted_message_with_unique_txn() {
        let e = event(EventKind::Down);
        let config = json!({"homeserver": "https://hs.example", "room_id": "!r:hs.example", "access_token": "syt_x"});
        let a = &build(&config, &e).unwrap()[0];
        let b = &build(&config, &e).unwrap()[0];
        assert_eq!(a.method, Method::PUT);
        assert!(a.url.starts_with(
            "https://hs.example/_matrix/client/v3/rooms/%21r%3Ahs.example/send/m.room.message/vv"
        ));
        assert_ne!(a.url, b.url, "transaction ids differ");
        assert_eq!(a.header_value("authorization"), Some("Bearer syt_x"));
        let body = a.json().unwrap();
        assert_eq!(body["msgtype"], "m.text");
        assert!(
            body["formatted_body"]
                .as_str()
                .unwrap()
                .starts_with("<strong>")
        );
    }

    #[test]
    fn validates_room_id() {
        assert_eq!(check(&json!({"room_id": "#alias:hs"})).len(), 1);
    }
}
