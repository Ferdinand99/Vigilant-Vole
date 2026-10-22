//! Email over SMTP (lettre): plain-text + HTML multipart, STARTTLS, implicit
//! TLS or plain.

use std::time::Duration;

use lettre::message::{Mailbox, Mailboxes, MultiPart, header};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde_json::Value;

use super::http::{flag, list, num, opt, req};
use super::message::{color_hex, detail, escape_html, facts, headline, product, text_body, title};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::notify::alert::Alert;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "email",
    label: "Email (SMTP)",
    icon: "mail",
    category: Category::Email,
    description: "Clean HTML email through your mail server",
    docs_url: "",
    fields: &[
        Field::text("host", "SMTP server")
            .required()
            .destination()
            .placeholder("smtp.example.com…"),
        Field::number("port", "Port")
            .destination()
            .default("587")
            .help("Usually 587 for STARTTLS, 465 for TLS, 25 for none."),
        Field::select(
            "security",
            "Security",
            &[
                ("starttls", "STARTTLS"),
                ("tls", "TLS (implicit)"),
                ("none", "None"),
            ],
        )
        .destination()
        .default("starttls"),
        Field::text("username", "Username").placeholder("alerts@example.com…"),
        Field::secret("password", "Password"),
        Field::text("from", "From")
            .required()
            .placeholder("Vigilant Vole <alerts@example.com>…"),
        Field::text("to", "To")
            .required()
            .placeholder("ops@example.com, oncall@example.com…")
            .help("Separate addresses with commas."),
        Field::text("cc", "Cc").placeholder("team@example.com…"),
        Field::text("bcc", "Bcc").placeholder("archive@example.com…"),
        Field::text("subject_prefix", "Subject prefix").placeholder("[Prod]…"),
        Field::boolean("ignore_tls", "Skip certificate verification")
            .help("Only for servers with a self-signed certificate.")
            .show_when("security=starttls,tls"),
    ],
    transport: Transport::Smtp,
    check: Some(check),
};

fn check(config: &Value) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if let Some(from) = opt(config, "from")
        && from.parse::<Mailbox>().is_err()
    {
        errors.push(field_error("from", crate::i18n::t("notif.err_email_from")));
    }
    for key in ["to", "cc", "bcc"] {
        if let Some(bad) = list(config, key)
            .into_iter()
            .find(|a| a.parse::<Mailbox>().is_err())
        {
            errors.push(field_error(
                key,
                crate::i18n::tf("notif.err_email_invalid", &[("value", &bad)]),
            ));
        }
    }
    if let Some(port) = num(config, "port")
        && !(1..=65535).contains(&port)
    {
        errors.push(field_error("port", crate::i18n::t("notif.err_port")));
    }
    if opt(config, "username").is_some() && opt(config, "password").is_none() {
        errors.push(field_error(
            "password",
            crate::i18n::t("notif.err_email_password"),
        ));
    }
    errors
}

fn mailboxes(config: &Value, key: &str) -> Result<Vec<Mailbox>, String> {
    list(config, key)
        .iter()
        .map(|a| {
            a.parse::<Mailbox>()
                .map_err(|e| format!("invalid {key} address '{a}': {e}"))
        })
        .collect()
}

/// The message (pure, testable).
pub fn build_message(config: &Value, event: &Alert) -> Result<Message, String> {
    let from: Mailbox = req(config, "from")?
        .parse()
        .map_err(|e| format!("invalid from address: {e}"))?;
    let to = mailboxes(config, "to")?;
    if to.is_empty() {
        return Err("missing 'to' in channel config".into());
    }
    let mut subject = title(event);
    if let Some(prefix) = opt(config, "subject_prefix") {
        subject = format!("{prefix} {subject}");
    }
    let mut builder = Message::builder().from(from).subject(subject);
    builder = builder.mailbox(header::To::from(to.into_iter().collect::<Mailboxes>()));
    let cc = mailboxes(config, "cc")?;
    if !cc.is_empty() {
        builder = builder.mailbox(header::Cc::from(cc.into_iter().collect::<Mailboxes>()));
    }
    let bcc = mailboxes(config, "bcc")?;
    if !bcc.is_empty() {
        builder = builder.mailbox(header::Bcc::from(bcc.into_iter().collect::<Mailboxes>()));
    }
    builder
        .multipart(MultiPart::alternative_plain_html(
            text_body(event),
            html_body(event),
        ))
        .map_err(|e| e.to_string())
}

