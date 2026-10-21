//! Transactional email and SMS APIs plus Gorush: SendGrid, Brevo, ClickSend,
//! 46elks and Gorush. Each is one JSON or form request per recipient list.

use serde_json::{Value, json};

use super::http::{OutRequest, join, list, opt, req};
use super::message::{Severity, body_text, headline, link, product, severity, summary};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::notify::alert::NotificationEvent;

fn valid_email(addr: &str) -> bool {
    let Some((local, domain)) = addr.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !addr.contains(char::is_whitespace)
}

/// Email problems for the `from` and `to` fields of the email APIs.
fn check_addresses(config: &Value) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if opt(config, "from").is_some_and(|f| !valid_email(f)) {
        errors.push(field_error(
            "from",
            crate::i18n::t("notif.err_email_address"),
        ));
    }
    if let Some(bad) = list(config, "to").into_iter().find(|a| !valid_email(a)) {
        errors.push(field_error(
            "to",
            crate::i18n::tf("notif.err_email_to", &[("value", &bad)]),
        ));
    }
    errors
}

fn email_subject(event: &NotificationEvent) -> String {
    format!("{} {}", super::message::emoji(event.kind), headline(event))
}

fn plain_text(event: &NotificationEvent) -> String {
    let mut text = format!("{}\n{}", headline(event), body_text(event));
    if let Some(url) = link(event)
        && !text.contains(&url)
    {
        text.push('\n');
        text.push_str(&url);
    }
    text
}

// -- SendGrid ---------------------------------------------------------------------

pub const SENDGRID: ProviderSchema = ProviderSchema {
    kind: "sendgrid",
    label: "SendGrid",
    icon: "mail",
    category: Category::Email,
    description: "Email through the SendGrid API",
    docs_url: "https://www.twilio.com/docs/sendgrid/api-reference/mail-send/mail-send",
    fields: &[
        Field::secret("api_key", "API key")
            .required()
            .placeholder("SG.…")
            .help("Needs the Mail Send permission."),
        Field::text("from", "From")
            .required()
            .placeholder("alerts@example.com…")
            .help("A verified sender in SendGrid."),
        Field::text("from_name", "Sender name").placeholder("Vigilant Vole…"),
        Field::text("to", "To")
            .required()
            .placeholder("ops@example.com…")
            .help("Separate several addresses with commas."),
    ],
    transport: Transport::Http(build_sendgrid),
    check: Some(check_addresses),
};

pub fn build_sendgrid(
    config: &Value,
    event: &NotificationEvent,
) -> Result<Vec<OutRequest>, String> {
    let key = req(config, "api_key")?;
    let from = req(config, "from")?;
    let to = list(config, "to");
    if to.is_empty() {
        return Err("missing 'to' in channel config".into());
    }
    let mut sender = json!({"email": from});
    if let Some(name) = opt(config, "from_name") {
        sender["name"] = json!(name);
    }
    let body = json!({
        "personalizations": [{"to": to.iter().map(|a| json!({"email": a})).collect::<Vec<_>>()}],
        "from": sender,
        "subject": email_subject(event),
        "content": [{"type": "text/plain", "value": plain_text(event)}],
    });
    Ok(vec![
        OutRequest::post_json("https://api.sendgrid.com/v3/mail/send", body).bearer(key),
    ])
}

// -- Brevo ------------------------------------------------------------------------

pub const BREVO: ProviderSchema = ProviderSchema {
    kind: "brevo",
    label: "Brevo",
    icon: "mail",
    category: Category::Email,
    description: "Email through the Brevo (Sendinblue) API",
    docs_url: "https://developers.brevo.com/reference/sendtransacemail",
    fields: &[
        Field::secret("api_key", "API key")
            .required()
            .placeholder("xkeysib-…"),
        Field::text("from", "From")
            .required()
            .placeholder("alerts@example.com…")
            .help("A sender you have added in Brevo."),
        Field::text("from_name", "Sender name").placeholder("Vigilant Vole…"),
        Field::text("to", "To")
            .required()
            .placeholder("ops@example.com…")
            .help("Separate several addresses with commas."),
    ],
    transport: Transport::Http(build_brevo),
    check: Some(check_addresses),
};

