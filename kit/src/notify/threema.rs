//! Threema Gateway (basic mode): `send_simple` to an ID, phone number or email.

use serde_json::Value;

use super::http::{OutRequest, opt, req};
use super::message::{link, summary};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::notify::alert::Alert;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "threema",
    label: "Threema",
    icon: "send",
    category: Category::Messaging,
    description: "End-to-end encrypted messages through Threema Gateway",
    docs_url: "https://gateway.threema.ch/en/developer/api",
    fields: &[
        Field::text("gateway_id", "Gateway ID")
            .destination()
            .required()
            .placeholder("*VOLE123…")
            .help("Starts with * and has 8 characters."),
        Field::secret("secret", "API secret").required(),
        Field::select(
            "recipient_type",
            "Send to",
            &[
                ("identity", "Threema ID"),
                ("phone", "Phone number"),
                ("email", "Email address"),
            ],
        )
        .default("identity"),
        Field::text("recipient", "Recipient")
            .required()
            .placeholder("ABCD1234…"),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if opt(config, "gateway_id").is_some_and(|id| !(id.len() == 8 && id.starts_with('*'))) {
        errors.push(field_error(
            "gateway_id",
            crate::i18n::t("notif.err_threema_gateway"),
        ));
    }
    if let Some(to) = opt(config, "recipient") {
        let ok = match opt(config, "recipient_type").unwrap_or("identity") {
            "phone" => to.chars().all(|c| c.is_ascii_digit()) && to.len() >= 5,
            "email" => to.contains('@'),
            _ => to.len() == 8 && to.chars().all(|c| c.is_ascii_alphanumeric()),
        };
        if !ok {
            errors.push(field_error(
                "recipient",
                crate::i18n::t("notif.err_threema_recipient"),
            ));
        }
    }
    errors
}

pub fn build(config: &Value, event: &Alert) -> Result<Vec<OutRequest>, String> {
    let from = req(config, "gateway_id")?;
    let secret = req(config, "secret")?;
    let to = req(config, "recipient")?;
    let to_field = match opt(config, "recipient_type").unwrap_or("identity") {
        "phone" => "phone",
        "email" => "email",
        _ => "to",
    };
    let mut text = summary(event);
    if let Some(url) = link(event) {
        text.push('\n');
        text.push_str(&url);
    }
    Ok(vec![OutRequest::post_form(
        "https://msgapi.threema.ch/send_simple",
        vec![
            ("from".into(), from.into()),
            (to_field.into(), to.trim_start_matches('+').into()),
            ("secret".into(), secret.into()),
            ("text".into(), text),
        ],
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::AlertKind;
    use crate::notify::http::OutBody;
    use crate::notify::tests::event;
    use serde_json::json;

    fn fields(r: &OutRequest) -> Vec<(String, String)> {
        match &r.body {
            OutBody::Form(f) => f.clone(),
            other => panic!("form expected, got {other:?}"),
        }
    }

    #[test]
    fn sends_a_simple_message_to_the_chosen_kind_of_recipient() {
        for (kind, to, field) in [
            ("identity", "ABCD1234", "to"),
            ("phone", "+4712345678", "phone"),
            ("email", "me@example.com", "email"),
        ] {
            let r = &build(&json!({"gateway_id": "*VOLE123", "secret": "S", "recipient_type": kind, "recipient": to}), &event(AlertKind::Problem)).unwrap()[0];
            assert_eq!(r.url, "https://msgapi.threema.ch/send_simple");
            let f = fields(r);
            let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            assert_eq!(get("from").as_deref(), Some("*VOLE123"));
            assert_eq!(
                get(field).as_deref(),
                Some(to.trim_start_matches('+')),
                "{kind}"
            );
            assert!(get("text").unwrap().contains("Payments API is down"));
        }
    }

    #[test]
    fn ids_and_recipients_are_checked() {
        assert!(
            check(&json!({"gateway_id": "VOLE123"}))
                .iter()
                .any(|(k, _)| k == "gateway_id")
        );
        assert!(
            check(&json!({"gateway_id": "*VOLE123", "recipient": "abc"}))
                .iter()
                .any(|(k, _)| k == "recipient")
        );
        assert!(check(&json!({"gateway_id": "*VOLE123", "recipient": "ABCD1234"})).is_empty());
        assert!(check(&json!({"recipient_type": "phone", "recipient": "4712345678"})).is_empty());
        assert!(!check(&json!({"recipient_type": "email", "recipient": "nope"})).is_empty());
    }
}
