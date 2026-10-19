//! Generic webhook: JSON (or form) POST with optional custom headers and an
//! optional body template with `{{placeholder}}` substitution.

use std::sync::LazyLock;

use regex::Regex;
use reqwest::Method;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use super::http::{OutBody, OutRequest, opt, pairs, req};
use super::message::{format_duration, headline, status_word, summary};
use super::schema::{Category, Field, FieldError, ProviderSchema, Transport, field_error};
use crate::db::models::NotificationEvent;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "webhook",
    label: "Webhook",
    icon: "link",
    category: Category::Automation,
    description: "POST JSON to any URL",
    docs_url: "",
    fields: &[
        Field::url("url", "URL").required().sensitive().placeholder("https://example.com/hooks/vigilant…"),
        Field::select("method", "Method", &[("POST", "POST"), ("PUT", "PUT"), ("PATCH", "PATCH")]).default("POST"),
        Field::select("content_type", "Body format", &[("json", "JSON"), ("form", "Form (URL-encoded)")]).default("json"),
        Field::key_value("headers", "Custom headers").sensitive().help("Sent with every request, e.g. Authorization."),
        Field::textarea("body_template", "Custom body")
            .placeholder("{\"text\": \"{{summary}}\"}…")
            .help("Optional. Placeholders: {{monitor_name}}, {{monitor_id}}, {{monitor_type}}, {{target}}, {{status}}, {{event}}, {{message}}, {{summary}}, {{headline}}, {{url}}, {{downtime}}, {{downtime_seconds}}, {{expires_in_days}}, {{occurred_at}}. Leave empty for the default JSON."),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

const PLACEHOLDERS: &[&str] = &[
    "monitor_name",
    "monitor_id",
    "monitor_type",
    "target",
    "status",
    "event",
    "message",
    "summary",
    "headline",
    "url",
    "downtime",
    "downtime_seconds",
    "expires_in_days",
    "occurred_at",
];

static PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*([a-z_]+)\s*\}\}").expect("constant pattern is valid"));

/// Placeholders that are numbers (rendered as `null` in JSON when absent).
const NUMERIC: &[&str] = &["monitor_id", "downtime_seconds", "expires_in_days"];

fn check(config: &Value) -> Vec<FieldError> {
    let mut errors = Vec::new();
    for (name, value) in pairs(config, "headers") {
        let bad_name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err();
        let bad_value = value != super::schema::SECRET_MASK
            && reqwest::header::HeaderValue::from_str(&value).is_err();
        if bad_name || bad_value {
            errors.push(field_error(
                "headers",
                crate::i18n::tf("notif.err_webhook_header", &[("name", &name)]),
            ));
            break;
        }
    }
    if let Some(tpl) = opt(config, "body_template") {
        if let Some(unknown) = PLACEHOLDER_RE
            .captures_iter(tpl)
            .filter_map(|c| c.get(1))
            .find(|m| !PLACEHOLDERS.contains(&m.as_str()))
        {
            errors.push(field_error(
                "body_template",
                crate::i18n::tf(
                    "notif.err_webhook_placeholder",
                    &[("placeholder", &format!("{{{{{}}}}}", unknown.as_str()))],
                ),
            ));
        } else if opt(config, "content_type").unwrap_or("json") == "json" {
            // Check the template yields valid JSON for a sample event.
            let sample = render(tpl, &sample_event(), Escape::Json);
            if let Err(e) = serde_json::from_str::<Value>(&sample) {
                errors.push(field_error(
                    "body_template",
                    crate::i18n::tf("notif.err_webhook_json", &[("err", &e.to_string())]),
                ));
            }
        }
    }
    errors
}

fn sample_event() -> NotificationEvent {
    let mut event = super::message::test_event("Sample \"monitor\"", Some("https://example.com"));
    event.monitor_id = Some(1);
    event
}

