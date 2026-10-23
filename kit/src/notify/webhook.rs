//! Generic webhook: JSON (or form) POST with optional custom headers and an
//! optional body template with `{{placeholder}}` substitution.
//!
//! What the alert is about is named in the product's words
//! ([`Wording`], `KitConfig::webhook`): the default body's fields and the
//! placeholders.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use reqwest::Method;
use serde_json::{Map, Value, json};
use time::format_description::well_known::Rfc3339;

use super::http::{OutBody, OutRequest, opt, pairs, req};
use super::message::{format_duration, headline, summary};
use super::schema::{
    Category, Field, FieldError, ProviderSchema, Transport, WEBHOOK_URL, field_error,
};
use crate::notify::alert::Alert;

/// How the webhook provider names what an alert is about, in the
/// product's [`KitConfig::webhook`](crate::config::KitConfig::webhook).
/// Both names are lower-case identifiers (`[a-z_]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wording {
    /// The thing alerts are about ("monitor", "issue"): the default body's
    /// `"<subject>"` (its name), `"<subject>_id"` and `"<subject>_type"`,
    /// and the placeholders `{{<subject>_name}}`, `{{<subject>_id}}` and
    /// `{{<subject>_type}}`.
    pub subject: &'static str,
    /// How long the problem lasted ("downtime", "duration"): the default
    /// body's `"<duration>_seconds"` and the placeholders `{{<duration>}}`
    /// and `{{<duration>_seconds}}`.
    pub duration: &'static str,
    /// The URL field's example ("https://example.com/hooks/…").
    pub url_example: &'static str,
}

impl Wording {
    /// Names that fit any product: `subject`, `duration`.
    pub const NEUTRAL: Wording = Wording {
        subject: "subject",
        duration: "duration",
        url_example: "https://example.com/hooks/…",
    };
}

fn wording() -> Wording {
    crate::config::get().webhook
}

/// `text` (a help text written with the neutral placeholders
/// `{{subject_…}}` and `{{duration…}}`) with the product's names.
pub(crate) fn with_wording(text: &'static str) -> Cow<'static, str> {
    if !text.contains("{{subject_") && !text.contains("{{duration") {
        return Cow::Borrowed(text);
    }
    let w = wording();
    Cow::Owned(
        text.replace("{{subject_", &format!("{{{{{}_", w.subject))
            .replace("{{duration", &format!("{{{{{}", w.duration)),
    )
}

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "webhook",
    label: "Webhook",
    icon: "link",
    category: Category::Automation,
    description: "POST JSON to any URL",
    docs_url: "",
    fields: &[
        Field::url("url", "URL").required().sensitive().placeholder(WEBHOOK_URL),
        Field::select("method", "Method", &[("POST", "POST"), ("PUT", "PUT"), ("PATCH", "PATCH")]).default("POST"),
        Field::select("content_type", "Body format", &[("json", "JSON"), ("form", "Form (URL-encoded)")]).default("json"),
        Field::key_value("headers", "Custom headers").sensitive().help("Sent with every request, e.g. Authorization."),
        Field::textarea("body_template", "Custom body")
            .placeholder("{\"text\": \"{{summary}}\"}…")
            .help("Optional. Placeholders: {{subject_name}}, {{subject_id}}, {{subject_type}}, {{target}}, {{status}}, {{event}}, {{message}}, {{summary}}, {{headline}}, {{url}}, {{duration}}, {{duration_seconds}}, {{expires_in_days}}, {{occurred_at}}. Leave empty for the default JSON."),
    ],
    transport: Transport::Http(build),
    check: Some(check),
};

/// What a placeholder stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    SubjectName,
    SubjectId,
    SubjectType,
    Target,
    Status,
    Event,
    Message,
    Summary,
    Headline,
    Url,
    Duration,
    DurationSeconds,
    ExpiresInDays,
    OccurredAt,
}

impl Slot {
    const ALL: [Slot; 14] = [
        Slot::SubjectName,
        Slot::SubjectId,
        Slot::SubjectType,
        Slot::Target,
        Slot::Status,
        Slot::Event,
        Slot::Message,
        Slot::Summary,
        Slot::Headline,
        Slot::Url,
        Slot::Duration,
        Slot::DurationSeconds,
        Slot::ExpiresInDays,
        Slot::OccurredAt,
    ];

    /// The placeholder's name in the product's words.
    fn name(self, w: Wording) -> Cow<'static, str> {
        match self {
            Slot::SubjectName => format!("{}_name", w.subject).into(),
            Slot::SubjectId => format!("{}_id", w.subject).into(),
            Slot::SubjectType => format!("{}_type", w.subject).into(),
            Slot::Target => "target".into(),
            Slot::Status => "status".into(),
            Slot::Event => "event".into(),
            Slot::Message => "message".into(),
            Slot::Summary => "summary".into(),
            Slot::Headline => "headline".into(),
            Slot::Url => "url".into(),
            Slot::Duration => w.duration.into(),
            Slot::DurationSeconds => format!("{}_seconds", w.duration).into(),
            Slot::ExpiresInDays => "expires_in_days".into(),
            Slot::OccurredAt => "occurred_at".into(),
        }
    }

    fn named(name: &str) -> Option<Slot> {
        let w = wording();
        Slot::ALL.into_iter().find(|slot| slot.name(w) == name)
    }

    /// Numbers are rendered as `null` in JSON when absent.
    fn numeric(self) -> bool {
        matches!(
            self,
            Slot::SubjectId | Slot::DurationSeconds | Slot::ExpiresInDays
        )
    }
}

