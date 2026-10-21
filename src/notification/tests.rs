//! Shared fixtures and end-to-end tests: real `send` calls against a local
//! capture server (HTTP) and a minimal fake SMTP server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use serde_json::{Value, json};
use time::macros::datetime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use super::*;
use crate::db::models::{EventKind, NotificationChannel, NotificationEvent};

/// "Payments API" (http, id 7) at a fixed time.
pub fn event(kind: EventKind) -> NotificationEvent {
    NotificationEvent {
        kind,
        monitor_id: Some(7),
        monitor_name: "Payments API".into(),
        monitor_type: "http".into(),
        target: "https://pay.example/health".into(),
        message: None,
        downtime: None,
        expires_in_days: None,
        url: None,
        occurred_at: datetime!(2026-10-19 14:03:12 UTC),
    }
}

#[derive(Debug, Clone)]
struct Captured {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

impl Captured {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Clone)]
struct Capture {
    requests: Arc<Mutex<Vec<Captured>>>,
    status: StatusCode,
    reply: &'static str,
}

async fn record(
    State(cap): State<Capture>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, &'static str) {
    cap.requests.lock().unwrap().push(Captured {
        method,
        uri,
        headers,
        body,
    });
    (cap.status, cap.reply)
}

/// Starts a server that records every request; returns its base URL.
async fn capture_server(
    status: StatusCode,
    reply: &'static str,
) -> (String, Arc<Mutex<Vec<Captured>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new().fallback(record).with_state(Capture {
        requests: requests.clone(),
        status,
        reply,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), requests)
}

fn channel(kind: &str, config: Value) -> NotificationChannel {
    NotificationChannel {
        id: 1,
        name: "Test".into(),
        kind: kind.into(),
        config: config.to_string(),
        active: true,
        is_default: false,
        created_at: "2026-10-19 00:00:00".into(),
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

#[test]
fn registry_is_consistent() {
    assert!(SUPPORTED_KINDS.len() >= 19);
    for (i, p) in PROVIDERS.iter().enumerate() {
        assert_eq!(SUPPORTED_KINDS[i], p.kind);
        assert!(
            PROVIDERS.iter().filter(|q| q.kind == p.kind).count() == 1,
            "duplicate kind {}",
            p.kind
        );
        assert!(!p.label.is_empty() && !p.icon.is_empty() && !p.fields.is_empty());
        for f in p.fields {
            assert!(
                f.key.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{}.{}",
                p.kind,
                f.key
            );
            if let FieldKind::Select(options) = f.kind
                && !f.default.is_empty()
            {
                assert!(
                    options.iter().any(|(v, _)| *v == f.default),
                    "{}.{} default",
                    p.kind,
                    f.key
                );
            }
        }
    }
    for legacy in ["discord", "slack", "telegram", "webhook"] {
        assert!(is_supported_kind(legacy));
    }
    assert!(!is_supported_kind("carrier-pigeon"));
}

#[test]
fn every_http_provider_builds_for_every_event_kind() {
    // Fill every field with something plausible and make sure no provider
    // errors or panics for any event.
    let kinds = [
        EventKind::Down,
        EventKind::Up,
        EventKind::Reminder,
        EventKind::Escalation,
        EventKind::CertExpiring,
        EventKind::DomainExpiring,
        EventKind::MaintenanceStart,
        EventKind::MaintenanceEnd,
        EventKind::Flapping,
        EventKind::Test,
    ];
    for p in PROVIDERS {
        let Transport::Http(build) = p.transport else {
            continue;
        };
        let mut config = serde_json::Map::new();
        for f in p.fields {
            let v = match f.kind {
                FieldKind::Url => json!("https://example.com/hook"),
                FieldKind::Number => json!(60),
                FieldKind::Bool => json!(false),
                FieldKind::Select(options) => json!(options[0].0),
                FieldKind::KeyValue => json!({"X-Test": "1"}),
                _ => json!("+15550001:AC!room:example.com"),
            };
            config.insert(f.key.into(), v);
        }
        let config = Value::Object(config);
        for kind in kinds {
            let mut e = event(kind);
            e.url = Some("https://vv.example/monitors/7".into());
            let result = build(&config, &e);
            assert!(result.is_ok(), "{} / {kind}: {result:?}", p.kind);
        }
    }
}

#[tokio::test]
async fn webhook_end_to_end_with_headers_and_template() {
    let (base, captured) = capture_server(StatusCode::OK, "ok").await;
    let ch = channel(
        "webhook",
        json!({"url": format!("{base}/hook"), "headers": {"Authorization": "Bearer s3cret"},
            "body_template": "{\"text\": \"{{summary}}\", \"id\": {{monitor_id}}}"}),
    );
    let mut e = event(EventKind::Down);
    e.message = Some("HTTP 503".into());
    send(&client(), &ch, &e).await.unwrap();
    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, Method::POST);
    assert_eq!(reqs[0].uri.path(), "/hook");
    assert_eq!(reqs[0].headers["authorization"], "Bearer s3cret");
    assert_eq!(reqs[0].headers["content-type"], "application/json");
    assert_eq!(
        reqs[0].json(),
        json!({"text": "\u{1f534} Payments API is down — HTTP 503", "id": 7})
    );
}

#[tokio::test]
async fn discord_and_slack_end_to_end() {
    let (base, captured) = capture_server(StatusCode::NO_CONTENT, "").await;
    let e = event(EventKind::Up);
    send(
        &client(),
        &channel(
            "discord",
            json!({"url": format!("{base}/api/webhooks/1/tok"), "username": "Vole"}),
        ),
        &e,
    )
    .await
    .unwrap();
    send(
        &client(),
        &channel("slack", json!({"url": format!("{base}/services/T/B/x")})),
        &e,
    )
    .await
    .unwrap();
    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs[0].json()["username"], "Vole");
    assert_eq!(reqs[0].json()["embeds"][0]["color"], 0x34C759);
    assert_eq!(reqs[1].uri.path(), "/services/T/B/x");
    assert!(reqs[1].json()["attachments"][0]["blocks"].is_array());
}

