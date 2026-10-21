//! SMS via Twilio's Messages API (one request per recipient).

use serde_json::Value;

use super::http::{OutRequest, list, opt, path_segment, req};
use super::message::summary;
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::notify::alert::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "twilio",
    label: "SMS (Twilio)",
    icon: "send",
    category: Category::Messaging,
    description: "Text messages to any phone",
    docs_url: "https://www.twilio.com/docs/messaging/api/message-resource#create-a-message-resource",
    fields: &[
        Field::text("account_sid", "Account SID")
            .destination()
            .required()
            .placeholder("AC…"),
        Field::secret("auth_token", "Auth token").required(),
        Field::text("from", "From")
            .required()
            .placeholder("+15005550006…")
            .help("A Twilio number or messaging service SID (MG…)."),
        Field::text("to", "To")
            .required()
            .placeholder("+4712345678…")
            .help("Separate several numbers with commas."),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if opt(config, "account_sid").is_some_and(|s| !s.starts_with("AC")) {
        errors.push(field_error(
            "account_sid",
            crate::i18n::t("notif.err_twilio_sid"),
        ));
    }
    if let Some(bad) = list(config, "to").into_iter().find(|n| !n.starts_with('+')) {
        errors.push(field_error(
            "to",
            crate::i18n::tf("notif.err_twilio_to", &[("value", &bad)]),
        ));
    }
    errors
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let sid = req(config, "account_sid")?;
    let token = req(config, "auth_token")?;
    let from = req(config, "from")?;
    let mut text = summary(event);
    if let Some(url) = &event.url {
        text.push('\n');
        text.push_str(url);
    }
    let url = format!(
        "https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json",
        path_segment(sid)
    );
    let from_field = if from.starts_with("MG") {
        "MessagingServiceSid"
    } else {
        "From"
    };
    let requests: Vec<OutRequest> = list(config, "to")
        .into_iter()
        .map(|to| {
            OutRequest::post_form(
                url.clone(),
                vec![
                    ("To".into(), to),
                    (from_field.into(), from.into()),
                    ("Body".into(), text.clone()),
                ],
            )
            .basic_auth(sid, token)
        })
        .collect();
    if requests.is_empty() {
        return Err("missing 'to' in channel config".into());
    }
    Ok(requests)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::notify::alert::EventKind;
    use crate::notify::http::OutBody;
    use crate::notify::tests::event;

    #[test]
    fn one_form_request_per_recipient() {
        let config =
            json!({"account_sid": "AC1", "auth_token": "tok", "from": "+1", "to": "+2, +3"});
        let reqs = build(&config, &event(EventKind::Down)).unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(
            reqs[0].url,
            "https://api.twilio.com/2010-04-01/Accounts/AC1/Messages.json"
        );
        let OutBody::Form(fields) = &reqs[1].body else {
            panic!("form body")
        };
        assert_eq!(fields[0], ("To".into(), "+3".into()));
        assert!(
            reqs[0]
                .header_value("authorization")
                .unwrap()
                .starts_with("Basic ")
        );
    }
}