/// The placeholders a body template may use, in the product's words.
pub fn placeholders() -> Vec<String> {
    let w = wording();
    Slot::ALL
        .iter()
        .map(|slot| slot.name(w).into_owned())
        .collect()
}

static PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*([a-z_]+)\s*\}\}").expect("constant pattern is valid"));

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
            .find(|m| Slot::named(m.as_str()).is_none())
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

/// A sample alert with characters that need escaping, to check templates.
fn sample_event() -> Alert {
    let kind = super::alert::AlertKind::Test;
    Alert {
        kind,
        event: "test".into(),
        event_label: "Test".into(),
        status: "test".into(),
        level: kind.level(),
        colour: kind.level().colour(),
        emoji: kind.emoji().into(),
        title: "Test notification from Sample \"monitor\"".into(),
        detail: Some("A \"quoted\" line\nand a second one".into()),
        facts: Vec::new(),
        link_label: "View".into(),
        subject_id: Some(1),
        subject: "Sample \"monitor\"".into(),
        subject_type: String::new(),
        target: String::new(),
        message: Some("A \"quoted\" line\nand a second one".into()),
        duration: None,
        expires_in_days: None,
        url: Some("https://example.com".into()),
        occurred_at: time::OffsetDateTime::now_utc(),
    }
}

/// The standard body: the subject's name under `"<subject>"`, `status` and
/// `message` (the original fields), then the rest.
pub fn default_body(event: &Alert) -> Value {
    let w = wording();
    let mut body = Map::new();
    body.insert(w.subject.into(), json!(event.subject));
    body.insert("status".into(), json!(event.status.as_str()));
    body.insert("message".into(), json!(event.message));
    body.insert("event".into(), json!(event.event.as_str()));
    body.insert("summary".into(), json!(summary(event)));
    body.insert(format!("{}_id", w.subject), json!(event.subject_id));
    body.insert(format!("{}_type", w.subject), json!(event.subject_type));
    body.insert("target".into(), json!(event.target));
    body.insert("url".into(), json!(event.url));
    body.insert(
        format!("{}_seconds", w.duration),
        json!(event.duration.map(|d| d.as_secs())),
    );
    body.insert("expires_in_days".into(), json!(event.expires_in_days));
    body.insert(
        "occurred_at".into(),
        json!(event.occurred_at.format(&Rfc3339).ok()),
    );
    Value::Object(body)
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

fn placeholder_value(slot: Slot, event: &Alert) -> String {
    let opt_num = |n: Option<i64>| n.map(|v| v.to_string()).unwrap_or_default();
    match slot {
        Slot::SubjectName => event.subject.clone(),
        Slot::SubjectId => opt_num(event.subject_id),
        Slot::SubjectType => event.subject_type.clone(),
        Slot::Target => event.target.clone(),
        Slot::Status => event.status.as_str().to_string(),
        Slot::Event => event.event.as_str().to_string(),
        Slot::Message => event.message.clone().unwrap_or_default(),
        Slot::Summary => summary(event),
        Slot::Headline => headline(event),
        Slot::Url => event.url.clone().unwrap_or_default(),
        Slot::Duration => event.duration.map(format_duration).unwrap_or_default(),
        Slot::DurationSeconds => event
            .duration
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default(),
        Slot::ExpiresInDays => opt_num(event.expires_in_days),
        Slot::OccurredAt => event.occurred_at.format(&Rfc3339).unwrap_or_default(),
    }
}

/// Substitutes `{{name}}` placeholders; unknown ones are left as written.
pub fn render(template: &str, event: &Alert, escape: Escape) -> String {
    PLACEHOLDER_RE
        .replace_all(template, |caps: &regex::Captures| {
            let slot = Slot::named(caps.get(1).map_or("", |m| m.as_str()));
            match slot.map(|slot| (slot, placeholder_value(slot, event))) {
                Some((slot, v)) if v.is_empty() && escape == Escape::Json && slot.numeric() => {
                    "null".to_string()
                }
                Some((_, v)) => match escape {
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

pub fn build(config: &Value, event: &Alert) -> Result<Vec<OutRequest>, String> {
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
    use crate::notify::alert::AlertKind;
    use crate::notify::tests::event;

    #[test]
    fn default_body_keeps_legacy_fields() {
        let mut e = event(AlertKind::Recovered);
        e.duration = Some(Duration::from_secs(90));
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
        let mut e = event(AlertKind::Problem);
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
        let e = event(AlertKind::Problem);
        assert_eq!(
            render("name={{monitor_name}}&s={{status}}", &e, Escape::Form),
            "name=Payments+API&s=down"
        );
    }

    #[test]
    fn custom_headers_method_and_form_body() {
        let e = event(AlertKind::Problem);
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