pub fn build_brevo(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let key = req(config, "api_key")?;
    let from = req(config, "from")?;
    let to = list(config, "to");
    if to.is_empty() {
        return Err("missing 'to' in channel config".into());
    }
    let body = json!({
        "sender": {"email": from, "name": opt(config, "from_name").unwrap_or(product())},
        "to": to.iter().map(|a| json!({"email": a})).collect::<Vec<_>>(),
        "subject": email_subject(event),
        "textContent": plain_text(event),
    });
    Ok(vec![
        OutRequest::post_json("https://api.brevo.com/v3/smtp/email", body).header("api-key", key),
    ])
}

// -- ClickSend ---------------------------------------------------------------------

pub const CLICKSEND: ProviderSchema = ProviderSchema {
    kind: "clicksend",
    label: "SMS (ClickSend)",
    icon: "send",
    category: Category::Messaging,
    description: "Text messages through ClickSend",
    docs_url: "https://developers.clicksend.com/docs/rest/v3/#send-sms",
    fields: &[
        Field::text("username", "Username").destination().required(),
        Field::secret("api_key", "API key").required(),
        Field::text("to", "To")
            .required()
            .placeholder("+4712345678…")
            .help("Separate several numbers with commas."),
        Field::text("from", "Sender")
            .placeholder("Vole…")
            .help("Optional: a sender ID or number you own."),
    ],
    transport: Transport::Http(build_clicksend),
    check: Some(check_numbers),
};

fn check_numbers(config: &Value) -> Vec<FieldError> {
    match list(config, "to").into_iter().find(|n| !n.starts_with('+')) {
        Some(bad) => vec![field_error(
            "to",
            crate::i18n::tf("notif.err_twilio_to", &[("value", &bad)]),
        )],
        None => Vec::new(),
    }
}

pub fn build_clicksend(
    config: &Value,
    event: &NotificationEvent,
) -> Result<Vec<OutRequest>, String> {
    let user = req(config, "username")?;
    let key = req(config, "api_key")?;
    let to = list(config, "to");
    if to.is_empty() {
        return Err("missing 'to' in channel config".into());
    }
    let text = sms_text(event);
    let messages: Vec<Value> = to
        .iter()
        .map(|n| {
            let mut m = json!({"source": super::message::slug(), "body": text, "to": n});
            if let Some(from) = opt(config, "from") {
                m["from"] = json!(from);
            }
            m
        })
        .collect();
    Ok(vec![
        OutRequest::post_json(
            "https://rest.clicksend.com/v3/sms/send",
            json!({ "messages": messages }),
        )
        .basic_auth(user, key),
    ])
}

/// SMS text: the summary and the deep link.
fn sms_text(event: &NotificationEvent) -> String {
    let mut text = summary(event);
    if let Some(url) = &event.url {
        text.push('\n');
        text.push_str(url);
    }
    text
}

// -- 46elks ------------------------------------------------------------------------

pub const ELKS: ProviderSchema = ProviderSchema {
    kind: "elks",
    label: "SMS (46elks)",
    icon: "send",
    category: Category::Messaging,
    description: "Text messages through 46elks",
    docs_url: "https://46elks.com/docs/send-sms",
    fields: &[
        Field::text("username", "API username")
            .destination()
            .required()
            .placeholder("u…"),
        Field::secret("password", "API password").required(),
        Field::text("from", "From")
            .required()
            .placeholder("Vole…")
            .help("A number you own, or a name of up to 11 letters."),
        Field::text("to", "To")
            .required()
            .placeholder("+46701234567…")
            .help("Separate several numbers with commas."),
    ],
    transport: Transport::Http(build_elks),
    check: Some(check_numbers),
};

