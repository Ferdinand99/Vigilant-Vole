//! Router-level tests for the REST API, config endpoints, docs and backup page.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::{Body as ReqBody, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response as HttpResponse;
use axum_extra::extract::cookie::Key;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::api::build_router;
use crate::auth::{self, Role};
use crate::config::Config;
use crate::db::api_tokens::{self as db_tokens, TokenScope};
use crate::db::users as db_users;
use crate::monitor::Scheduler;

const PASSWORD: &str = "correct horse battery";
static PASSWORD_HASH: LazyLock<String> = LazyLock::new(|| auth::try_hash_password(PASSWORD).unwrap());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TestDir(PathBuf);

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An app with an admin, an editor and a viewer, and tokens for each.
struct Harness {
    app: Router,
    state: AppState,
    admin: String,
    admin_read: String,
    editor: String,
    viewer: String,
    _dir: TestDir,
}

async fn token(state: &AppState, user_id: i64, scope: TokenScope) -> String {
    let t = auth::tokens::generate();
    let (prefix, hash) = (auth::tokens::display_prefix(&t), auth::tokens::hash(&t));
    state.with_conn(move |c| db_tokens::insert(c, user_id, "test", &prefix, &hash, scope, None)).await.unwrap();
    t
}

async fn harness() -> Harness {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("vv-v1-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pool = crate::db::build_pool(&dir.join("test.db"));
    crate::db::run_migrations(&pool, &dir.join("backups")).await;
    let (update_tx, _) = tokio::sync::broadcast::channel(16);
    let http = reqwest::Client::new();
    let scheduler = Scheduler::new(pool.clone(), update_tx.clone(), http.clone());
    let state = AppState {
        db: pool,
        scheduler,
        session_key: Key::generate(),
        update_tx,
        http,
        config: Arc::new(Config { data_dir: dir.clone(), bind_addr: "127.0.0.1:0".into() }),
    };
    let mut ids = Vec::new();
    for (name, role) in [("admin", Role::Admin), ("editor", Role::Editor), ("viewer", Role::Viewer)] {
        let hash = PASSWORD_HASH.clone();
        ids.push(state.with_conn(move |c| db_users::insert_user(c, name, &hash, role)).await.unwrap());
    }
    let admin = token(&state, ids[0], TokenScope::Write).await;
    let admin_read = token(&state, ids[0], TokenScope::Read).await;
    let editor = token(&state, ids[1], TokenScope::Write).await;
    let viewer = token(&state, ids[2], TokenScope::Write).await;
    Harness { app: build_router(state.clone()), state, admin, admin_read, editor, viewer, _dir: TestDir(dir) }
}

impl Harness {
    async fn call(&self, method: Method, uri: &str, token: &str, body: Option<Value>) -> HttpResponse {
        let mut req = Request::builder().method(method).uri(uri).header(header::HOST, "vole.test");
        if !token.is_empty() {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                ReqBody::from(v.to_string())
            }
            None => ReqBody::empty(),
        };
        self.app.clone().oneshot(req.body(body).unwrap()).await.unwrap()
    }

    async fn json(&self, method: Method, uri: &str, token: &str, body: Option<Value>) -> (StatusCode, Value) {
        let res = self.call(method, uri, token, body).await;
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn raw(&self, method: Method, uri: &str, token: &str, content_type: &str, body: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "vole.test")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, content_type)
            .body(ReqBody::from(body.to_string()))
            .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn http_monitor(name: &str) -> Value {
    json!({ "name": name, "monitor_type": "http", "target": "https://example.com", "interval_seconds": 60 })
}

// ---------------------------------------------------------------------------
// Auth and roles
// ---------------------------------------------------------------------------

#[tokio::test]
async fn requests_without_credentials_are_rejected() {
    let h = harness().await;
    assert_eq!(h.call(Method::GET, "/api/v1/monitors", "", None).await.status(), StatusCode::UNAUTHORIZED);
    let bogus = format!("vv_{}", "a".repeat(43));
    assert_eq!(h.call(Method::GET, "/api/v1/monitors", &bogus, None).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn read_tokens_can_read_but_never_write() {
    let h = harness().await;
    assert_eq!(h.json(Method::GET, "/api/v1/monitors", &h.admin_read, None).await.0, StatusCode::OK);
    let (status, body) = h.json(Method::POST, "/api/v1/monitors", &h.admin_read, Some(http_monitor("x"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden");
    let (_, me) = h.json(Method::GET, "/api/v1/me", &h.admin_read, None).await;
    assert_eq!((me["role"].as_str(), me["auth"].as_str(), me["token_scope"].as_str()), (Some("viewer"), Some("token"), Some("read")));
}

#[tokio::test]
async fn viewers_get_403_and_admin_only_routes_need_admins() {
    let h = harness().await;
    assert_eq!(h.json(Method::POST, "/api/v1/tags", &h.viewer, Some(json!({"name": "x"}))).await.0, StatusCode::FORBIDDEN);
    for uri in ["/api/v1/settings", "/api/v1/agents"] {
        assert_eq!(h.json(Method::GET, uri, &h.editor, None).await.0, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(h.json(Method::GET, uri, &h.admin, None).await.0, StatusCode::OK, "{uri}");
    }
    assert_eq!(h.json(Method::POST, "/api/v1/config?dry_run=1", &h.editor, Some(json!({}))).await.0, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Monitors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn monitor_crud_round_trip() {
    let h = harness().await;
    let mut body = http_monitor("Website");
    body["tags"] = json!(["production", {"name": "env", "value": "prod"}]);
    body["basic_auth_user"] = json!("bot");
    body["basic_auth_pass"] = json!("hunter2");
    let (status, created) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_i64().unwrap();
    assert_eq!(created["status"], "unknown");
    assert_eq!(created["basic_auth_pass"], crate::notification::SECRET_MASK);
    assert_eq!(created["tags"].as_array().unwrap().len(), 2);

    let (status, got) = h.json(Method::GET, &format!("/api/v1/monitors/{id}"), &h.viewer, None).await;
    assert_eq!((status, got["name"].as_str()), (StatusCode::OK, Some("Website")));

    // PATCH changes only what's sent; the masked password is kept.
    let (status, patched) = h
        .json(Method::PATCH, &format!("/api/v1/monitors/{id}"), &h.editor, Some(json!({"interval_seconds": 30, "basic_auth_pass": crate::notification::SECRET_MASK})))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["interval_seconds"], 30);
    assert_eq!(patched["target"], "https://example.com");
    let stored = h.state.with_conn(move |c| crate::db::monitors::get_monitor(c, id)).await.unwrap().unwrap();
    assert_eq!(stored.basic_auth_pass.as_deref(), Some("hunter2"));

    // PUT replaces: fields left out go back to defaults; tags are kept.
    let (status, put) = h.json(Method::PUT, &format!("/api/v1/monitors/{id}"), &h.editor, Some(http_monitor("Website 2"))).await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!((put["name"].as_str(), put["interval_seconds"].as_i64(), put["basic_auth_user"].is_null()), (Some("Website 2"), Some(60), true));
    assert_eq!(put["tags"].as_array().unwrap().len(), 2);

    let (status, paused) = h.json(Method::POST, &format!("/api/v1/monitors/{id}/pause"), &h.editor, None).await;
    assert_eq!((status, paused["status"].as_str()), (StatusCode::OK, Some("paused")));
    let (status, err) = h.json(Method::POST, &format!("/api/v1/monitors/{id}/check"), &h.editor, None).await;
    assert_eq!((status, err["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")));
    assert_eq!(h.json(Method::POST, &format!("/api/v1/monitors/{id}/resume"), &h.editor, None).await.1["active"], true);

    let (status, filtered) = h.json(Method::GET, "/api/v1/monitors?tag=PRODUCTION&q=website", &h.viewer, None).await;
    assert_eq!((status, filtered["data"].as_array().unwrap().len()), (StatusCode::OK, 1));
    assert_eq!(h.json(Method::GET, "/api/v1/monitors?type=ping", &h.viewer, None).await.1["data"].as_array().unwrap().len(), 0);

    for sub in ["heartbeats", "uptime?days=7", "events"] {
        assert_eq!(h.json(Method::GET, &format!("/api/v1/monitors/{id}/{sub}"), &h.viewer, None).await.0, StatusCode::OK, "{sub}");
    }
    let (_, uptime) = h.json(Method::GET, &format!("/api/v1/monitors/{id}/uptime?days=7"), &h.viewer, None).await;
    assert_eq!(uptime["daily"].as_array().unwrap().len(), 7);

    assert_eq!(h.call(Method::DELETE, &format!("/api/v1/monitors/{id}"), &h.editor, None).await.status(), StatusCode::NO_CONTENT);
    let (status, gone) = h.json(Method::GET, &format!("/api/v1/monitors/{id}"), &h.viewer, None).await;
    assert_eq!((status, gone["error"]["code"].as_str()), (StatusCode::NOT_FOUND, Some("not_found")));
}

#[tokio::test]
async fn monitor_validation_errors_name_their_fields() {
    let h = harness().await;
    let (status, body) = h
        .json(Method::POST, "/api/v1/monitors", &h.editor, Some(json!({"name": "", "monitor_type": "http", "target": "example.com"})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "validation_failed");
    assert!(body["error"]["fields"]["target"].is_string() && body["error"]["fields"]["name"].is_string(), "{body}");

    let (status, body) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(json!({"name": "x", "intervall": 5}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"]["fields"]["intervall"].is_string());

    let (status, body) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(json!({"name": "x", "monitor_type": "nope"}))).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("validation_failed")));

    let (status, body) = h.raw(Method::POST, "/api/v1/monitors", &h.editor, "application/json", "{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(h.raw(Method::POST, "/api/v1/monitors", &h.editor, "text/plain", "{}").await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(h.json(Method::GET, "/api/v1/monitors/abc", &h.viewer, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.json(Method::GET, "/api/v1/monitors?status=sideways", &h.viewer, None).await.0, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn lists_paginate_with_cursors_and_etags() {
    let h = harness().await;
    for name in ["A", "B", "C"] {
        assert_eq!(h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(http_monitor(name))).await.0, StatusCode::CREATED);
    }
    let (_, first) = h.json(Method::GET, "/api/v1/monitors?limit=2", &h.viewer, None).await;
    assert_eq!(first["data"].as_array().unwrap().len(), 2);
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    let (_, second) = h.json(Method::GET, &format!("/api/v1/monitors?limit=2&cursor={cursor}"), &h.viewer, None).await;
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    assert!(second["next_cursor"].is_null());
    assert_eq!(second["data"][0]["name"], "C");

    let res = h.call(Method::GET, "/api/v1/monitors", &h.viewer, None).await;
    let etag = res.headers()[header::ETAG].to_str().unwrap().to_string();
    let req = Request::builder()
        .uri("/api/v1/monitors")
        .header(header::AUTHORIZATION, format!("Bearer {}", h.viewer))
        .header(header::IF_NONE_MATCH, &etag)
        .body(ReqBody::empty())
        .unwrap();
    assert_eq!(h.app.clone().oneshot(req).await.unwrap().status(), StatusCode::NOT_MODIFIED);
}

// ---------------------------------------------------------------------------
// Tags, channels, status pages, maintenance, settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tags_crud_and_assignment() {
    let h = harness().await;
    let (status, tag) = h.json(Method::POST, "/api/v1/tags", &h.editor, Some(json!({"name": "prod", "color": "#00ff00"}))).await;
    assert_eq!((status, tag["color"].as_str()), (StatusCode::CREATED, Some("green")));
    assert_eq!(h.json(Method::POST, "/api/v1/tags", &h.editor, Some(json!({"name": "PROD"}))).await.0, StatusCode::CONFLICT);
    assert_eq!(h.json(Method::POST, "/api/v1/tags", &h.editor, Some(json!({"name": "x", "color": "plaid"}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let tid = tag["id"].as_i64().unwrap();
    let (_, m) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(http_monitor("M"))).await;
    let mid = m["id"].as_i64().unwrap();
    let (status, m) = h.json(Method::PUT, &format!("/api/v1/monitors/{mid}/tags"), &h.editor, Some(json!([{"id": tid, "value": "eu"}]))).await;
    assert_eq!(status, StatusCode::OK, "{m}");
    assert_eq!(m["tags"][0]["value"], "eu");
    let (_, list) = h.json(Method::GET, "/api/v1/tags", &h.viewer, None).await;
    assert_eq!(list["data"][0]["monitors"], 1);
    assert_eq!(h.json(Method::PATCH, &format!("/api/v1/tags/{tid}"), &h.editor, Some(json!({"name": "production"}))).await.1["name"], "production");
    assert_eq!(h.call(Method::DELETE, &format!("/api/v1/tags/{tid}"), &h.editor, None).await.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn notification_channels_are_redacted_and_keep_secrets_on_update() {
    let h = harness().await;
    let url = "https://discord.com/api/webhooks/1/very-secret";
    let (status, ch) = h
        .json(Method::POST, "/api/v1/notification-channels", &h.editor, Some(json!({"name": "Ops", "kind": "discord", "config": {"url": url, "username": "Vole"}})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{ch}");
    assert_eq!(ch["config"]["url"], crate::notification::SECRET_MASK);
    assert_eq!(ch["config"]["username"], "Vole");
    let id = ch["id"].as_i64().unwrap();
    let (_, listed) = h.json(Method::GET, "/api/v1/notification-channels", &h.viewer, None).await;
    assert!(!listed.to_string().contains("very-secret"));

    let (status, upd) = h
        .json(Method::PATCH, &format!("/api/v1/notification-channels/{id}"), &h.editor, Some(json!({"config": {"url": crate::notification::SECRET_MASK, "username": "Mole"}, "active": false})))
        .await;
    assert_eq!(status, StatusCode::OK, "{upd}");
    let stored = h.state.with_conn(move |c| crate::db::notifications::get_channel(c, id)).await.unwrap().unwrap();
    assert!(stored.config.contains("very-secret") && stored.config.contains("Mole") && !stored.active);

    let (status, err) = h.json(Method::POST, "/api/v1/notification-channels", &h.editor, Some(json!({"name": "x", "kind": "discord", "config": {}}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(err["error"]["fields"]["config.url"].is_string(), "{err}");
    assert_eq!(h.json(Method::PATCH, &format!("/api/v1/notification-channels/{id}"), &h.editor, Some(json!({"kind": "slack"}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, providers) = h.json(Method::GET, "/api/v1/notification-providers", &h.viewer, None).await;
    assert_eq!(providers["data"].as_array().unwrap().len(), crate::notification::PROVIDERS.len());
}

#[tokio::test]
async fn status_pages_sections_and_incidents() {
    let h = harness().await;
    let (_, m) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(http_monitor("Site"))).await;
    let mid = m["id"].as_i64().unwrap();
    let (status, page) = h
        .json(Method::POST, "/api/v1/status-pages", &h.editor, Some(json!({"title": "Acme Cloud", "sections": [{"name": "Web", "monitor_ids": [mid]}]})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{page}");
    assert_eq!(page["slug"], "acme-cloud");
    assert_eq!(page["sections"][0]["monitors"][0]["id"], mid);
    let pid = page["id"].as_i64().unwrap();
    assert_eq!(h.json(Method::POST, "/api/v1/status-pages", &h.editor, Some(json!({"title": "Dup", "slug": "acme-cloud"}))).await.0, StatusCode::CONFLICT);
    assert_eq!(h.json(Method::POST, "/api/v1/status-pages", &h.editor, Some(json!({"title": "Bad", "slug": "api"}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = h.json(Method::PUT, &format!("/api/v1/status-pages/{pid}/sections"), &h.editor, Some(json!([{"name": "A"}, {"name": "B", "monitor_ids": [mid]}]))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, page) = h.json(Method::PATCH, &format!("/api/v1/status-pages/{pid}"), &h.editor, Some(json!({"theme": "dark"}))).await;
    assert_eq!((page["theme"].as_str(), page["sections"].as_array().unwrap().len()), (Some("dark"), 2));

    let (status, inc) = h.json(Method::POST, &format!("/api/v1/status-pages/{pid}/incidents"), &h.editor, Some(json!({"title": "Slow", "severity": "warning"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{inc}");
    let iid = inc["id"].as_i64().unwrap();
    let (_, inc) = h.json(Method::PATCH, &format!("/api/v1/status-pages/{pid}/incidents/{iid}"), &h.editor, Some(json!({"resolved": true, "severity": "success"}))).await;
    assert!(inc["resolved_at"].is_string());
    assert_eq!(h.json(Method::POST, &format!("/api/v1/status-pages/{pid}/incidents"), &h.editor, Some(json!({"title": "x", "severity": "meh"}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);

    // The incident timeline: a non-resolving update reopens, a resolving one resolves.
    let updates = format!("/api/v1/status-pages/{pid}/incidents/{iid}/updates");
    let (status, u) = h.json(Method::POST, &updates, &h.editor, Some(json!({"status": "monitoring", "body": "Watching"}))).await;
    assert_eq!((status, u["status"].as_str(), u["body"].as_str()), (StatusCode::CREATED, Some("monitoring"), Some("Watching")), "{u}");
    let uid = u["id"].as_i64().unwrap();
    let (_, inc) = h.json(Method::GET, &format!("/api/v1/status-pages/{pid}/incidents/{iid}"), &h.viewer, None).await;
    assert!(inc["resolved_at"].is_null(), "an update that isn't 'resolved' reopens: {inc}");
    assert_eq!(inc["updates"].as_array().map(Vec::len), Some(1));
    h.json(Method::POST, &updates, &h.editor, Some(json!({"status": "resolved", "body": "Fixed"}))).await;
    let (_, list) = h.json(Method::GET, &updates, &h.viewer, None).await;
    assert_eq!(list["data"][0]["status"], "resolved", "newest first: {list}");
    for bad in [json!({"status": "nope", "body": "x"}), json!({"status": "resolved", "body": " "}), json!({"body": "x"})] {
        let (status, err) = h.json(Method::POST, &updates, &h.editor, Some(bad)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    }
    assert_eq!(h.json(Method::POST, &updates, &h.viewer, Some(json!({"status": "resolved", "body": "x"}))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(h.json(Method::POST, &format!("/api/v1/status-pages/{pid}/incidents/9999/updates"), &h.editor, Some(json!({"status": "resolved", "body": "x"}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.call(Method::DELETE, &format!("{updates}/{uid}"), &h.editor, None).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(h.call(Method::DELETE, &format!("{updates}/{uid}"), &h.editor, None).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.call(Method::DELETE, &format!("/api/v1/status-pages/{pid}"), &h.editor, None).await.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn status_page_subscriptions_are_configured_and_listed() {
    let h = harness().await;
    let (_, page) = h.json(Method::POST, "/api/v1/status-pages", &h.editor, Some(json!({"title": "Subs"}))).await;
    let pid = page["id"].as_i64().unwrap();
    let url = format!("/api/v1/status-pages/{pid}/subscription");
    let (status, s) = h.json(Method::GET, &url, &h.viewer, None).await;
    assert_eq!((status, s["enabled"].as_bool(), s["webhooks"].as_bool()), (StatusCode::OK, Some(false), Some(false)));
    assert_eq!(h.json(Method::PUT, &url, &h.viewer, Some(json!({"enabled": true}))).await.0, StatusCode::FORBIDDEN);

    let (_, hook) = h.json(Method::POST, "/api/v1/notification-channels", &h.editor, Some(json!({"name": "Hook", "kind": "webhook", "config": {"url": "https://example.com/h"}}))).await;
    let (status, _) = h.json(Method::PUT, &url, &h.editor, Some(json!({"channel_id": hook["id"]}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "only email channels");

    let (status, s) = h.json(Method::PUT, &url, &h.editor, Some(json!({"enabled": true, "webhooks": true}))).await;
    assert_eq!((status, s["enabled"].as_bool(), s["webhooks"].as_bool(), s["channel_id"].is_null()), (StatusCode::OK, Some(true), Some(true), true));
    let (_, s) = h.json(Method::PUT, &url, &h.editor, Some(json!({"webhooks": false}))).await;
    assert_eq!((s["enabled"].as_bool(), s["webhooks"].as_bool()), (Some(true), Some(false)), "omitted fields stay");

    let list = format!("/api/v1/status-pages/{pid}/subscribers");
    assert_eq!(h.json(Method::GET, &list, &h.viewer, None).await.0, StatusCode::FORBIDDEN, "addresses are for editors");
    let (status, l) = h.json(Method::GET, &list, &h.editor, None).await;
    assert_eq!((status, l["data"].as_array().map(Vec::len)), (StatusCode::OK, Some(0)));
    assert_eq!(h.call(Method::DELETE, &format!("{list}/99"), &h.editor, None).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.json(Method::GET, "/api/v1/status-pages/999/subscription", &h.viewer, None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn maintenance_windows_validate_and_round_trip() {
    let h = harness().await;
    let (_, m) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(http_monitor("Site"))).await;
    let mid = m["id"].as_i64().unwrap();
    let body = json!({"title": "Patch", "strategy": "weekly", "weekdays": "2,4", "time_from": "22:00", "time_to": "23:30", "timezone": "Europe/Oslo", "monitor_ids": [mid]});
    let (status, w) = h.json(Method::POST, "/api/v1/maintenance", &h.editor, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{w}");
    assert_eq!(w["monitor_ids"], json!([mid]));
    let id = w["id"].as_i64().unwrap();
    let (status, w) = h.json(Method::PATCH, &format!("/api/v1/maintenance/{id}"), &h.editor, Some(json!({"active": false}))).await;
    assert_eq!((status, w["weekdays"].as_str(), w["active"].as_bool()), (StatusCode::OK, Some("2,4"), Some(false)));
    let (status, err) = h.json(Method::POST, "/api/v1/maintenance", &h.editor, Some(json!({"title": "x", "strategy": "weekly", "monitor_ids": [999]}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(err["error"]["fields"]["monitor_ids"].is_string(), "{err}");
}

#[tokio::test]
async fn maintenance_cron_monthly_and_interval_windows_round_trip_and_validate() {
    let h = harness().await;
    let post = |body: Value| h.json(Method::POST, "/api/v1/maintenance", &h.editor, Some(body));
    let (status, w) = post(json!({"title": "Sundays", "strategy": "cron", "cron": "0 2 * * 0", "duration_minutes": 90, "timezone": "Europe/Oslo"})).await;
    assert_eq!(status, StatusCode::CREATED, "{w}");
    assert_eq!((w["cron"].as_str(), w["duration_minutes"].as_i64()), (Some("0 2 * * 0"), Some(90)));
    let id = w["id"].as_i64().unwrap();
    // PATCH keeps what it isn't given and can clear a field with null.
    let (status, w) = h.json(Method::PATCH, &format!("/api/v1/maintenance/{id}"), &h.editor, Some(json!({"duration_minutes": 30}))).await;
    assert_eq!((status, w["cron"].as_str(), w["duration_minutes"].as_i64()), (StatusCode::OK, Some("0 2 * * 0"), Some(30)));
    let (status, w) = h.json(Method::PATCH, &format!("/api/v1/maintenance/{id}"), &h.editor, Some(json!({"strategy": "monthly", "cron": null, "duration_minutes": null, "month_days": "1,-1", "time_from": "22:00", "time_to": "23:00"}))).await;
    assert_eq!((status, w["month_days"].as_str(), w["cron"].is_null()), (StatusCode::OK, Some("1,-1"), true), "{w}");
    let (status, w) = post(json!({"title": "Every 3", "strategy": "interval", "interval_days": 3, "start_at": "2026-10-01 00:00:00", "time_from": "01:00", "time_to": "02:00"})).await;
    assert_eq!((status, w["interval_days"].as_i64()), (StatusCode::CREATED, Some(3)), "{w}");
    // Validation names the field.
    for (body, field) in [
        (json!({"title": "x", "strategy": "cron", "cron": "nope", "duration_minutes": 5}), "cron"),
        (json!({"title": "x", "strategy": "cron", "cron": "0 2 * * *", "duration_minutes": 0}), "duration_minutes"),
        (json!({"title": "x", "strategy": "monthly", "month_days": "0", "time_from": "01:00", "time_to": "02:00"}), "month_days"),
        (json!({"title": "x", "strategy": "interval", "interval_days": 0, "start_at": "2026-10-01 00:00:00", "time_from": "01:00", "time_to": "02:00"}), "interval_days"),
        (json!({"title": "x", "strategy": "interval", "interval_days": 2, "time_from": "01:00", "time_to": "02:00"}), "start_at"),
    ] {
        let (status, err) = post(body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{field}: {err}");
        assert!(err["error"]["fields"][field].is_string(), "{field}: {err}");
    }
}

#[tokio::test]
async fn settings_patch_validates_like_the_settings_page() {
    let h = harness().await;
    let (status, s) = h.json(Method::PATCH, "/api/v1/settings", &h.admin, Some(json!({"retention_days": 30, "site_name": "Acme"}))).await;
    assert_eq!((status, s["retention_days"].as_i64(), s["site_name"].as_str()), (StatusCode::OK, Some(30), Some("Acme")));
    let (status, err) = h.json(Method::PATCH, "/api/v1/settings", &h.admin, Some(json!({"timezone": "Mars/Base"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(err["error"]["fields"]["timezone"].is_string());
    assert_eq!(h.json(Method::PATCH, "/api/v1/settings", &h.admin, Some(json!({"nope": 1}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, s) = h.json(Method::PATCH, "/api/v1/settings", &h.admin, Some(json!({"agent_confirmation_default": true}))).await;
    assert_eq!((status, s["agent_confirmation_default"].as_bool()), (StatusCode::OK, Some(true)));
    let (_, health) = h.json(Method::GET, "/api/v1/health", &h.viewer, None).await;
    assert_eq!(health["status"], "ok");
}

// ---------------------------------------------------------------------------
// Config as code
// ---------------------------------------------------------------------------

#[tokio::test]
async fn config_export_import_and_replace_confirmation() {
    let h = harness().await;
    let mut m = http_monitor("Site");
    m["basic_auth_user"] = json!("u");
    m["basic_auth_pass"] = json!("topsecret");
    h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(m)).await;

    let (status, yaml) = h.raw(Method::GET, "/api/v1/config", &h.editor, "text/plain", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(yaml.contains("name: Site") && !yaml.contains("topsecret"), "{yaml}");
    assert_eq!(h.call(Method::GET, "/api/v1/config?secrets=1", &h.editor, None).await.status(), StatusCode::FORBIDDEN);
    let (_, with_secrets) = h.raw(Method::GET, "/api/v1/config?secrets=1&format=json", &h.admin, "text/plain", "").await;
    assert!(with_secrets.contains("topsecret"));

    // Dry run of an edited document.
    // Dry run of the export (unchanged) and of an edited copy (one update).
    let (status, plan) = h.raw(Method::POST, "/api/v1/config?dry_run=1", &h.admin, "application/yaml", &yaml).await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    let plan: Value = serde_json::from_str(&plan).unwrap();
    assert_eq!((plan["applied"].as_bool(), plan["summary"]["update"].as_i64()), (Some(false), Some(0)));
    let edited = yaml.replace("target: https://example.com", "target: https://example.com/v2");
    let (_, plan) = h.raw(Method::POST, "/api/v1/config?dry_run=1", &h.admin, "application/yaml", &edited).await;
    let plan: Value = serde_json::from_str(&plan).unwrap();
    assert_eq!(plan["summary"]["update"], 1, "{plan}");
    assert_eq!(plan["items"].as_array().unwrap().iter().find(|i| i["action"] == "update").unwrap()["changes"], json!(["target"]));

    // Replace without confirm is refused; with confirm it deletes unlisted monitors.
    let doc = "monitors: []\n";
    let (status, body) = h.raw(Method::POST, "/api/v1/config?mode=replace", &h.admin, "application/yaml", doc).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("confirmation_required"));
    let (status, body) = h.raw(Method::POST, "/api/v1/config?mode=replace&confirm=1", &h.admin, "application/yaml", doc).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, list) = h.json(Method::GET, "/api/v1/monitors", &h.viewer, None).await;
    assert!(list["data"].as_array().unwrap().is_empty());

    // Invalid documents: envelope plus the plan.
    let (status, body) = h.raw(Method::POST, "/api/v1/config", &h.admin, "application/yaml", "monitors:\n  - name: X\n    monitor_type: http\n    target: nope\n").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["error"]["code"], "validation_failed");
    assert_eq!(body["plan"]["errors"][0]["field"], "target");
    let (status, _) = h.raw(Method::POST, "/api/v1/config", &h.admin, "application/yaml", "monitorz: 1").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------
// Docs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_documented_v1_route_exists() {
    let h = harness().await;
    for r in ROUTES.iter().filter(|r| r.path.starts_with("/api/v1/")) {
        let uri = r.path.replace("{id}", "999999").replace("{iid}", "999999");
        let method = Method::from_bytes(r.method.as_bytes()).unwrap();
        let body = matches!(r.method, "POST" | "PUT" | "PATCH").then(|| json!({}));
        let res = h.call(method, &uri, &h.admin_read, body).await;
        assert_ne!(res.status(), StatusCode::METHOD_NOT_ALLOWED, "{} {}", r.method, r.path);
        // A 404 must come from the handler (missing object), not the router.
        if res.status() == StatusCode::NOT_FOUND {
            assert!(r.path.contains('{'), "{} {} is not routed", r.method, r.path);
        }
    }
}

#[tokio::test]
async fn openapi_document_lists_every_route() {
    let h = harness().await;
    let (status, doc) = h.json(Method::GET, "/api/v1/openapi.json", &h.viewer, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["openapi"], "3.1.0");
    let ops: usize = doc["paths"].as_object().unwrap().values().map(|p| p.as_object().unwrap().len()).sum();
    assert_eq!(ops, ROUTES.len());
    assert!(doc["paths"]["/api/v1/monitors/{id}"]["patch"]["requestBody"].is_object());
    assert_eq!(doc["paths"]["/api/push/{token}"]["get"]["security"], json!([]));
    // Creates answer 201, actions 200.
    for (path, code) in [("/api/v1/monitors", "201"), ("/api/v1/status-pages/{id}/incidents", "201"), ("/api/v1/monitors/bulk", "200"), ("/api/v1/monitors/{id}/pause", "200"), ("/api/v1/notification-channels/{id}/test", "200"), ("/api/v1/config", "200")] {
        assert!(doc["paths"][path]["post"]["responses"][code].is_object(), "POST {path} → {code}");
    }
}

#[tokio::test]
async fn documented_success_codes_match_the_server() {
    let h = harness().await;
    let res = h.call(Method::POST, "/api/v1/tags", &h.editor, Some(json!({"name": "web"}))).await;
    assert_eq!(res.status().as_u16(), ROUTES.iter().find(|r| r.method == "POST" && r.path == "/api/v1/tags").unwrap().success_status());
    let (status, monitor) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(http_monitor("Docs"))).await;
    assert_eq!(status, StatusCode::CREATED);
    let id = monitor["id"].as_i64().unwrap();
    for (method, path, uri, body) in [
        ("POST", "/api/v1/monitors/{id}/pause", format!("/api/v1/monitors/{id}/pause"), None),
        ("POST", "/api/v1/monitors/bulk", "/api/v1/monitors/bulk".to_string(), Some(json!({"ids": [id], "set": {"retries": 2}}))),
        ("DELETE", "/api/v1/monitors/{id}", format!("/api/v1/monitors/{id}"), None),
    ] {
        let doc = ROUTES.iter().find(|r| r.method == method && r.path == path).unwrap();
        let res = h.call(Method::from_bytes(method.as_bytes()).unwrap(), &uri, &h.editor, body).await;
        assert_eq!(res.status().as_u16(), doc.success_status(), "{method} {path}");
    }
}

#[test]
fn route_docs_have_unique_method_and_path() {
    let mut seen = std::collections::HashSet::new();
    for r in ROUTES {
        assert!(seen.insert((r.method, r.path)), "duplicate {} {}", r.method, r.path);
        assert!(!r.summary.is_empty());
    }
}

#[test]
fn simple_content_types_are_the_cross_site_ones() {
    let mut h = axum::http::HeaderMap::new();
    assert!(is_simple_content_type(&h));
    h.insert(header::CONTENT_TYPE, "text/plain;charset=utf-8".parse().unwrap());
    assert!(is_simple_content_type(&h));
    h.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    assert!(!is_simple_content_type(&h));
}

// ---------------------------------------------------------------------------
// Session-authenticated pages (docs, backup) and the session write guard
// ---------------------------------------------------------------------------

struct Browser {
    app: Router,
    cookies: HashMap<String, String>,
}

impl Browser {
    async fn send(&mut self, req: axum::http::request::Builder, body: ReqBody) -> HttpResponse {
        let cookie = self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
        let req = req.header(header::HOST, "vole.test").header(header::COOKIE, cookie);
        let res = self.app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        for value in res.headers().get_all(header::SET_COOKIE) {
            let value = value.to_str().unwrap();
            if let Some((name, val)) = value.split(';').next().and_then(|p| p.split_once('=')) {
                self.cookies.insert(name.to_string(), val.to_string());
            }
        }
        res
    }

    async fn login(h: &Harness, user: &str) -> Self {
        let mut b = Browser { app: h.app.clone(), cookies: HashMap::new() };
        let body = format!("username={user}&password={}", PASSWORD.replace(' ', "+"));
        let req = Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::ORIGIN, "http://vole.test")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        let res = b.send(req, ReqBody::from(body)).await;
        assert!(res.status().is_redirection(), "login failed: {}", res.status());
        b
    }

    async fn get(&mut self, uri: &str) -> (StatusCode, String) {
        let res = self.send(Request::builder().uri(uri), ReqBody::empty()).await;
        let status = res.status();
        (status, String::from_utf8_lossy(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned())
    }
}

#[tokio::test]
async fn docs_and_backup_pages_render_for_the_right_roles() {
    let h = harness().await;
    let mut viewer = Browser::login(&h, "viewer").await;
    let (status, html) = viewer.get("/api-docs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("/api/v1/monitors") && html.contains("API reference"));
    assert_eq!(viewer.get("/settings/backup").await.0, StatusCode::FORBIDDEN);

    let mut admin = Browser::login(&h, "admin").await;
    let (status, html) = admin.get("/settings/backup").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Move from Uptime Kuma") && html.contains("/settings/backup/database"));
    let res = admin.send(Request::builder().uri("/settings/backup/database"), ReqBody::empty()).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().contains("vigilant-vole-backup-"));
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    assert!(kuma_import_magic(&bytes));
}

#[tokio::test]
async fn markdown_reference_follows_the_docs_page_access_and_caches() {
    let h = harness().await;
    for uri in ["/api-docs", "/api-docs.md", "/llms.txt"] {
        // Signed out, even with an API token (tokens only work under /api/): the login page.
        for token in ["", h.viewer.as_str()] {
            let res = h.call(Method::GET, uri, token, None).await;
            assert_eq!(res.status(), StatusCode::SEE_OTHER, "{uri}");
            let next = url::form_urlencoded::byte_serialize(uri.as_bytes()).collect::<String>();
            assert_eq!(res.headers()[header::LOCATION], format!("/login?next={next}").as_str());
        }
    }

    let mut viewer = Browser::login(&h, "viewer").await;
    let (status, html) = viewer.get("/api-docs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("class=\"card ai-strip\"") && html.contains("data-copy-url=\"/api-docs.md\""));
    assert!(html.contains("Copy for AI") && html.contains("Hand it the whole API in one paste."));

    let res = viewer.send(Request::builder().uri("/api-docs.md"), ReqBody::empty()).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/markdown; charset=utf-8");
    assert!(res.headers()[header::CACHE_CONTROL].to_str().unwrap().contains("no-cache"));
    let etag = res.headers()[header::ETAG].clone();
    let markdown = String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
    assert!(markdown.starts_with("# Vigilant Vole API\n"));
    assert!(markdown.contains("- API base URL: http://vole.test/api/v1\n"));

    let req = Request::builder().uri("/api-docs.md").header(header::IF_NONE_MATCH, etag.clone());
    let res = viewer.send(req, ReqBody::empty()).await;
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(res.headers()[header::ETAG], etag);

    let res = viewer.send(Request::builder().uri("/llms.txt"), ReqBody::empty()).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert_eq!(res.headers()[header::ETAG], etag);
    assert_eq!(to_bytes(res.into_body(), usize::MAX).await.unwrap(), markdown.as_bytes());

    // The configured public URL wins over the request's host.
    let (status, _) = h.json(Method::PATCH, "/api/v1/settings", &h.admin, Some(json!({"base_url": "https://status.example.com"}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, markdown) = viewer.get("/api-docs.md").await;
    assert_eq!(status, StatusCode::OK);
    assert!(markdown.contains("export VV_URL=\"https://status.example.com\"") && !markdown.contains("vole.test"));
}

fn kuma_import_magic(bytes: &[u8]) -> bool {
    crate::kuma_import::is_sqlite(bytes)
}

#[tokio::test]
async fn session_api_writes_need_same_origin_or_json() {
    let h = harness().await;
    let mut b = Browser::login(&h, "editor").await;
    // A cross-site form post (simple content type, no Origin) is refused…
    let req = Request::builder().method("POST").uri("/api/v1/tags").header(header::CONTENT_TYPE, "text/plain");
    let res = b.send(req, ReqBody::from("{}")).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    // …while JSON (which needs a CORS preflight cross-site) is fine.
    let req = Request::builder().method("POST").uri("/api/v1/tags").header(header::CONTENT_TYPE, "application/json");
    assert_eq!(b.send(req, ReqBody::from(r#"{"name": "web"}"#)).await.status(), StatusCode::CREATED);
    // Same-origin bodiless POSTs work too.
    let req = Request::builder().method("POST").uri("/api/v1/monitors/1/pause").header(header::ORIGIN, "http://vole.test");
    assert_eq!(b.send(req, ReqBody::empty()).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn backup_import_preview_and_apply_via_the_page() {
    let h = harness().await;
    let mut admin = Browser::login(&h, "admin").await;
    let boundary = "XBOUNDARY";
    let doc = "monitors:\n  - name: From file\n    monitor_type: ping\n    target: 192.0.2.10\n";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"document\"\r\n\r\n{doc}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"mode\"\r\n\r\nmerge\r\n--{boundary}--\r\n"
    );
    let req = Request::builder()
        .method("POST")
        .uri("/settings/backup/import/preview")
        .header("hx-request", "true")
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"));
    let res = admin.send(req, ReqBody::from(body)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let html = String::from_utf8_lossy(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned();
    assert!(html.contains("From file") && html.contains("Apply 1 change"), "{html}");
    let token = html.split("name=\"token\" value=\"").nth(1).unwrap().split('"').next().unwrap().to_string();

    let req = Request::builder()
        .method("POST")
        .uri("/settings/backup/import/apply")
        .header("hx-request", "true")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    let res = admin.send(req, ReqBody::from(format!("token={token}"))).await;
    assert_eq!(res.headers()["hx-refresh"], "true");
    let (_, list) = h.json(Method::GET, "/api/v1/monitors", &h.viewer, None).await;
    assert_eq!(list["data"][0]["name"], "From file");
    // Tokens are single-use.
    let req = Request::builder()
        .method("POST")
        .uri("/settings/backup/import/apply")
        .header("hx-request", "true")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    let res = admin.send(req, ReqBody::from(format!("token={token}"))).await;
    let html = String::from_utf8_lossy(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned();
    assert!(html.contains("expired"));
}

// ---------------------------------------------------------------------------
// Security regressions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_update_to_a_new_server_needs_the_secret_again() {
    let h = harness().await;
    let config = json!({"server": "https://push.example.com", "app_token": "STORED-TOKEN"});
    let (status, ch) = h
        .json(Method::POST, "/api/v1/notification-channels", &h.editor, Some(json!({"name": "Gotify", "kind": "gotify", "config": config})))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{ch}");
    let id = ch["id"].as_i64().unwrap();
    let uri = format!("/api/v1/notification-channels/{id}");
    // Moving the channel to another server with the token masked is refused…
    let moved = json!({"config": {"server": "https://attacker.example", "app_token": crate::notification::SECRET_MASK}});
    let (status, err) = h.json(Method::PATCH, &uri, &h.editor, Some(moved)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["error"]["fields"]["config.app_token"].is_string(), "{err}");
    let stored = h.state.with_conn(move |c| crate::db::notifications::get_channel(c, id)).await.unwrap().unwrap();
    assert!(stored.config.contains("push.example.com"));
    // …a retyped token is fine, and so is a masked token on the same server.
    let (status, _) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"config": {"server": "https://new.example", "app_token": "NEW"}}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"config": {"server": "https://new.example", "app_token": crate::notification::SECRET_MASK, "priority": 8}}))).await;
    assert_eq!(status, StatusCode::OK);
    let stored = h.state.with_conn(move |c| crate::db::notifications::get_channel(c, id)).await.unwrap().unwrap();
    assert!(stored.config.contains("\"NEW\"") && stored.config.contains("new.example"));
}

/// A same-origin form post from a logged-in browser.
async fn form_post(b: &mut Browser, uri: &str, body: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::ORIGIN, "http://vole.test")
        .header("hx-request", "true")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    let res = b.send(req, ReqBody::from(body.to_string())).await;
    let status = res.status();
    let headers = format!("{:?}", res.headers());
    (status, headers + &String::from_utf8_lossy(&to_bytes(res.into_body(), usize::MAX).await.unwrap()))
}

#[tokio::test]
async fn notification_form_update_and_test_refuse_stored_secret_on_new_server() {
    let h = harness().await;
    let config = json!({"server": "https://push.example.com", "app_token": "STORED-TOKEN"}).to_string();
    let id = h
        .state
        .with_conn(move |c| crate::db::notifications::insert_channel(c, "Gotify", "gotify", &config, false))
        .await
        .unwrap();
    let mut editor = Browser::login(&h, "editor").await;
    let body = "name=Gotify&kind=gotify&c_server=https%3A%2F%2Fattacker.example&c_app_token=";
    // "Send test" with the edited form: refused before anything is sent.
    let (status, out) = form_post(&mut editor, &format!("/settings/notifications/{id}/test"), body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(out.contains("destination changed") && !out.contains("Test sent"), "{out}");
    // Save: 422 with the error on the token field; nothing stored.
    let (status, html) = form_post(&mut editor, &format!("/settings/notifications/{id}"), body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("destination changed"), "{html}");
    let stored = h.state.with_conn(move |c| crate::db::notifications::get_channel(c, id)).await.unwrap().unwrap();
    assert!(stored.config.contains("push.example.com"));
}

#[tokio::test]
async fn long_markdown_fields_are_capped_everywhere() {
    let h = harness().await;
    let footer = "x".repeat(crate::db::status_pages::FOOTER_MAX + 1);
    let (status, err) = h.json(Method::POST, "/api/v1/status-pages", &h.editor, Some(json!({"title": "Acme", "footer_text": footer}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["error"]["fields"]["footer_text"].is_string(), "{err}");
    let ok_footer = "x".repeat(crate::db::status_pages::FOOTER_MAX);
    let (status, page) = h.json(Method::POST, "/api/v1/status-pages", &h.editor, Some(json!({"title": "Acme", "footer_text": ok_footer}))).await;
    assert_eq!(status, StatusCode::CREATED, "{page}");
    let pid = page["id"].as_i64().unwrap();

    let description = "y".repeat(crate::db::maintenance::DESCRIPTION_MAX + 1);
    let body = json!({"title": "Patch", "strategy": "manual", "description": description});
    let (status, err) = h.json(Method::POST, "/api/v1/maintenance", &h.editor, Some(body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["error"]["fields"]["description"].is_string(), "{err}");

    let body = "z".repeat(20_001);
    let (status, _) = h.json(Method::POST, &format!("/api/v1/status-pages/{pid}/incidents"), &h.editor, Some(json!({"title": "Slow", "body": body}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // The settings form refuses it too.
    let mut editor = Browser::login(&h, "editor").await;
    let form = format!("title=Acme&slug=acme&theme=auto&refresh_seconds=60&footer_text={}", "x".repeat(5_001));
    let (status, html) = form_post(&mut editor, &format!("/status-pages/{pid}/edit"), &form).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("f-footer_text-error"), "{html}");
    let form = format!("title=Acme&slug=acme&theme=auto&strategy=manual&description={}", "y".repeat(20_001));
    let (status, html) = form_post(&mut editor, "/maintenance/new", &form).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("f-description-error"), "{html}");
}

#[tokio::test]
async fn custom_domain_cannot_be_the_instance_itself() {
    let h = harness().await;
    // The request's own Host (the harness sends "vole.test").
    let (status, err) = h.json(Method::POST, "/api/v1/status-pages", &h.editor, Some(json!({"title": "Acme", "custom_domain": "vole.test"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["error"]["fields"]["custom_domain"].is_string(), "{err}");
    // The configured base URL's host.
    h.state
        .with_conn(|c| crate::db::settings::set(c, crate::db::settings::BASE_URL, "https://uptime.example.com/"))
        .await
        .unwrap();
    let (status, _) = h.json(Method::POST, "/api/v1/status-pages", &h.editor, Some(json!({"title": "Acme", "custom_domain": "Uptime.Example.com"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, page) = h.json(Method::POST, "/api/v1/status-pages", &h.editor, Some(json!({"title": "Acme", "custom_domain": "status.acme.test"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{page}");
    let pid = page["id"].as_i64().unwrap();

    // Settings form.
    let mut editor = Browser::login(&h, "editor").await;
    let (status, html) = form_post(&mut editor, &format!("/status-pages/{pid}/edit"), "title=Acme&slug=acme&theme=auto&refresh_seconds=60&custom_domain=vole.test").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("address of Vigilant Vole"), "{html}");

    // Config import (admin) checks the base URL.
    let doc = "status_pages:\n  - slug: other\n    title: Other\n    custom_domain: uptime.example.com\n";
    let (status, body) = h.raw(Method::POST, "/api/v1/config?dry_run=1", &h.admin, "application/yaml", doc).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && body.contains("custom_domain"), "{status} {body}");
}

#[tokio::test]
async fn public_views_are_cached_briefly_and_dropped_on_edits() {
    let h = harness().await;
    let (_, page) = h.json(Method::POST, "/api/v1/status-pages", &h.editor, Some(json!({"title": "Cache Co"}))).await;
    let pid = page["id"].as_i64().unwrap();
    let (status, first) = h.json(Method::GET, "/api/status-page/cache-co", "", None).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    // A change behind the app's back is not seen while the view is fresh…
    h.state
        .with_conn(move |c| c.execute("UPDATE status_pages SET footer_text = 'sneaky' WHERE id = ?1", [pid]).map(|_| ()))
        .await
        .unwrap();
    let (_, cached) = h.json(Method::GET, "/api/status-page/cache-co", "", None).await;
    assert_eq!(first, cached);
    // …but an edit through the API is.
    let (status, _) = h.json(Method::POST, &format!("/api/v1/status-pages/{pid}/incidents"), &h.editor, Some(json!({"title": "Fresh incident"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, fresh) = h.json(Method::GET, "/api/status-page/cache-co", "", None).await;
    assert!(fresh.to_string().contains("Fresh incident"), "{fresh}");
}

#[tokio::test]
async fn monitor_secrets_are_not_kept_when_the_target_host_changes() {
    let h = harness().await;
    let mut body = http_monitor("Site");
    body["basic_auth_user"] = json!("bot");
    body["basic_auth_pass"] = json!("hunter2");
    body["http_headers"] = json!({"Authorization": "Bearer T0K"});
    let (_, m) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(body)).await;
    let id = m["id"].as_i64().unwrap();
    let uri = format!("/api/v1/monitors/{id}");
    let mask = crate::notification::SECRET_MASK;
    // Same host, new path: masked password kept.
    let (status, _) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"target": "https://example.com/health", "basic_auth_pass": mask}))).await;
    assert_eq!(status, StatusCode::OK);
    // New host with masked (or simply omitted) secrets: refused.
    let (status, err) = h
        .json(Method::PATCH, &uri, &h.editor, Some(json!({"target": "https://attacker.example/", "basic_auth_pass": mask, "http_headers": {"Authorization": mask}})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["error"]["fields"]["http_headers"].is_string(), "{err}");
    let (status, err) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"target": "https://attacker.example/"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    let (status, err) = h
        .json(Method::PATCH, &uri, &h.editor, Some(json!({"target": "https://attacker.example/", "http_headers": {"Authorization": "Bearer NEW"}})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["error"]["fields"]["basic_auth_pass"].is_string(), "{err}");
    let stored = h.state.with_conn(move |c| crate::db::monitors::get_monitor(c, id)).await.unwrap().unwrap();
    assert_eq!(stored.target, "https://example.com/health");
    // Retyped: fine.
    let (status, _) = h
        .json(Method::PATCH, &uri, &h.editor, Some(json!({"target": "https://new.example/", "basic_auth_pass": "pw2", "http_headers": {"Authorization": "Bearer NEW"}})))
        .await;
    assert_eq!(status, StatusCode::OK);
}

fn multipart(boundary: &str, parts: &[(&str, Option<&str>, &str)]) -> String {
    let mut body = String::new();
    for (name, file, content) in parts {
        let filename = file.map(|f| format!("; filename=\"{f}\"")).unwrap_or_default();
        body.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"{filename}\r\n\r\n{content}\r\n"));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    body
}

#[tokio::test]
async fn backup_uploads_are_bounded_and_cleaned_up() {
    let h = harness().await;
    let mut admin = Browser::login(&h, "admin").await;
    let boundary = "XB";
    let send = |uri: &'static str, body: String| {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("hx-request", "true")
            .header(header::ORIGIN, "http://vole.test")
            .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"));
        (req, ReqBody::from(body))
    };
    // A file that isn't a database: refused, and nothing is left behind.
    let (req, body) = send("/settings/backup/restore", multipart(boundary, &[("file", Some("../../evil.db"), "not a database")]));
    let res = admin.send(req, body).await;
    assert!(res.headers()["hx-trigger"].to_str().unwrap().contains("error"));
    let data = h.state.config.data_dir.clone();
    assert!(!data.join(crate::config_io::RESTORE_PENDING_FILE).exists());
    let leftovers: Vec<_> = std::fs::read_dir(data.join("tmp")).map(|d| d.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(!data.parent().unwrap().join("evil.db").exists());
    // Two files in one upload are refused.
    let (req, body) = send("/settings/backup/import/preview", multipart(boundary, &[("file", Some("a.yaml"), "monitors: []"), ("file", Some("b.yaml"), "monitors: []")]));
    let res = admin.send(req, body).await;
    let html = String::from_utf8_lossy(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned();
    assert!(html.contains("one file"), "{html}");
    // Config documents have a small explicit limit.
    let big = "#".repeat(super::config::MAX_DOCUMENT_BYTES + 128 * 1024);
    let (req, body) = send("/settings/backup/import/preview", multipart(boundary, &[("document", None, &big)]));
    let res = admin.send(req, body).await;
    let html = String::from_utf8_lossy(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned();
    assert!(!html.contains("Apply"), "{html}");
}

#[tokio::test]
async fn auth_methods_are_masked_in_the_api_and_secrets_stay_with_their_destination() {
    use crate::monitor::checkers::http_auth::test_pki::Pki;
    let h = harness().await;
    let mask = crate::notification::SECRET_MASK;
    let pki = Pki::new();

    // OAuth2: the client secret never comes back; PATCHing other fields keeps it.
    let mut body = http_monitor("Oauth");
    body["auth_method"] = json!("oauth2");
    body["oauth_token_url"] = json!("https://login.example.com/token");
    body["oauth_client_id"] = json!("vole");
    body["oauth_client_secret"] = json!("client-secret-value");
    body["oauth_scope"] = json!("read write");
    let (status, created) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!((created["auth_method"].as_str(), created["oauth_client_secret"].as_str()), (Some("oauth2"), Some(mask)));
    assert!(!created.to_string().contains("client-secret-value"));
    let id = created["id"].as_i64().unwrap();
    let uri = format!("/api/v1/monitors/{id}");
    let (status, patched) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"interval_seconds": 30, "target": "https://other.example.com/"}))).await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    let stored = h.state.with_conn(move |c| crate::db::monitors::get_monitor(c, id)).await.unwrap().unwrap();
    assert_eq!(stored.oauth_client_secret.as_deref(), Some("client-secret-value"));
    // A different token endpoint needs the secret again.
    let (status, err) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"oauth_token_url": "https://attacker.example/token"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["error"]["fields"]["oauth_client_secret"].is_string(), "{err}");
    // Validation errors name the field.
    let (status, err) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"auth_method": "kerberos"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(err["error"]["fields"]["auth_method"].is_string(), "{err}");

    // mTLS: certificate visible, private key masked and tied to the target.
    let mut body = http_monitor("Mtls");
    body["auth_method"] = json!("mtls");
    body["tls_client_cert"] = json!(pki.client_cert_pem);
    body["tls_client_key"] = json!(pki.client_key_pem);
    body["tls_ca_cert"] = json!(pki.ca_pem);
    let (status, created) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["tls_client_key"], mask);
    assert!(!created.to_string().contains("PRIVATE KEY"));
    assert!(created["tls_client_cert"].as_str().unwrap().contains("BEGIN CERTIFICATE"));
    let id = created["id"].as_i64().unwrap();
    let uri = format!("/api/v1/monitors/{id}");
    let (status, err) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"target": "https://attacker.example/"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert!(err["error"]["fields"]["tls_client_key"].is_string(), "{err}");
    let (status, err) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"tls_client_cert": "not a certificate"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(err["error"]["fields"]["tls_client_cert"].is_string(), "{err}");
    // Lists and viewers see the same masking.
    let (_, list) = h.json(Method::GET, "/api/v1/monitors", &h.viewer, None).await;
    assert!(!list.to_string().contains("PRIVATE KEY") && !list.to_string().contains("client-secret-value"));

    // Clients that only know basic auth keep working without `auth_method`.
    let mut body = http_monitor("Legacy");
    body["basic_auth_user"] = json!("bot");
    body["basic_auth_pass"] = json!("hunter2");
    let (status, legacy) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{legacy}");
    assert_eq!(legacy["auth_method"], "basic");
}

#[tokio::test]
async fn auth_method_none_switches_basic_auth_off() {
    let h = harness().await;
    let mut body = http_monitor("Basic");
    body["auth_method"] = json!("basic");
    body["basic_auth_user"] = json!("bot");
    body["basic_auth_pass"] = json!("hunter2");
    let (status, created) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_i64().unwrap();
    let uri = format!("/api/v1/monitors/{id}");
    let stored = |h: &Harness| {
        let state = h.state.clone();
        async move { state.with_conn(move |c| crate::db::monitors::get_monitor(c, id)).await.unwrap().unwrap() }
    };

    // An explicit "none" is respected and clears the basic credentials.
    let (status, patched) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"auth_method": "none"}))).await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["auth_method"], "none");
    let m = stored(&h).await;
    assert_eq!((m.auth_method.as_str(), m.basic_auth_user.as_deref(), m.basic_auth_pass.as_deref()), ("none", None, None));

    // Old clients that only send a user name (or an empty method) still mean basic.
    let (status, patched) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"auth_method": "", "basic_auth_user": "bot", "basic_auth_pass": "pw"}))).await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    let m = stored(&h).await;
    assert_eq!((m.auth_method.as_str(), m.basic_auth_user.as_deref()), ("basic", Some("bot")));
    // ... and a PATCH that doesn't mention the method keeps it.
    let (status, _) = h.json(Method::PATCH, &uri, &h.editor, Some(json!({"interval_seconds": 120}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored(&h).await.auth_method, "basic");
}

// ---------------------------------------------------------------------------
// Bulk edit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bulk_edit_by_ids_and_filter() {
    let h = harness().await;
    let mut ids = Vec::new();
    for body in [
        http_monitor("Web"),
        json!({"name": "Cron", "monitor_type": "push"}),
        json!({"name": "Prod", "monitor_type": "group"}),
        json!({"name": "DB", "monitor_type": "tcp", "target": "127.0.0.1:5432", "tags": ["db"]}),
    ] {
        let (status, m) = h.json(Method::POST, "/api/v1/monitors", &h.editor, Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{m}");
        ids.push(m["id"].as_i64().unwrap());
    }
    let (_, ch) = h
        .json(Method::POST, "/api/v1/notification-channels", &h.editor, Some(json!({"name": "Ops", "kind": "discord", "config": {"url": "https://discord.com/api/webhooks/1/x"}})))
        .await;
    let ops = ch["id"].as_i64().unwrap();

    let body = json!({
        "ids": ids[..3],
        "set": {
            "run_on_agents": true,
            "interval_seconds": 120,
            "channels": {"add": [{"name": "ops"}]},
            "tags": {"add": [{"name": "eu", "color": "teal", "value": "west"}]},
        },
    });
    let (status, out) = h.json(Method::POST, "/api/v1/monitors/bulk", &h.editor, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["updated"], 3);
    let skipped = out["skipped"].as_array().unwrap();
    assert!(skipped.contains(&json!({"id": ids[1], "reason": "agents_unsupported"})), "{out}");
    assert!(skipped.contains(&json!({"id": ids[2], "reason": "timing_unsupported"})), "{out}");
    let (_, web) = h.json(Method::GET, &format!("/api/v1/monitors/{}", ids[0]), &h.viewer, None).await;
    assert_eq!((web["interval_seconds"].as_i64(), web["run_on_agents"].as_bool()), (Some(120), Some(true)));
    assert_eq!(web["notifications"], json!([{"channel_id": ops, "escalation": false}]));
    assert_eq!(web["tags"][0]["name"], "eu");

    // Every monitor matching a filter (the list's semantics: tag by name).
    let body = json!({"filter": {"tag": "DB"}, "set": {"retries": 4, "tags": {"remove": ["db"]}}});
    let (status, out) = h.json(Method::POST, "/api/v1/monitors/bulk", &h.editor, Some(body)).await;
    assert_eq!((status, out["updated"].as_i64()), (StatusCode::OK, Some(1)));
    let (_, db) = h.json(Method::GET, &format!("/api/v1/monitors/{}", ids[3]), &h.viewer, None).await;
    assert_eq!((db["retries"].as_i64(), db["tags"].as_array().map(Vec::len)), (Some(4), Some(0)));

    // Replace channels with an escalation one: skipped without a delay.
    let body = json!({"ids": [ids[0]], "set": {"channels": {"replace": [{"id": ops, "escalation": true}]}}});
    let (_, out) = h.json(Method::POST, "/api/v1/monitors/bulk", &h.editor, Some(body)).await;
    assert_eq!(out["skipped"], json!([{"id": ids[0], "reason": "escalation_needs_delay"}]));
}

#[tokio::test]
async fn bulk_edit_validation_and_roles() {
    let h = harness().await;
    let post = |token: &str, body: Value| {
        let token = token.to_string();
        let h = &h;
        async move { h.json(Method::POST, "/api/v1/monitors/bulk", &token, Some(body)).await }
    };
    let set = json!({"retries": 1});
    let (status, _) = post(&h.viewer, json!({"ids": [1], "set": set})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = post(&h.admin_read, json!({"ids": [1], "set": set})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let cases = [
        (json!({"set": set}), "ids"),
        (json!({"ids": [1], "filter": {}, "set": set}), "ids"),
        (json!({"ids": [1]}), "set"),
        (json!({"ids": [1], "set": {}}), "set"),
        (json!({"ids": [1], "set": {"interval_seconds": 5}}), "set.interval_seconds"),
        (json!({"ids": [1], "set": {"interval_seconds": 30, "timeout_seconds": 60}}), "set.timeout_seconds"),
        (json!({"ids": [1], "set": {"retries": "two"}}), "set.retries"),
        (json!({"ids": [1], "set": {"color": "red"}}), "set.color"),
        (json!({"ids": [1], "set": {"run_on_agents": "yes"}}), "set.run_on_agents"),
        (json!({"ids": [1], "set": {"channels": {"add": ["Nope"]}}}), "set.channels.add"),
        (json!({"ids": [1], "set": {"channels": {"add": [1], "replace": []}}}), "set.channels"),
        (json!({"ids": [1], "set": {"tags": {"remove": ["missing"]}}}), "set.tags.remove"),
        (json!({"ids": [1], "set": {"tags": {"add": [{"name": "x".repeat(80)}]}}}), "set.tags.add"),
        (json!({"filter": {"status": "sideways"}, "set": set}), "filter.status"),
        (json!({"filter": {"colour": "red"}, "set": set}), "filter"),
        (json!({"ids": [1], "set": set, "dry_run": true}), "dry_run"),
    ];
    for (body, field) in cases {
        let (status, err) = post(&h.editor, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body} → {err}");
        assert!(err["error"]["fields"].get(field).is_some(), "{body}: expected {field} in {err}");
    }
    // Unknown ids are simply not there to change.
    let (status, out) = post(&h.editor, json!({"ids": [424242], "set": set})).await;
    assert_eq!((status, out["updated"].as_i64()), (StatusCode::OK, Some(0)));
}
