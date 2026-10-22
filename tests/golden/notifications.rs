//! Outgoing notifications: for every provider, the exact requests its
//! `build` produces (method, URL, headers, body) for a fixed "down" and
//! "back up" event, from the config the settings form would store. Email
//! freezes the built MIME message. A generic `Alert` refactor must produce
//! byte-identical output.

use std::time::Duration;

use insta::assert_snapshot;
use serde_json::Value;
use time::OffsetDateTime;
use vigilant_vole::db::models::{EventKind, NotificationEvent};
use vigilant_vole::i18n::{self, Lang};
use vigilant_vole::notification::http::{OutBody, OutRequest};
use vigilant_vole::notification::schema::{FieldKind, parse_form};
use vigilant_vole::notification::{
    PROVIDERS, ProviderSchema, Transport, dingtalk, email, feishu, http::hmac_sha256,
};

use crate::norm;

/// "Payments API" (http, id 7) at 2026-10-19 14:03:12 UTC.
fn event(kind: EventKind) -> NotificationEvent {
    let (message, downtime) = match kind {
        EventKind::Up => (Some("200 OK".to_string()), Some(Duration::from_secs(724))),
        _ => (Some("HTTP 503 (after 3 retries)".to_string()), None),
    };
    NotificationEvent {
        kind,
        monitor_id: Some(7),
        monitor_name: "Payments API".into(),
        monitor_type: "http".into(),
        target: "https://pay.example/health".into(),
        message,
        downtime,
        expires_in_days: None,
        url: Some("https://vv.example/monitors/7".into()),
        occurred_at: OffsetDateTime::from_unix_timestamp(1_792_418_592).unwrap(),
    }
}

/// A form value for a field: its default, else its placeholder (without the
/// trailing "…"), else a made-up value of the right kind.
fn sample(
    p: &ProviderSchema,
    key: &str,
    kind: FieldKind,
    placeholder: &str,
    default: &str,
) -> String {
    let ph = placeholder.trim_end_matches('…').trim();
    if !default.is_empty() {
        return default.to_string();
    }
    match kind {
        FieldKind::Bool => "true".into(),
        FieldKind::Select(options) => options.first().map(|o| o.0).unwrap_or("").into(),
        FieldKind::Number => ph
            .parse::<i64>()
            .map(|n| n.to_string())
            .unwrap_or_else(|_| "1".into()),
        FieldKind::KeyValue => r#"{"X-Golden": "yes"}"#.into(),
        FieldKind::Url if ph.starts_with("http") || ph.starts_with("ws") => ph.into(),
        FieldKind::Url => format!("https://{}.example.test/{key}", p.kind),
        FieldKind::Secret if !ph.is_empty() => ph.into(),
        FieldKind::Secret => format!("secret-{key}"),
        _ if !ph.is_empty() => ph.into(),
        _ => format!("{key}-value"),
    }
}

/// The stored config for a form with the required fields (and those with a
/// default) filled in, or with every field filled in.
fn config(p: &ProviderSchema, full: bool) -> Value {
    let mut pairs = Vec::new();
    for f in p.fields {
        if !(full || f.required || !f.default.is_empty()) {
            continue;
        }
        pairs.push((
            f.form_name(),
            sample(p, f.key, f.kind, &f.placeholder_text(), &f.default_value()),
        ));
    }
    parse_form(p, &pairs).config
}

fn render(r: &OutRequest) -> String {
    let mut out = format!("{} {}\n", r.method, r.url);
    for (k, v) in &r.headers {
        out.push_str(&format!("{k}: {v}\n"));
    }
    match &r.body {
        OutBody::Empty => out.push_str("(no body)\n"),
        OutBody::Json(v) => {
            out.push_str(&format!(
                "(json)\n{}\n",
                serde_json::to_string_pretty(v).unwrap()
            ));
        }
        OutBody::Form(fields) => {
            out.push_str("(form)\n");
            for (k, v) in fields {
                out.push_str(&format!("{k}={v}\n"));
            }
        }
        OutBody::Raw { content_type, body } => {
            out.push_str(&format!("(raw {content_type})\n{body}\n"));
        }
    }
    out
}