pub fn build_elks(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let user = req(config, "username")?;
    let pass = req(config, "password")?;
    let from = req(config, "from")?;
    let text = sms_text(event);
    let requests: Vec<OutRequest> = list(config, "to")
        .into_iter()
        .map(|to| {
            OutRequest::post_form(
                "https://api.46elks.com/a1/sms",
                vec![
                    ("from".into(), from.into()),
                    ("to".into(), to),
                    ("message".into(), text.clone()),
                ],
            )
            .basic_auth(user, pass)
        })
        .collect();
    if requests.is_empty() {
        return Err("missing 'to' in channel config".into());
    }
    Ok(requests)
}

// -- Gorush -----------------------------------------------------------------------

pub const GORUSH: ProviderSchema = ProviderSchema {
    kind: "gorush",
    label: "Gorush",
    icon: "bell",
    category: Category::Push,
    description: "Push to iOS, Android and Huawei devices through your Gorush server",
    docs_url: "https://github.com/appleboy/gorush",
    fields: &[
        Field::url("server", "Gorush URL")
            .required()
            .placeholder("http://gorush:8088…"),
        Field::select(
            "platform",
            "Platform",
            &[("ios", "iOS"), ("android", "Android"), ("huawei", "Huawei")],
        )
        .default("android"),
        Field::text("tokens", "Device tokens")
            .required()
            .help("Separate several tokens with commas."),
        Field::text("topic", "App topic")
            .placeholder("com.example.app…")
            .help("iOS only: the app’s bundle ID."),
    ],
    transport: Transport::Http(build_gorush),
    check: None,
};

