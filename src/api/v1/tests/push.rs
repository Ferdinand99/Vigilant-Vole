//! The Hedgerow app channel end to end: a channel, a device registered with a
//! read-only token, a monitor going down, the push the (fake) relay gets and
//! the text the app fetches with its ref.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use super::*;
use crate::db::models::CheckOutcome;

/// What the fake relay saw.
#[derive(Default)]
struct Seen {
    registrations: usize,
    pushes: Vec<Value>,
}

/// A relay that takes every registration (difficulty 0) and accepts every
/// handle. Returns its URL.
async fn fake_relay() -> (String, Arc<Mutex<Seen>>) {
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let app = Router::new()
        .route(
            "/v1/challenge",
            get(|| async {
                Json(json!({
                    "challenge": "challenge-1",
                    "difficulty": 0,
                    "expires_at": "2099-01-01T00:00:00Z",
                }))
            }),
        )
        .route(
            "/v1/instances",
            post(|State(seen): State<Arc<Mutex<Seen>>>| async move {
                seen.lock().unwrap().registrations += 1;
                (
                    axum::http::StatusCode::CREATED,
                    Json(json!({
                        "instance_credential": "hrk_registered0",
                        "tier": "new",
                        "limits": {"sends_per_hour": 600, "handles_per_request": 25, "unknown_handles_per_hour": 20},
                    })),
                )
            }),
        )
        .route(
            "/v1/push",
            post(
                |State(seen): State<Arc<Mutex<Seen>>>, Json(body): Json<Value>| async move {
                    let n = body["handles"].as_array().map_or(0, Vec::len);
                    seen.lock().unwrap().pushes.push(body);
                    let results: Vec<Value> =
                        (0..n).map(|_| json!({"status": "accepted"})).collect();
                    Json(json!({ "results": results }))
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, seen)
}

const HANDLE: &str = "hrh_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

async fn add_hedgerow_channel(h: &Harness, name: &str, extra: Value) -> (StatusCode, Value) {
    let mut body = json!({"name": name, "kind": "hedgerow", "config": {}});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    h.json(
        Method::POST,
        "/api/v1/notification-channels",
        &h.editor,
        Some(body),
    )
    .await
}

#[tokio::test]
async fn a_monitor_going_down_pushes_a_ref_and_the_ref_opens_the_text() {
    let h = harness().await;
    let (relay_url, seen) = fake_relay().await;
    // Point the push client at the fake relay, and say who we are.
    h.state
        .with_conn(move |c| {
            crate::kit::db::settings::set(c, crate::kit::push::relay::SETTING_RELAY_URL, &relay_url)
        })
        .await
        .unwrap();
    let (status, _) = h
        .json(
            Method::PATCH,
            "/api/v1/settings",
            &h.admin,
            Some(json!({"base_url": "https://vole.test", "language": "en"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    crate::kit::push::install(&h.state.core());

    // Push is off until there is a Hedgerow app channel.
    let register = json!({ "handle": HANDLE });
    let (status, body) = h
        .json(
            Method::POST,
            "/api/v1/push/devices",
            &h.admin_read,
            Some(register.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "push_disabled");

    // The channel has no settings of its own; link it to the monitor.
    let (status, channel) = add_hedgerow_channel(&h, "Phones", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{channel}");
    assert_eq!(channel["kind"], "hedgerow");
    let channel_id = channel["id"].as_i64().unwrap();
    let (status, monitor) = h
        .json(
            Method::POST,
            "/api/v1/monitors",
            &h.editor,
            Some(json!({
                "name": "Payments",
                "monitor_type": "push",
                "retries": 0,
                "notifications": [channel_id],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{monitor}");
    let monitor_id = monitor["id"].as_i64().unwrap();

    // A read-only token registers its own device.
    let (status, device) = h
        .json(
            Method::POST,
            "/api/v1/push/devices",
            &h.admin_read,
            Some(register),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{device}");

    // Up, then down: one alert.
    let scheduler = &h.state.scheduler;
    scheduler
        .ingest_external(monitor_id, CheckOutcome::up(5), None)
        .await;
    scheduler
        .ingest_external(monitor_id, CheckOutcome::down("connection refused"), None)
        .await;
    for _ in 0..100 {
        if !seen.lock().unwrap().pushes.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Give a wrong second push time to show up.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (registrations, mut pushes) = {
        let seen = seen.lock().unwrap();
        (seen.registrations, seen.pushes.clone())
    };
    assert_eq!(registrations, 1, "the server registered itself once");
    assert_eq!(pushes.len(), 1, "one push for one alert");
    let push = pushes.remove(0);
    assert_eq!(push["kind"], "alert");
    assert_eq!(push["handles"], json!([HANDLE]));
    let reference = push["ref"].as_str().expect("a ref").to_string();
    assert!(!reference.is_empty());
    // The relay and Apple get a ref and nothing else of the alert.
    let wire = push.to_string();
    for secret in ["Payments", "is down", "connection refused", "vole.test"] {
        assert!(!wire.contains(secret), "{secret} went to the relay: {wire}");
    }

    // The app fetches the text from this server, with a read-only token.
    let (status, text) = h
        .json(
            Method::GET,
            &format!("/api/v1/push/events/{reference}"),
            &h.admin_read,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let title = text["title"].as_str().unwrap();
    assert!(title.contains("Payments is down"), "{title}");
    assert_eq!(text["kind"], "problem");
    assert_eq!(text["level"], "critical");
    let body = text["body"].as_str().unwrap();
    assert!(body.contains("connection refused"), "{body}");
    assert!(
        !title.contains("vole.test") && !body.contains("vole.test"),
        "the deep link isn't stored: {title} {body}"
    );
    // Not without a token.
    let (status, _) = h
        .json(
            Method::GET,
            &format!("/api/v1/push/events/{reference}"),
            "",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // An unknown ref is a plain 404.
    let (status, _) = h
        .json(Method::GET, "/api/v1/push/events/nope", &h.admin_read, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn only_one_hedgerow_channel_can_be_active() {
    let h = harness().await;
    let (status, first) = add_hedgerow_channel(&h, "Phones", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let first_id = first["id"].as_i64().unwrap();

    // A second active one is refused, and says which one is in the way.
    let (status, refused) = add_hedgerow_channel(&h, "More phones", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"]["code"], "conflict");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Phones"),
        "{refused}"
    );

    // A paused one is fine, but can't be switched on while the first is.
    let (status, second) = add_hedgerow_channel(&h, "More phones", json!({"active": false})).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    let second_id = second["id"].as_i64().unwrap();
    let uri = format!("/api/v1/notification-channels/{second_id}");
    let (status, body) = h
        .json(
            Method::PATCH,
            &uri,
            &h.editor,
            Some(json!({"active": true})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // Editing the active one keeps working.
    let (status, body) = h
        .json(
            Method::PATCH,
            &format!("/api/v1/notification-channels/{first_id}"),
            &h.editor,
            Some(json!({"name": "Our phones"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Once the first is gone, the second can take over.
    let (status, _) = h
        .json(
            Method::DELETE,
            &format!("/api/v1/notification-channels/{first_id}"),
            &h.editor,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = h
        .json(
            Method::PATCH,
            &uri,
            &h.editor,
            Some(json!({"active": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], true);
}

#[tokio::test]
async fn the_hedgerow_channel_is_listed_without_fields_and_follows_the_defaults() {
    let h = harness().await;
    let (_, providers) = h
        .json(
            Method::GET,
            "/api/v1/notification-providers",
            &h.viewer,
            None,
        )
        .await;
    let hedgerow = providers["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["kind"] == "hedgerow")
        .expect("the provider is listed");
    assert_eq!(hedgerow["fields"], json!([]));

    // A default channel is linked to monitors created later, as any other.
    let (status, channel) = add_hedgerow_channel(&h, "Phones", json!({"is_default": true})).await;
    assert_eq!(status, StatusCode::CREATED, "{channel}");
    let channel_id = channel["id"].as_i64().unwrap();
    let (_, monitor) = h
        .json(
            Method::POST,
            "/api/v1/monitors",
            &h.editor,
            Some(json!({"name": "Later", "monitor_type": "push"})),
        )
        .await;
    assert_eq!(
        monitor["notifications"],
        json!([{"channel_id": channel_id, "escalation": false}]),
        "{monitor}"
    );
}

#[tokio::test]
async fn a_read_token_can_register_and_remove_its_own_device_only() {
    let h = harness().await;
    let (status, _) = add_hedgerow_channel(&h, "Phones", json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, device) = h
        .json(
            Method::POST,
            "/api/v1/push/devices",
            &h.admin_read,
            Some(json!({ "handle": HANDLE })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{device}");
    let id = device["id"].as_i64().unwrap();
    // Another user can't remove it.
    let (status, _) = h
        .json(
            Method::DELETE,
            &format!("/api/v1/push/devices/{id}"),
            &h.viewer,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h
        .json(
            Method::DELETE,
            &format!("/api/v1/push/devices/{id}"),
            &h.admin_read,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // The read-only token still can't change anything else.
    let (status, _) = h
        .json(
            Method::POST,
            "/api/v1/tags",
            &h.admin_read,
            Some(json!({"name": "nope"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn maintenance_notices_go_to_the_hedgerow_channel_too() {
    use crate::db::models::{EventKind, NotificationEvent};

    let h = harness().await;
    let (relay_url, seen) = fake_relay().await;
    h.state
        .with_conn(move |c| {
            crate::kit::db::settings::set(c, crate::kit::push::relay::SETTING_RELAY_URL, &relay_url)
        })
        .await
        .unwrap();
    let (status, channel) = add_hedgerow_channel(&h, "Phones", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{channel}");
    let channel_id = channel["id"].as_i64().unwrap();
    let (_, monitor) = h
        .json(
            Method::POST,
            "/api/v1/monitors",
            &h.editor,
            Some(
                json!({"name": "Payments", "monitor_type": "push", "notifications": [channel_id]}),
            ),
        )
        .await;
    let monitor_id = monitor["id"].as_i64().unwrap();
    let (status, _) = h
        .json(
            Method::POST,
            "/api/v1/push/devices",
            &h.admin_read,
            Some(json!({ "handle": HANDLE })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // The channels a maintenance window notifies are the monitors' own.
    let channels = h
        .state
        .with_conn(move |c| {
            crate::db::notifications::list_channels_for_monitor(c, monitor_id, false)
        })
        .await
        .unwrap();
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].kind, "hedgerow");

    let event = NotificationEvent {
        kind: EventKind::MaintenanceStart,
        monitor_id: None,
        monitor_name: "Database upgrade".into(),
        monitor_type: String::new(),
        target: String::new(),
        message: Some("Affects Payments".into()),
        downtime: None,
        expires_in_days: None,
        url: Some("https://vole.test/maintenance/1/edit".into()),
        occurred_at: time::OffsetDateTime::now_utc(),
    };
    let ctx = std::sync::Arc::new(
        crate::kit::push::relay::Context::new(&h.state.core()).expect("a context"),
    );
    crate::kit::push::relay::scoped(
        ctx,
        crate::notification::send_all(&h.state.http, &h.state.session_key, &channels, &event),
    )
    .await;

    let reference = {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.pushes.len(), 1);
        seen.pushes[0]["ref"].as_str().unwrap().to_string()
    };
    let (status, text) = h
        .json(
            Method::GET,
            &format!("/api/v1/push/events/{reference}"),
            &h.admin_read,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(text["kind"], "maintenance_started");
    assert!(
        text["title"].as_str().unwrap().contains("Database upgrade"),
        "{text}"
    );
}