/// The MIME message with its random parts (boundary, Message-ID, Date)
/// replaced.
fn render_email(config: &Value, ev: &NotificationEvent) -> String {
    let msg = match email::build_message(config, ev) {
        Ok(m) => m,
        Err(e) => return format!("error: {e}\n"),
    };
    let raw = String::from_utf8_lossy(&msg.formatted()).into_owned();
    let mut out = raw.replace("\r\n", "\n");
    let boundary = regex::Regex::new(r#"boundary="([^"]+)""#).unwrap();
    let found: Vec<String> = boundary
        .captures_iter(&out)
        .map(|c| c[1].to_string())
        .collect();
    for (i, b) in found.iter().enumerate() {
        out = out.replace(b, &format!("<boundary-{i}>"));
    }
    let date = regex::Regex::new(r"(?m)^Date: .*$").unwrap();
    let id = regex::Regex::new(r"(?m)^Message-ID: .*$").unwrap();
    let out = date.replace_all(&out, "Date: <date>");
    id.replace_all(&out, "Message-ID: <id>").into_owned()
}

/// Signatures over the current time or a random value are checked against
/// the provider's own scheme, then replaced by what they stand for; the
/// process-wide Matrix transaction counter becomes `<seq>`.
fn stabilise(kind: &str, config: &Value, requests: &mut [OutRequest]) {
    let secret = |key: &str| config.get(key).and_then(Value::as_str).unwrap_or("");
    let re = regex::Regex::new(r"timestamp=(\d+)&sign=([^&]+)").unwrap();
    let txn = regex::Regex::new(r"/vv(\d+)-\d+$").unwrap();
    for r in requests {
        match kind {
            "matrix" => {
                let url = r.url.clone();
                let c = txn.captures(&url).expect("matrix txn id");
                // vv<event time in unix nanoseconds>-<counter>
                assert_eq!(&c[1], "1792418592000000000", "matrix txn id");
                r.url = txn.replace(&url, "/vv$1-<seq>").into_owned();
            }
            "dingtalk" => {
                let url = r.url.clone();
                if let Some(c) = re.captures(&url) {
                    let ts: i64 = c[1].parse().unwrap();
                    assert_eq!(&c[2], dingtalk::sign(ts, secret("secret")), "dingtalk sign");
                    r.url = re
                        .replace(&url, "timestamp=<unix-ms>&sign=<sign(timestamp, secret)>")
                        .into_owned();
                }
            }
            "feishu" => {
                if let OutBody::Json(body) = &mut r.body
                    && let Some(ts) = body.get("timestamp").and_then(Value::as_str)
                {
                    let ts: i64 = ts.parse().unwrap();
                    assert_eq!(
                        body["sign"],
                        feishu::sign(ts, secret("secret")),
                        "feishu sign"
                    );
                    body["timestamp"] = "<unix-seconds>".into();
                    body["sign"] = "<sign(timestamp, secret)>".into();
                }
            }
            "nextcloudtalk" => {
                let message = r
                    .json()
                    .and_then(|b| b["message"].as_str())
                    .unwrap_or("")
                    .to_string();
                let random = r
                    .header_value("X-Nextcloud-Talk-Bot-Random")
                    .unwrap_or("")
                    .to_string();
                assert_eq!(random.len(), 64, "nextcloudtalk random");
                let expected = hex::encode(hmac_sha256(
                    secret("bot_secret").as_bytes(),
                    format!("{random}{message}").as_bytes(),
                ));
                for (name, value) in &mut r.headers {
                    if name == "X-Nextcloud-Talk-Bot-Random" {
                        *value = "<random 32 bytes, hex>".into();
                    } else if name == "X-Nextcloud-Talk-Bot-Signature" {
                        assert_eq!(*value, expected, "nextcloudtalk signature");
                        *value = "<hex(hmac_sha256(secret, random + message))>".into();
                    }
                }
            }
            _ => {}
        }
    }
}

fn build(p: &ProviderSchema, config: &Value, ev: &NotificationEvent) -> String {
    match p.transport {
        Transport::Http(build) => match build(config, ev) {
            Ok(requests) if requests.is_empty() => "(nothing to send)\n".into(),
            Ok(mut requests) => {
                stabilise(p.kind, config, &mut requests);
                requests
                    .iter()
                    .map(render)
                    .collect::<Vec<_>>()
                    .join("---\n")
            }
            Err(e) => format!("error: {e}\n"),
        },
        Transport::Smtp => render_email(config, ev),
    }
}

#[test]
fn provider_requests() {
    let mut kinds = Vec::new();
    for p in PROVIDERS {
        kinds.push(p.kind);
        let out = i18n::sync_scope(Lang::EN, || {
            let minimal = config(p, false);
            let full = config(p, true);
            let mut out = format!(
                "provider: {} ({})\n\n## minimal config\n{}\n\n",
                p.kind,
                p.label,
                serde_json::to_string_pretty(&minimal).unwrap()
            );
            out.push_str(&format!(
                "### down\n{}\n### up\n{}\n",
                build(p, &minimal, &event(EventKind::Down)),
                build(p, &minimal, &event(EventKind::Up)),
            ));
            out.push_str(&format!(
                "## full config\n{}\n\n### down\n{}\n### up\n{}",
                serde_json::to_string_pretty(&full).unwrap(),
                build(p, &full, &event(EventKind::Down)),
                build(p, &full, &event(EventKind::Up)),
            ));
            out
        });
        assert_snapshot!(format!("notify_{}", p.kind), norm::text(&out));
    }
    assert_snapshot!("notify_providers", kinds.join("\n"));
}