#[tokio::test]
async fn telegram_and_matrix_end_to_end() {
    let (base, captured) = capture_server(StatusCode::OK, "{\"ok\":true}").await;
    let e = event(EventKind::Down);
    send(
        &client(),
        &channel(
            "telegram",
            json!({"bot_token": "1:abc", "chat_id": "42", "server_url": base}),
        ),
        &e,
    )
    .await
    .unwrap();
    send(
        &client(),
        &channel(
            "matrix",
            json!({"homeserver": base, "room_id": "!r:hs", "access_token": "syt_tok"}),
        ),
        &e,
    )
    .await
    .unwrap();
    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs[0].uri.path(), "/bot1:abc/sendMessage");
    assert_eq!(reqs[0].json()["parse_mode"], "HTML");
    assert_eq!(reqs[1].method, Method::PUT);
    assert!(
        reqs[1]
            .uri
            .path()
            .starts_with("/_matrix/client/v3/rooms/%21r%3Ahs/send/m.room.message/")
    );
    assert_eq!(reqs[1].headers["authorization"], "Bearer syt_tok");
}

#[tokio::test]
async fn ntfy_end_to_end_with_basic_auth() {
    let (base, captured) = capture_server(StatusCode::OK, "{}").await;
    let ch = channel(
        "ntfy",
        json!({"server": base, "topic": "alerts", "username": "u", "password": "p"}),
    );
    send(&client(), &ch, &event(EventKind::CertExpiring))
        .await
        .unwrap();
    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs[0].uri.path(), "/");
    assert_eq!(reqs[0].headers["authorization"], "Basic dTpw");
    assert_eq!(reqs[0].json()["priority"], 4);
}

#[tokio::test]
async fn errors_include_status_and_scrub_secrets() {
    // The server echoes the token back in its error body.
    let (base, _) = capture_server(
        StatusCode::UNAUTHORIZED,
        "{\"description\":\"bad token 1:SECRETTOKEN\"}",
    )
    .await;
    let ch = channel(
        "telegram",
        json!({"bot_token": "1:SECRETTOKEN", "chat_id": "42", "server_url": base}),
    );
    let err = send(&client(), &ch, &event(EventKind::Down))
        .await
        .unwrap_err();
    assert!(err.starts_with("HTTP 401 Unauthorized"), "{err}");
    assert!(!err.contains("SECRETTOKEN"), "{err}");
}

#[tokio::test]
async fn connection_errors_never_leak_the_url() {
    // Nothing listens on port 9 (discard) on loopback in CI.
    let ch = channel(
        "telegram",
        json!({"bot_token": "1:SECRETTOKEN", "chat_id": "42", "server_url": "http://127.0.0.1:9"}),
    );
    let err = send(&client(), &ch, &event(EventKind::Down))
        .await
        .unwrap_err();
    assert!(!err.contains("SECRETTOKEN"), "{err}");
    assert!(!err.contains("127.0.0.1"), "{err}");
}