/// A calm, single-column HTML email (inline styles — mail clients ignore
/// stylesheets).
pub fn html_body(event: &Alert) -> String {
    let font = "-apple-system,BlinkMacSystemFont,'SF Pro Text','Inter','Segoe UI',Roboto,Helvetica,Arial,sans-serif";
    let accent = color_hex(event);
    let mut rows = String::new();
    for (label, value) in facts(event) {
        rows.push_str(&format!(
            "<tr><td style=\"padding:10px 0;border-top:1px solid #e5e5ea;color:#6e6e73;font-size:14px;width:96px;vertical-align:top\">{}</td>\
             <td style=\"padding:10px 0;border-top:1px solid #e5e5ea;color:#1d1d1f;font-size:14px;word-break:break-word\">{}</td></tr>",
            escape_html(&label),
            escape_html(&value)
        ));
    }
    let detail_html = detail(event)
        .map(|d| {
            format!(
                "<p style=\"margin:8px 0 0;color:#424245;font-size:15px;line-height:1.45\">{}</p>",
                escape_html(&d)
            )
        })
        .unwrap_or_default();
    let button = event
        .url
        .as_ref()
        .map(|u| {
            format!(
                "<p style=\"margin:24px 0 0\"><a href=\"{}\" style=\"display:inline-block;padding:11px 20px;border-radius:980px;background:#1d1d1f;color:#ffffff;text-decoration:none;font-size:15px;font-weight:600\">{}</a></p>",
                escape_html(u),
                escape_html(event.link_label.as_str())
            )
        })
        .unwrap_or_default();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"color-scheme\" content=\"light\"><title>{title}</title></head>\
<body style=\"margin:0;padding:0;background:#f5f5f7;font-family:{font};-webkit-font-smoothing:antialiased\">\
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"background:#f5f5f7\"><tr><td align=\"center\" style=\"padding:32px 16px\">\
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"max-width:520px;background:#ffffff;border-radius:18px\">\
<tr><td style=\"padding:28px 28px 8px\">\
<p style=\"margin:0 0 14px;font-size:13px;font-weight:600;color:{accent}\">&#9679;&nbsp;{status}</p>\
<h1 style=\"margin:0;font-size:22px;line-height:1.25;font-weight:600;letter-spacing:-0.02em;color:#1d1d1f\">{headline}</h1>\
{detail_html}{button}</td></tr>\
<tr><td style=\"padding:16px 28px 20px\"><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\">{rows}</table></td></tr>\
</table>\
<p style=\"margin:16px 0 0;font-size:12px;color:#86868b\">{sent_by}</p>\
</td></tr></table></body></html>",
        title = escape_html(&title(event)),
        status = escape_html(event.event_label.as_str()),
        headline = escape_html(&headline(event)),
        sent_by = escape_html(&crate::i18n::tf("msg.sent_by", &[("product", product())])),
    )
}

pub async fn send(config: &Value, event: &Alert) -> Result<(), String> {
    deliver(config, build_message(config, event)?).await
}

/// Sends an already-built message through the SMTP settings in `config`.
pub async fn deliver(config: &Value, message: Message) -> Result<(), String> {
    let host = req(config, "host")?;
    let security = opt(config, "security").unwrap_or("starttls");
    let port = num(config, "port")
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(match security {
            "tls" => 465,
            "none" => 25,
            _ => 587,
        });
    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
        .port(port)
        .timeout(Some(Duration::from_secs(10)));
    if security != "none" {
        let mut params = TlsParameters::builder(host.to_string())
            .dangerous_accept_invalid_certs(flag(config, "ignore_tls"))
            .dangerous_accept_invalid_hostnames(flag(config, "ignore_tls"));
        // A system without CA certificates can't build the platform verifier
        // on its own; give it the bundled roots.
        if !crate::tls_roots::system_store_available() {
            for der in webpki_root_certs::TLS_SERVER_ROOT_CERTS {
                if let Ok(cert) =
                    lettre::transport::smtp::client::Certificate::from_der(der.to_vec())
                {
                    params = params.add_root_certificate(cert);
                }
            }
        }
        let params = params
            .build_rustls()
            .map_err(|e| format!("TLS setup failed: {e}"))?;
        builder = builder.tls(if security == "tls" {
            Tls::Wrapper(params)
        } else {
            Tls::Required(params)
        });
    }
    if let Some(user) = opt(config, "username") {
        builder = builder.credentials(Credentials::new(
            user.to_string(),
            opt(config, "password").unwrap_or("").to_string(),
        ));
    }
    let transport = builder.build();
    tokio::time::timeout(Duration::from_secs(30), transport.send(message))
        .await
        .map_err(|_| "timed out talking to the SMTP server".to_string())?
        .map(|_| ())
        .map_err(|e| format!("SMTP: {e}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::notify::alert::AlertKind;
    use crate::notify::tests::alert;

    fn config() -> Value {
        json!({"host": "smtp.test", "from": "Vole <vv@example.com>", "to": "a@example.com, b@example.com",
            "cc": "c@example.com", "subject_prefix": "[Prod]"})
    }

    #[test]
    fn builds_multipart_message_with_recipients() {
        let mut e = alert(AlertKind::Problem, Some("<HTTP 503>"), None);
        e.url = Some("https://vv.example/monitors/7".into());
        let msg = build_message(&config(), &e).unwrap();
        let raw = String::from_utf8(msg.formatted()).unwrap();
        assert!(raw.contains("To: a@example.com, b@example.com"), "{raw}");
        assert!(raw.contains("Cc: c@example.com"));
        assert!(raw.contains("multipart/alternative"));
        assert!(raw.contains("text/plain"));
        assert!(raw.contains("text/html"));
        assert_eq!(msg.envelope().to().len(), 3);
    }

    #[test]
    fn html_is_escaped_and_has_button() {
        let mut e = alert(AlertKind::Problem, Some("<script>"), None);
        e.url = Some("https://vv.example/monitors/7".into());
        let html = html_body(&e);
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("href=\"https://vv.example/monitors/7\""));
        assert!(html.contains("Payments API is down"));
    }

    #[test]
    fn validates_addresses_and_password() {
        let errors = check(
            &json!({"from": "nope", "to": "a@example.com, bad", "username": "u", "port": 70000}),
        );
        let keys: Vec<_> = errors.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["from", "to", "port", "password"]);
    }
}