/// The standard body. `monitor` / `status` / `message` are the original
/// (pre-0002) fields; the rest were added alongside them.
pub fn default_body(event: &NotificationEvent) -> Value {
    json!({
        "monitor": event.monitor_name,
        "status": status_word(event.kind),
        "message": event.message,
        "event": event.kind.as_str(),
        "summary": summary(event),
        "monitor_id": event.monitor_id,
        "monitor_type": event.monitor_type,
        "target": event.target,
        "url": event.url,
        "downtime_seconds": event.downtime.map(|d| d.as_secs()),
        "expires_in_days": event.expires_in_days,
        "occurred_at": event.occurred_at.format(&Rfc3339).ok(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    /// Values are JSON-string-escaped (no surrounding quotes), so
    /// `"{{message}}"` is always valid JSON.
    Json,
    /// Values are URL-encoded.
    Form,
    None,
}

fn placeholder_value(name: &str, event: &NotificationEvent) -> Option<String> {
    let opt_num = |n: Option<i64>| n.map(|v| v.to_string()).unwrap_or_default();
    Some(match name {
        "monitor_name" => event.monitor_name.clone(),
        "monitor_id" => opt_num(event.monitor_id),
        "monitor_type" => event.monitor_type.clone(),
        "target" => event.target.clone(),
        "status" => status_word(event.kind).to_string(),
        "event" => event.kind.as_str().to_string(),
        "message" => event.message.clone().unwrap_or_default(),
        "summary" => summary(event),
        "headline" => headline(event),
        "url" => event.url.clone().unwrap_or_default(),
        "downtime" => event.downtime.map(format_duration).unwrap_or_default(),
        "downtime_seconds" => event
            .downtime
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default(),
        "expires_in_days" => opt_num(event.expires_in_days),
        "occurred_at" => event.occurred_at.format(&Rfc3339).unwrap_or_default(),
        _ => return None,
    })
}

/// Substitutes `{{name}}` placeholders; unknown ones are left as written.
pub fn render(template: &str, event: &NotificationEvent, escape: Escape) -> String {
    PLACEHOLDER_RE
        .replace_all(template, |caps: &regex::Captures| {
            let name = caps.get(1).map_or("", |m| m.as_str());
            match placeholder_value(name, event) {
                Some(v) if v.is_empty() && escape == Escape::Json && NUMERIC.contains(&name) => {
                    "null".to_string()
                }
                Some(v) => match escape {
                    Escape::Json => {
                        let quoted = Value::String(v).to_string();
                        quoted[1..quoted.len() - 1].to_string()
                    }
                    Escape::Form => super::http::urlencode(&v),
                    Escape::None => v,
                },
                None => caps[0].to_string(),
            }
        })
        .into_owned()
}

fn flatten(value: &Value) -> Vec<(String, String)> {
    value
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.as_str().map_or_else(|| v.to_string(), str::to_string),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn build(config: &Value, event: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
    let url = req(config, "url")?;
    let method = match opt(config, "method").unwrap_or("POST") {
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        _ => Method::POST,
    };
    let form = opt(config, "content_type") == Some("form");
    let body = match (opt(config, "body_template"), form) {
        (Some(tpl), false) => OutBody::Raw {
            content_type: "application/json".into(),
            body: render(tpl, event, Escape::Json),
        },
        (Some(tpl), true) => OutBody::Raw {
            content_type: "application/x-www-form-urlencoded".into(),
            body: render(tpl, event, Escape::Form),
        },
        (None, false) => OutBody::Json(default_body(event)),
        (None, true) => OutBody::Form(flatten(&default_body(event))),
    };
    let mut request = OutRequest {
        method,
        url: url.to_string(),
        headers: Vec::new(),
        body,
    };
    for (name, value) in pairs(config, "headers") {
        request = request.header(name, value);
    }
    Ok(vec![request])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::db::models::EventKind;
    use crate::notification::tests::event;

    #[test]
    fn default_body_keeps_legacy_fields() {
        let mut e = event(EventKind::Up);
        e.downtime = Some(Duration::from_secs(90));
        let reqs = build(&json!({"url": "https://x.test/h"}), &e).unwrap();
        assert_eq!(reqs[0].method, Method::POST);
        let body = reqs[0].json().unwrap();
        assert_eq!(body["monitor"], "Payments API");
        assert_eq!(body["status"], "up");
        assert_eq!(body["event"], "up");
        assert_eq!(body["downtime_seconds"], 90);
        assert_eq!(body["monitor_id"], 7);
    }

    #[test]
    fn template_values_are_json_escaped() {
        let mut e = event(EventKind::Down);
        e.message = Some("said \"no\"\nnewline".into());
        let out = render(
            r#"{"t": "{{ monitor_name }} {{message}}", "id": {{monitor_id}}, "x": "{{nope}}"}"#,
            &e,
            Escape::Json,
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["t"], "Payments API said \"no\"\nnewline");
        assert_eq!(v["id"], 7);
        assert_eq!(v["x"], "{{nope}}");
    }

    #[test]
    fn form_template_is_url_encoded() {
        let e = event(EventKind::Down);
        assert_eq!(
            render("name={{monitor_name}}&s={{status}}", &e, Escape::Form),
            "name=Payments+API&s=down"
        );
    }

    #[test]
    fn custom_headers_method_and_form_body() {
        let e = event(EventKind::Down);
        let config = json!({"url": "https://x.test/h", "method": "PUT", "content_type": "form",
            "headers": {"Authorization": "Bearer t"}});
        let req = &build(&config, &e).unwrap()[0];
        assert_eq!(req.method, Method::PUT);
        assert_eq!(req.header_value("authorization"), Some("Bearer t"));
        let OutBody::Form(fields) = &req.body else {
            panic!("expected form body")
        };
        assert!(fields.contains(&("monitor".into(), "Payments API".into())));
        assert!(!fields.iter().any(|(k, _)| k == "url"), "nulls are dropped");
    }

    #[test]
    fn validation_catches_bad_templates_and_headers() {
        let bad_json = check(&json!({"body_template": "{\"a\": {{message}}}"}));
        assert_eq!(
            bad_json.len(),
            1,
            "unquoted string placeholder → invalid JSON"
        );
        let unknown = check(&json!({"body_template": "{\"a\": \"{{monitr}}\"}"}));
        assert!(unknown[0].1.contains("Unknown placeholder"));
        let ok =
            check(&json!({"body_template": "{\"a\": \"{{message}}\", \"id\": {{monitor_id}}}"}));
        assert!(ok.is_empty(), "{ok:?}");
        let header = check(&json!({"headers": {"Bad Header": "x"}}));
        assert_eq!(header[0].0, "headers");
    }
}