#[tokio::test]
async fn unknown_kind_and_bad_config_are_errors() {
    let e = event(EventKind::Down);
    assert!(
        send(&client(), &channel("carrier-pigeon", json!({})), &e)
            .await
            .unwrap_err()
            .contains("unsupported")
    );
    let mut bad = channel("webhook", json!({}));
    bad.config = "not json".into();
    assert!(
        send(&client(), &bad, &e)
            .await
            .unwrap_err()
            .contains("invalid channel config")
    );
}

#[tokio::test]
async fn maintenance_to_pagerduty_sends_nothing() {
    // No server needed: PagerDuty has nothing to send for maintenance.
    let ch = channel("pagerduty", json!({"routing_key": "R"}));
    send(&client(), &ch, &event(EventKind::MaintenanceStart))
        .await
        .unwrap();
}

#[test]
fn redacted_config_hides_secrets() {
    let ch = channel(
        "telegram",
        json!({"bot_token": "1:SECRET", "chat_id": "42"}),
    );
    let shown = redacted_config(&ch);
    assert_eq!(shown, json!({"bot_token": SECRET_MASK, "chat_id": "42"}));
    let hook = channel(
        "discord",
        json!({"url": "https://discord.com/api/webhooks/1/SECRET"}),
    );
    assert!(!redacted_config(&hook).to_string().contains("SECRET"));
}

/// Minimal SMTP server: accepts one message and returns its DATA.
async fn fake_smtp() -> (u16, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        write.write_all(b"220 fake ESMTP\r\n").await.unwrap();
        let mut data = String::new();
        let mut in_data = false;
        while let Ok(Some(line)) = lines.next_line().await {
            if in_data {
                if line == "." {
                    in_data = false;
                    write.write_all(b"250 queued\r\n").await.unwrap();
                } else {
                    data.push_str(&line);
                    data.push('\n');
                }
                continue;
            }
            let upper = line.to_ascii_uppercase();
            let reply: &[u8] = if upper.starts_with("EHLO") {
                b"250-fake\r\n250 8BITMIME\r\n"
            } else if upper.starts_with("DATA") {
                in_data = true;
                b"354 go ahead\r\n"
            } else if upper.starts_with("QUIT") {
                write.write_all(b"221 bye\r\n").await.unwrap();
                break;
            } else {
                b"250 ok\r\n"
            };
            write.write_all(reply).await.unwrap();
        }
        data
    });
    (port, handle)
}

#[tokio::test]
async fn email_end_to_end_over_plain_smtp() {
    let (port, server) = fake_smtp().await;
    let ch = channel(
        "email",
        json!({"host": "127.0.0.1", "port": port, "security": "none", "from": "vv@example.com", "to": "ops@example.com"}),
    );
    let mut e = event(EventKind::Down);
    e.message = Some("HTTP 503".into());
    send(&client(), &ch, &e).await.unwrap();
    let data = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(data.contains("To: ops@example.com"), "{data}");
    assert!(data.contains("multipart/alternative"));
    assert!(data.contains("Subject: "));
}

#[test]
fn public_target_drops_credentials_query_and_fragment() {
    use super::message::public_target;
    assert_eq!(
        public_target("https://bot:hunter2@api.example/health?token=abc#x"),
        "https://api.example/health"
    );
    assert_eq!(public_target("wss://u:p@ws.example/feed?k=1"), "wss://ws.example/feed");
    for unchanged in ["db.example:5432", "my-container", "192.0.2.1", "example.com"] {
        assert_eq!(public_target(unchanged), unchanged);
    }
}

#[tokio::test]
async fn webhook_never_sends_target_credentials() {
    let (base, captured) = capture_server(StatusCode::OK, "ok").await;
    let mut e = event(EventKind::Down);
    e.target = "https://bot:hunter2@pay.example/health?api_key=K3Y".into();
    for config in [
        json!({"url": format!("{base}/hook")}),
        json!({"url": format!("{base}/hook"), "body_template": "{\"t\": \"{{target}}\", \"s\": \"{{summary}}\"}"}),
    ] {
        send(&client(), &channel("webhook", config), &e).await.unwrap();
    }
    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    for r in &reqs {
        let body = String::from_utf8_lossy(&r.body);
        assert!(body.contains("https://pay.example/health"), "{body}");
        assert!(!body.contains("hunter2") && !body.contains("K3Y") && !body.contains("bot:"), "{body}");
    }
}

#[test]
fn every_required_field_has_its_own_message() {
    for p in PROVIDERS {
        for f in p.fields.iter().filter(|f| f.required && !matches!(f.kind, FieldKind::Bool | FieldKind::Select(_))) {
            let key = format!("notif.{}.{}.required", p.kind, f.key);
            assert!(!crate::i18n::t_or(&key, "").is_empty(), "missing {key}");
        }
    }
    let errors = schema::validate(&email::SCHEMA, &json!({})).unwrap_err();
    let host = errors.iter().find(|(k, _)| k == "host").map(|(_, m)| m.as_str());
    assert_eq!(host, Some("Enter the SMTP server"));
    let from = errors.iter().find(|(k, _)| k == "from").map(|(_, m)| m.as_str());
    assert_eq!(from, Some("Enter a sender address"));
}

