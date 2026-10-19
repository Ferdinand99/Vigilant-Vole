//! Signal via a signal-cli-rest-api bridge (`POST /v2/send`).

use serde_json::{Value, json};

use super::http::{OutRequest, join, list, req};
use super::message::text_body;
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::db::models::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "signal",
    label: "Signal",
    icon: "send",
    category: Category::Messaging,
    description: "Through a signal-cli REST API bridge",
    docs_url: "https://github.com/bbernhard/signal-cli-rest-api",
    fields: &[
        Field::url("url", "API URL")
            .required()
            .placeholder("http://signal-api:8080…"),
        Field::text("number", "Sender number")
            .required()
            .placeholder("+4712345678…")
            .help("The number registered with signal-cli."),
        Field::text("recipients", "Recipients")
            .required()
            .placeholder("+4787654321, group.abc…")
            .help("Phone numbers or group IDs, separated by commas."),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    match super::http::opt(config, "number") {
        Some(n) if !n.starts_with('+') => vec![field_error(
            "number",
            crate::i18n::t("notif.err_intl_number"),
        )],
        _ => Vec::new(),
    }
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    let number = req(config, "number")?;
    let recipients = list(config, "recipients");
    if recipients.is_empty() {
        return Err("missing 'recipients' in channel config".into());
    }
    let body = json!({ "message": text_body(event), "number": number, "recipients": recipients });
    Ok(vec![OutRequest::post_json(join(url, "/v2/send"), body)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::EventKind;
    use crate::notification::tests::event;

    #[test]
    fn sends_to_every_recipient() {
        let config = json!({"url": "http://sig:8080/", "number": "+1", "recipients": "+2, +3"});
        let req = &build(&config, &event(EventKind::Down)).unwrap()[0];
        assert_eq!(req.url, "http://sig:8080/v2/send");
        assert_eq!(req.json().unwrap()["recipients"], json!(["+2", "+3"]));
    }
}