pub fn build_gorush(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let server = req(config, "server")?;
    let tokens = list(config, "tokens");
    if tokens.is_empty() {
        return Err("missing 'tokens' in channel config".into());
    }
    let platform = match opt(config, "platform") {
        Some("ios") => 1,
        Some("huawei") => 3,
        _ => 2,
    };
    let mut notification = json!({
        "tokens": tokens,
        "platform": platform,
        "title": headline(event),
        "message": body_text(event),
        "priority": if severity(event.kind) == Severity::Critical { "high" } else { "normal" },
    });
    if platform == 1
        && let Some(topic) = opt(config, "topic")
    {
        notification["topic"] = json!(topic);
    }
    Ok(vec![OutRequest::post_json(
        join(server, "/api/push"),
        json!({ "notifications": [notification] }),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::alert::EventKind;
    use crate::notify::http::OutBody;
    use crate::notify::tests::event;

    #[test]
    fn sendgrid_sends_one_mail_to_all_recipients_with_a_bearer_key() {
        let c = json!({"api_key": "SG.key", "from": "alerts@example.com", "from_name": "Vole", "to": "a@example.com, b@example.com"});
        let r = &build_sendgrid(&c, &event(EventKind::Down)).unwrap()[0];
        assert_eq!(r.url, "https://api.sendgrid.com/v3/mail/send");
        assert_eq!(r.header_value("Authorization"), Some("Bearer SG.key"));
        let b = r.json().unwrap();
        assert_eq!(
            b["personalizations"][0]["to"],
            json!([{"email": "a@example.com"}, {"email": "b@example.com"}])
        );
        assert_eq!(
            (b["from"]["email"].as_str(), b["from"]["name"].as_str()),
            (Some("alerts@example.com"), Some("Vole"))
        );
        assert!(
            b["subject"]
                .as_str()
                .unwrap()
                .contains("Payments API is down")
        );
        assert_eq!(b["content"][0]["type"], "text/plain");
    }

    #[test]
    fn brevo_uses_the_api_key_header_and_text_content() {
        let c =
            json!({"api_key": "xkeysib-1", "from": "alerts@example.com", "to": "ops@example.com"});
        let r = &build_brevo(&c, &event(EventKind::Up)).unwrap()[0];
        assert_eq!(r.url, "https://api.brevo.com/v3/smtp/email");
        assert_eq!(r.header_value("api-key"), Some("xkeysib-1"));
        let b = r.json().unwrap();
        assert_eq!(
            (b["sender"]["email"].as_str(), b["sender"]["name"].as_str()),
            (Some("alerts@example.com"), Some("Vigilant Vole"))
        );
        assert_eq!(b["to"], json!([{"email": "ops@example.com"}]));
        assert!(b["textContent"].as_str().unwrap().contains("Payments API"));
    }

    #[test]
    fn email_addresses_are_checked() {
        assert!(
            check_addresses(&json!({"from": "nope", "to": "a@b.example"}))
                .iter()
                .any(|(k, _)| k == "from")
        );
        assert!(
            check_addresses(&json!({"from": "a@b.example", "to": "ok@b.example, bad"}))
                .iter()
                .any(|(k, _)| k == "to")
        );
        assert!(
            check_addresses(&json!({"from": "a@b.example", "to": "x@y.example,z@w.example"}))
                .is_empty()
        );
        assert!(
            valid_email("a.b+c@sub.example.com")
                && !valid_email("a@b")
                && !valid_email("@b.example")
                && !valid_email("a b@c.example")
        );
    }

    #[test]
    fn clicksend_batches_the_recipients_into_one_request() {
        let c = json!({"username": "u", "api_key": "K", "to": "+4711111111, +4722222222", "from": "Vole"});
        let reqs = build_clicksend(&c, &event(EventKind::Down)).unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0]
                .header_value("Authorization")
                .unwrap()
                .starts_with("Basic ")
        );
        let m = &reqs[0].json().unwrap()["messages"];
        assert_eq!(m.as_array().unwrap().len(), 2);
        assert_eq!(
            (
                m[1]["to"].as_str(),
                m[1]["from"].as_str(),
                m[0]["source"].as_str()
            ),
            (Some("+4722222222"), Some("Vole"), Some("vigilant-vole"))
        );
        assert!(!check_numbers(&json!({"to": "4711111111"})).is_empty());
    }

    #[test]
    fn elks_sends_one_form_request_per_number() {
        let c = json!({"username": "u1", "password": "pw", "from": "Vole", "to": "+46701111111,+46702222222"});
        let reqs = build_elks(&c, &event(EventKind::Down)).unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].url, "https://api.46elks.com/a1/sms");
        let OutBody::Form(f) = &reqs[1].body else {
            panic!("form")
        };
        let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(
            (get("from"), get("to")),
            (Some("Vole"), Some("+46702222222"))
        );
        assert!(get("message").unwrap().contains("Payments API is down"));
    }

    #[test]
    fn gorush_maps_platforms_and_only_sends_topics_to_ios() {
        let base = json!({"server": "http://gorush:8088/", "tokens": "t1, t2", "topic": "com.example.app"});
        let android = &build_gorush(&base, &event(EventKind::Down)).unwrap()[0];
        assert_eq!(android.url, "http://gorush:8088/api/push");
        let n = &android.json().unwrap()["notifications"][0];
        assert_eq!(
            (n["platform"].as_i64(), n["priority"].as_str()),
            (Some(2), Some("high"))
        );
        assert_eq!(n["tokens"], json!(["t1", "t2"]));
        assert!(n.get("topic").is_none());
        let mut ios = base.clone();
        ios["platform"] = json!("ios");
        let built = build_gorush(&ios, &event(EventKind::Up)).unwrap();
        let n = &built[0].json().unwrap()["notifications"][0];
        assert_eq!(
            (
                n["platform"].as_i64(),
                n["topic"].as_str(),
                n["priority"].as_str()
            ),
            (Some(1), Some("com.example.app"), Some("normal"))
        );
        ios["platform"] = json!("huawei");
        assert_eq!(
            build_gorush(&ios, &event(EventKind::Up)).unwrap()[0]
                .json()
                .unwrap()["notifications"][0]["platform"],
            3
        );
    }
}