/// Prints the `[notif.<kind>]` blocks of `locales/en.toml` for providers that
/// don't have one yet (run with `--ignored --nocapture` after adding a
/// provider, paste the output into the file, then translate it).
#[test]
#[ignore = "a helper for writing locale files"]
fn print_missing_provider_locale_blocks() {
    let en = include_str!("../../locales/en.toml");
    for p in PROVIDERS {
        if en.contains(&format!("[notif.{}]", p.kind)) {
            continue;
        }
        println!("[notif.{}]\ndesc = {:?}", p.kind, p.description);
        let mut required = Vec::new();
        for f in p.fields {
            println!("{}.label = {:?}", f.key, f.label);
            if !f.help.is_empty() {
                println!("{}.help = {:?}", f.key, f.help);
            }
            if let FieldKind::Select(options) = f.kind {
                for (value, label) in options {
                    println!("{}.opt_{value} = {:?}", f.key, label);
                }
            }
            if f.required && !matches!(f.kind, FieldKind::Bool | FieldKind::Select(_)) {
                required.push(format!("{}.required = {:?}", f.key, format!("Enter the {}", f.label.to_lowercase())));
            }
        }
        for line in required {
            println!("{line}");
        }
        println!();
    }
}

// ---------------------------------------------------------------------------
// Chat and messaging providers added later: end to end against a local server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn zulip_nextcloud_waha_and_evolution_end_to_end() {
    let (base, captured) = capture_server(StatusCode::OK, "{}").await;
    let e = event(EventKind::Down);
    send(&client(), &channel("zulip", json!({"site": base, "bot_email": "bot@org.example", "api_key": "ZKEY", "stream": "ops", "topic": "Alerts"})), &e)
        .await
        .unwrap();
    send(&client(), &channel("nextcloudtalk", json!({"host": base, "conversation_token": "room1", "bot_secret": "b".repeat(40)})), &e)
        .await
        .unwrap();
    send(&client(), &channel("waha", json!({"server": base, "chat_id": "4712345678", "api_key": "WKEY"})), &e).await.unwrap();
    send(&client(), &channel("evolution", json!({"server": base, "instance": "vole", "api_key": "EKEY", "number": "4712345678"})), &e)
        .await
        .unwrap();
    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs.len(), 4);

    assert_eq!((reqs[0].method.clone(), reqs[0].uri.path()), (Method::POST, "/api/v1/messages"));
    assert!(reqs[0].headers["authorization"].to_str().unwrap().starts_with("Basic "));
    let form = String::from_utf8_lossy(&reqs[0].body).to_string();
    assert!(form.contains("type=stream") && form.contains("to=ops") && form.contains("topic=Alerts"), "{form}");

    assert_eq!(reqs[1].uri.path(), "/ocs/v2.php/apps/spreed/api/v1/bot/room1/message");
    let message = reqs[1].json()["message"].as_str().unwrap().to_string();
    let random = reqs[1].headers["x-nextcloud-talk-bot-random"].to_str().unwrap().to_string();
    let expected: String = http::hmac_sha256("b".repeat(40).as_bytes(), format!("{random}{message}").as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(reqs[1].headers["x-nextcloud-talk-bot-signature"], expected.as_str());

    assert_eq!(reqs[2].uri.path(), "/api/sendText");
    assert_eq!((reqs[2].headers["x-api-key"].to_str().unwrap(), reqs[2].json()["chatId"].as_str()), ("WKEY", Some("4712345678@c.us")));
    assert_eq!(reqs[3].uri.path(), "/message/sendText/vole");
    assert_eq!(reqs[3].headers["apikey"], "EKEY");
}

#[tokio::test]
async fn a_failing_zulip_server_cannot_leak_the_bot_key() {
    let (base, _) = capture_server(StatusCode::UNAUTHORIZED, "{\"msg\":\"bad key ZKEY-SECRET\"}").await;
    let ch = channel("zulip", json!({"site": base, "bot_email": "bot@org.example", "api_key": "ZKEY-SECRET", "stream": "ops"}));
    let err = send(&client(), &ch, &event(EventKind::Down)).await.unwrap_err();
    assert!(err.contains("401") && !err.contains("ZKEY-SECRET"), "{err}");
}
