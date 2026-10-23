//! Protocol and tool tests for `POST /mcp`, through the real router.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use axum_extra::extract::cookie::Key;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::*;
use crate::api::build_router;
use crate::auth::{self, Role};
use crate::config::Config;
use crate::db::api_tokens::{self as db_tokens, TokenScope};
use crate::db::users as db_users;
use crate::monitor::Scheduler;

static PASSWORD_HASH: LazyLock<String> =
    LazyLock::new(|| auth::try_hash_password("correct horse battery").unwrap());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Harness {
    app: Router,
    state: AppState,
    admin: String,
    admin_read: String,
    editor: String,
    viewer_write: String,
    dir: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn token(state: &AppState, user_id: i64, scope: TokenScope) -> String {
    let t = auth::tokens::generate();
    let (prefix, hash) = (auth::tokens::display_prefix(&t), auth::tokens::hash(&t));
    state
        .with_conn(move |c| db_tokens::insert(c, user_id, "test", &prefix, &hash, scope, None))
        .await
        .unwrap();
    t
}

async fn harness() -> Harness {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("vv-mcp-test-{}-{n}", std::process::id()));
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
        sign_in_limits: Default::default(),
        config: Arc::new(Config {
            data_dir: dir.clone(),
            bind_addr: "127.0.0.1:0".into(),
        }),
    };
    let mut ids = Vec::new();
    for (name, role) in [
        ("admin", Role::Admin),
        ("editor", Role::Editor),
        ("viewer", Role::Viewer),
    ] {
        let hash = PASSWORD_HASH.clone();
        ids.push(
            state
                .with_conn(move |c| db_users::insert_user(c, name, &hash, role))
                .await
                .unwrap(),
        );
    }
    let admin = token(&state, ids[0], TokenScope::Write).await;
    let admin_read = token(&state, ids[0], TokenScope::Read).await;
    let editor = token(&state, ids[1], TokenScope::Write).await;
    let viewer_write = token(&state, ids[2], TokenScope::Write).await;
    Harness {
        app: build_router(state.clone()),
        state,
        admin,
        admin_read,
        editor,
        viewer_write,
        dir,
    }
}

impl Harness {
    async fn post(
        &self,
        token: Option<&str>,
        extra: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::HOST, "vole.test")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        let res = self
            .app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    /// One JSON-RPC request; returns the whole reply.
    async fn rpc(&self, token: &str, method: &str, params: Value) -> Value {
        let body =
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let (status, _, text) = self.post(Some(token), &[], &body).await;
        assert_eq!(status, StatusCode::OK, "{method}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    /// A tool call: `(is_error, text)`.
    async fn tool(&self, token: &str, name: &str, args: Value) -> (bool, String) {
        let reply = self
            .rpc(
                token,
                "tools/call",
                json!({"name": name, "arguments": args}),
            )
            .await;
        let result = &reply["result"];
        assert!(result.is_object(), "{name}: {reply}");
        (
            result["isError"].as_bool().unwrap(),
            result["content"][0]["text"].as_str().unwrap().to_string(),
        )
    }

    async fn tool_json(&self, token: &str, name: &str, args: Value) -> Value {
        let (err, text) = self.tool(token, name, args).await;
        assert!(!err, "{name} failed: {text}");
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    }

    async fn rest(&self, method: Method, uri: &str, token: &str, body: Option<Value>) -> Value {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "vole.test")
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }
}

fn tool_names(reply: &Value) -> Vec<String> {
    reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Transport, authentication, protocol
// ---------------------------------------------------------------------------

#[tokio::test]
async fn requests_need_a_valid_bearer_token_and_the_right_origin() {
    let h = harness().await;
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let (status, headers, _) = h.post(None, &[], ping).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        headers
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Bearer")
    );
    assert_eq!(
        h.post(Some("vv_notatoken"), &[], ping).await.0,
        StatusCode::UNAUTHORIZED
    );
    // A session cookie is never enough, and neither is Basic.
    assert_eq!(
        h.post(None, &[("cookie", "vv_session=abc")], ping).await.0,
        StatusCode::UNAUTHORIZED
    );
    let basic = format!(
        "Basic {}",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("x:{}", h.admin)
        )
    );
    assert_eq!(
        h.post(None, &[("authorization", basic.as_str())], ping)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    // OK with a token; browsers on other origins are refused even with one.
    assert_eq!(h.post(Some(&h.admin), &[], ping).await.0, StatusCode::OK);
    // A name that matches the Host header proves nothing: a rebound name
    // (evil.example → this server) matches too. Without a public URL only
    // IP addresses and localhost pass.
    assert_eq!(
        h.post(Some(&h.admin), &[("origin", "http://vole.test")], ping)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, _, body) = h
        .post(Some(&h.admin), &[("origin", "https://evil.example")], ping)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        h.post(Some(&h.admin), &[("origin", "null")], ping).await.0,
        StatusCode::FORBIDDEN
    );
    // With a public URL, its origin is the one browsers may use.
    h.state
        .with_conn(|c| {
            crate::db::settings::set(c, crate::db::settings::BASE_URL, "http://vole.test")
        })
        .await
        .unwrap();
    assert_eq!(
        h.post(Some(&h.admin), &[("origin", "http://vole.test")], ping)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        h.post(Some(&h.admin), &[("origin", "https://vole.test")], ping)
            .await
            .0,
        StatusCode::FORBIDDEN,
        "another scheme is another origin"
    );
    // An unsupported protocol version header is a 400.
    assert_eq!(
        h.post(
            Some(&h.admin),
            &[("mcp-protocol-version", "1999-01-01")],
            ping
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(
            Some(&h.admin),
            &[("mcp-protocol-version", "2025-06-18")],
            ping
        )
        .await
        .0,
        StatusCode::OK
    );

    // No server-initiated stream: GET and DELETE are 405 with Allow: POST.
    for method in [Method::GET, Method::DELETE] {
        let req = Request::builder()
            .method(method)
            .uri("/mcp")
            .header(header::HOST, "vole.test")
            .body(Body::empty())
            .unwrap();
        let res = h.app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(res.headers().get(header::ALLOW).unwrap(), "POST");
    }
    // Oversized bodies are refused.
    let big = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{"x":"{}"}}}}"#,
        "a".repeat(300 * 1024)
    );
    assert_eq!(
        h.post(Some(&h.admin), &[], &big).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn initialize_negotiates_the_version_and_notifications_get_202() {
    let h = harness().await;
    let r = h.rpc(&h.admin, "initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}})).await;
    assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(r["result"]["serverInfo"]["name"], "vigilant-vole");
    assert!(
        r["result"]["capabilities"]["tools"].is_object()
            && r["result"]["capabilities"]["resources"].is_object()
    );
    assert!(
        r["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("instance_summary")
    );
    // An unknown version gets our newest; a read token is told it is read-only.
    let r = h
        .rpc(
            &h.admin_read,
            "initialize",
            json!({"protocolVersion": "2030-01-01"}),
        )
        .await;
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    assert!(
        r["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("read-only")
    );

    let (status, _, body) = h
        .post(
            Some(&h.admin),
            &[],
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;
    assert_eq!((status, body.as_str()), (StatusCode::ACCEPTED, ""));
}

#[tokio::test]
async fn malformed_and_unknown_requests_get_json_rpc_errors() {
    let h = harness().await;
    let (status, _, body) = h.post(Some(&h.admin), &[], "{nope").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
        -32700
    );
    let r = h.rpc(&h.admin, "no/such/method", json!({})).await;
    assert_eq!(r["error"]["code"], -32601);
    let r = h
        .rpc(&h.admin, "tools/call", json!({"name": "no_such_tool"}))
        .await;
    assert_eq!(r["error"]["code"], -32602);
    let r = h.rpc(&h.admin, "tools/call", json!({})).await;
    assert_eq!(r["error"]["code"], -32602);
    let (_, _, body) = h
        .post(Some(&h.admin), &[], r#"{"id": 3, "method": "ping"}"#)
        .await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
        -32600,
        "jsonrpc must be 2.0"
    );
    // Batches (dropped by MCP 2025-06-18) are refused as a whole: one
    // request can't carry a pile of tool calls past the rate limit.
    let batch = r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":"b","method":"ping"}]"#;
    for body in [batch, "[]"] {
        let (status, _, reply) = h.post(Some(&h.admin), &[], body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{reply}");
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["error"]["code"], -32600, "{reply}");
        assert!(reply["id"].is_null());
    }
}

#[test]
fn browser_origins_need_the_public_url_or_a_fixed_address() {
    fn h(pairs: &[(&'static str, &'static str)]) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, axum::http::HeaderValue::from_static(v));
        }
        map
    }
    // No browser, no check.
    assert!(origin_allowed(&h(&[("host", "vole.test")]), None));
    // Without a public URL: IP addresses and localhost on the Host they reach.
    for (host, origin) in [
        ("127.0.0.1:3001", "http://127.0.0.1:3001"),
        ("localhost:3001", "http://localhost:3001"),
        ("[::1]:3001", "http://[::1]:3001"),
        ("192.168.1.5", "http://192.168.1.5"),
    ] {
        let map = {
            let mut m = axum::http::HeaderMap::new();
            m.insert("host", host.parse().unwrap());
            m.insert("origin", origin.parse().unwrap());
            m
        };
        assert!(origin_allowed(&map, None), "{origin}");
    }
    for (host, origin) in [
        ("vole.test", "http://vole.test"),
        ("127.0.0.1:3001", "http://127.0.0.1:3002"),
        ("127.0.0.1", "null"),
    ] {
        let map = {
            let mut m = axum::http::HeaderMap::new();
            m.insert("host", host.parse().unwrap());
            m.insert("origin", origin.parse().unwrap());
            m
        };
        assert!(!origin_allowed(&map, None), "{origin}");
    }
    // With one, exactly its origin (default ports normalised), whatever the Host.
    let base = Some("https://status.example.com/");
    assert!(origin_allowed(
        &h(&[
            ("host", "internal:3001"),
            ("origin", "https://status.example.com:443")
        ]),
        base
    ));
    assert!(!origin_allowed(
        &h(&[("host", "127.0.0.1"), ("origin", "http://127.0.0.1")]),
        base
    ));
    assert!(origin_allowed(
        &h(&[("referer", "https://status.example.com/settings")]),
        base
    ));
}

#[test]
fn tokens_are_rate_limited_per_window() {
    let t0 = Instant::now();
    for i in 0..RATE_LIMIT {
        assert_eq!(rate_limited(777_001, 1, t0), None, "request {i}");
    }
    let wait = rate_limited(777_001, 1, t0).expect("over the limit");
    assert!(wait >= Duration::from_secs(1) && wait <= RATE_WINDOW);
    assert_eq!(
        rate_limited(777_001, 2, t0),
        None,
        "other tokens are unaffected"
    );
    assert_eq!(
        rate_limited(777_002, 1, t0),
        None,
        "other instances are unaffected"
    );
    assert_eq!(
        rate_limited(777_001, 1, t0 + RATE_WINDOW),
        None,
        "a new window starts over"
    );
}

#[tokio::test]
async fn too_many_requests_answer_429_with_retry_after() {
    let h = harness().await;
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let mut last = StatusCode::OK;
    let mut retry = None;
    for _ in 0..=RATE_LIMIT {
        let (status, headers, _) = h.post(Some(&h.admin_read), &[], ping).await;
        last = status;
        retry = headers.get(header::RETRY_AFTER).cloned();
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    assert!(retry.is_some());
    // The other token keeps working.
    assert_eq!(h.post(Some(&h.admin), &[], ping).await.0, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_tokens_see_only_read_tools_and_cannot_call_write_tools() {
    let h = harness().await;
    let read = tool_names(&h.rpc(&h.admin_read, "tools/list", json!({})).await);
    let write = tool_names(&h.rpc(&h.editor, "tools/list", json!({})).await);
    let viewer = tool_names(&h.rpc(&h.viewer_write, "tools/list", json!({})).await);
    for name in [
        "instance_summary",
        "list_monitors",
        "get_monitor",
        "monitor_history",
        "list_events",
        "why_is_it_down",
        "list_status_pages",
        "list_incidents",
        "list_maintenance",
    ] {
        assert!(
            read.contains(&name.to_string()),
            "{name} missing from {read:?}"
        );
    }
    for name in [
        "create_monitor",
        "update_monitor",
        "pause_monitor",
        "resume_monitor",
        "check_now",
        "bulk_edit",
        "create_incident",
        "update_incident",
        "start_maintenance",
        "end_maintenance",
    ] {
        assert!(
            !read.contains(&name.to_string()) && !viewer.contains(&name.to_string()),
            "{name} offered to a read-only caller"
        );
        assert!(
            write.contains(&name.to_string()),
            "{name} missing for an editor"
        );
    }
    // No tool deletes data or touches users, settings, tokens or backups.
    for name in &write {
        for forbidden in [
            "delete", "remove", "user", "setting", "token", "backup", "restore", "import",
        ] {
            assert!(!name.contains(forbidden), "{name} looks dangerous");
        }
    }
    // Every tool has a description, a schema and the read-only hint matches.
    let list = h.rpc(&h.editor, "tools/list", json!({})).await;
    for t in list["result"]["tools"].as_array().unwrap() {
        assert!(
            t["description"].as_str().unwrap().len() > 20 && t["inputSchema"]["type"] == "object",
            "{t}"
        );
        let writes = !read.contains(&t["name"].as_str().unwrap().to_string());
        assert_eq!(t["annotations"]["readOnlyHint"], !writes, "{t}");
    }

    // Calling a write tool anyway is refused, with advice.
    let (err, text) = h
        .tool(
            &h.admin_read,
            "create_monitor",
            json!({"name": "x", "monitor_type": "http", "target": "https://example.com"}),
        )
        .await;
    assert!(err && text.contains("read-only"), "{text}");
    let (err, text) = h
        .tool(&h.viewer_write, "pause_monitor", json!({"id": 1}))
        .await;
    assert!(err && text.contains("read-only"), "{text}");
    let monitors = h
        .rest(Method::GET, "/api/v1/monitors", &h.admin, None)
        .await;
    assert_eq!(
        monitors["data"].as_array().unwrap().len(),
        0,
        "nothing was created"
    );
}

#[tokio::test]
async fn monitors_can_be_created_listed_inspected_paused_and_changed() {
    let h = harness().await;
    let m = h.tool_json(&h.editor, "create_monitor", json!({"name": "Site", "monitor_type": "http", "target": "https://example.com", "interval_seconds": 60})).await;
    let id = m["id"].as_i64().unwrap();
    assert_eq!(m["name"], "Site");

    let list = h
        .tool_json(&h.admin_read, "list_monitors", json!({"q": "sit"}))
        .await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    let none = h
        .tool_json(&h.admin_read, "list_monitors", json!({"status": "down"}))
        .await;
    assert_eq!(none["data"].as_array().unwrap().len(), 0);
    let one = h
        .tool_json(&h.admin_read, "get_monitor", json!({"id": id}))
        .await;
    assert_eq!(one["target"], "https://example.com");

    let paused = h
        .tool_json(&h.editor, "pause_monitor", json!({"id": id}))
        .await;
    assert_eq!(paused["status"], "paused");
    let resumed = h
        .tool_json(&h.editor, "resume_monitor", json!({"id": id}))
        .await;
    assert_ne!(resumed["status"], "paused");
    let changed = h
        .tool_json(
            &h.editor,
            "update_monitor",
            json!({"id": id, "interval_seconds": 120, "retries": 2}),
        )
        .await;
    assert_eq!(
        (
            changed["interval_seconds"].as_i64(),
            changed["retries"].as_i64()
        ),
        (Some(120), Some(2))
    );

    // Errors from the REST API come back as tool errors with the field named.
    let (err, text) = h
        .tool(
            &h.editor,
            "create_monitor",
            json!({"name": "Bad", "monitor_type": "http", "target": "not a url"}),
        )
        .await;
    assert!(err && text.contains("target"), "{text}");
    let (err, text) = h.tool(&h.editor, "update_monitor", json!({"id": id})).await;
    assert!(err && text.contains("at least one"), "{text}");
    let (err, text) = h
        .tool(&h.admin_read, "get_monitor", json!({"id": 9999}))
        .await;
    assert!(err && text.contains("404"), "{text}");
    let (err, text) = h
        .tool(&h.admin_read, "get_monitor", json!({"id": "x"}))
        .await;
    assert!(err && text.contains("positive integer"), "{text}");
    let (err, text) = h
        .tool(&h.admin_read, "list_monitors", json!({"limit": 5000}))
        .await;
    assert!(err && text.contains("limit"), "{text}");
}

#[tokio::test]
async fn secrets_are_always_masked() {
    let h = harness().await;
    let m = h
        .tool_json(
            &h.editor,
            "create_monitor",
            json!({
                "name": "Secret site", "monitor_type": "http", "target": "https://deploy:hunter2@example.com/health",
                "basic_auth_user": "deploy", "basic_auth_pass": "hunter2-basic",
                "http_headers": {"Authorization": "Bearer abc123xyz", "X-Trace": "visible-trace"}
            }),
        )
        .await;
    let id = m["id"].as_i64().unwrap();
    let push = h
        .tool_json(
            &h.editor,
            "create_monitor",
            json!({"name": "Beat", "monitor_type": "push"}),
        )
        .await;
    let push_token = h
        .rest(
            Method::GET,
            &format!("/api/v1/monitors/{}", push["id"]),
            &h.editor,
            None,
        )
        .await["push_token"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        !push_token.is_empty() && push_token != "••••••",
        "the REST API shows editors their push token"
    );
    for (token, tool, args) in [
        (
            &h.editor,
            "create_monitor",
            json!({"name": "Again", "monitor_type": "http", "target": "https://x:hunter2@example.com", "basic_auth_pass": "hunter2-basic"}),
        ),
        (
            &h.editor,
            "update_monitor",
            json!({"id": id, "description": "touched"}),
        ),
        (&h.admin_read, "get_monitor", json!({"id": id})),
        (&h.admin_read, "list_monitors", json!({})),
        (&h.admin_read, "why_is_it_down", json!({"id": id})),
        (&h.admin_read, "get_monitor", json!({"id": push["id"]})),
        (&h.admin_read, "list_monitors", json!({"q": "beat"})),
    ] {
        let (err, text) = h.tool(token, tool, args).await;
        assert!(!err, "{tool}: {text}");
        for secret in ["hunter2", "abc123xyz", &push_token] {
            assert!(!text.contains(secret), "{tool} leaked {secret}: {text}");
        }
        assert!(!text.contains("hunter2-basic"), "{tool}: {text}");
    }
    let got = h
        .tool_json(&h.admin_read, "get_monitor", json!({"id": id}))
        .await;
    assert_eq!(got["basic_auth_pass"], "••••••");
    assert_eq!(got["http_headers"]["Authorization"], "••••••");
    assert_eq!(
        got["http_headers"]["X-Trace"], "••••••",
        "header values are hidden, names stay"
    );
    assert_eq!(got["target"], "https://deploy:••••••@example.com/health");
    // The resource view is masked too.
    let r = h
        .rpc(
            &h.admin_read,
            "resources/read",
            json!({"uri": format!("vigilant-vole://monitor/{id}")}),
        )
        .await;
    let text = r["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(
        !text.contains("hunter2") && !text.contains("abc123xyz") && text.contains("••••••"),
        "{text}"
    );
}

#[test]
fn masking_covers_nested_values_and_urls() {
    let mut v = json!({
        "name": "n", "api_key": "k", "nested": {"client_secret": "s", "list": [{"password": "p"}, {"fine": "x"}]},
        "url": "https://u:pw@h.example/p?q=1", "empty_password": "", "already": {"token": "••••••"},
        "snmp_community": "public", "monitor_key": "keep", "note": "mailto:a@b.example"
    });
    tools::mask(&mut v);
    assert_eq!(v["api_key"], "••••••");
    assert_eq!(v["nested"]["client_secret"], "••••••");
    assert_eq!(v["nested"]["list"][0]["password"], "••••••");
    assert_eq!(v["nested"]["list"][1]["fine"], "x");
    assert_eq!(v["url"], "https://u:••••••@h.example/p?q=1");
    assert_eq!(v["empty_password"], "");
    assert_eq!(v["snmp_community"], "••••••");
    assert_eq!(
        (v["monitor_key"].as_str(), v["note"].as_str()),
        (Some("keep"), Some("mailto:a@b.example"))
    );
}

#[tokio::test]
async fn history_and_diagnosis_tools_report_what_happened() {
    let h = harness().await;
    let m = h
        .tool_json(
            &h.editor,
            "create_monitor",
            json!({"name": "Flaky", "monitor_type": "http", "target": "https://example.com"}),
        )
        .await;
    let id = m["id"].as_i64().unwrap();
    h.state
        .with_conn(move |c| {
            crate::db::heartbeats::insert(c, id, None, crate::db::models::HeartbeatStatus::Up, Some(80), Some("200 OK"))?;
            crate::db::heartbeats::insert(c, id, None, crate::db::models::HeartbeatStatus::Down, None, Some("connection refused"))?;
            c.execute(
                "INSERT INTO monitor_events (monitor_id, kind, message, started_at) VALUES (?1, 'down', 'connection refused', datetime('now', '-10 minutes'))",
                [id],
            )?;
            c.execute(
                "INSERT INTO monitor_expiry (monitor_id, kind, subject, issuer, expires_at) VALUES (?1, 'cert', 'example.com', 'Test CA', datetime('now', '+9 days', '+1 hour'))",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let why = h
        .tool_json(&h.admin_read, "why_is_it_down", json!({"id": id}))
        .await;
    assert_eq!(why["monitor"]["name"], "Flaky");
    assert_eq!(why["ongoing_outage"]["message"], "connection refused");
    assert!(
        why["last_checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["message"] == "connection refused"),
        "{why}"
    );
    assert_eq!(why["expiry"][0]["kind"], "certificate");
    assert_eq!(why["expiry"][0]["days_left"], 9);

    let history = h
        .tool_json(
            &h.admin_read,
            "monitor_history",
            json!({"id": id, "days": 7, "checks": 5}),
        )
        .await;
    assert_eq!(history["days"], 7);
    assert!(history["uptime"]["uptime_pct"].is_number(), "{history}");
    assert_eq!(history["recent_checks"].as_array().unwrap().len(), 2);
    let no_checks = h
        .tool_json(
            &h.admin_read,
            "monitor_history",
            json!({"id": id, "checks": 0}),
        )
        .await;
    assert!(no_checks.get("recent_checks").is_none());

    let events = h
        .tool_json(&h.admin_read, "list_events", json!({"id": id}))
        .await;
    assert_eq!(events["data"][0]["kind"], "down");

    let summary = h
        .tool_json(&h.admin_read, "instance_summary", json!({}))
        .await;
    assert_eq!(summary["monitors"]["total"], 1);
    assert!(summary["version"].is_string());
    assert!(
        summary.get("agent_list").is_none(),
        "a read token acts as a viewer, and agent details are for admins"
    );
    let admin_summary = h.tool_json(&h.admin, "instance_summary", json!({})).await;
    assert!(
        admin_summary["agent_list"].is_array(),
        "an admin's write token sees the agents"
    );
    let viewer_summary = h
        .tool_json(&h.viewer_write, "instance_summary", json!({}))
        .await;
    assert!(
        viewer_summary.get("agent_list").is_none(),
        "agent details are for admins"
    );
}

#[tokio::test]
async fn bulk_edit_has_the_rest_semantics() {
    let h = harness().await;
    let mut ids = Vec::new();
    for name in ["a", "b", "c"] {
        let m = h.tool_json(&h.editor, "create_monitor", json!({"name": name, "monitor_type": "http", "target": "https://example.com", "interval_seconds": 60})).await;
        ids.push(m["id"].as_i64().unwrap());
    }
    let r = h
        .tool_json(
            &h.editor,
            "bulk_edit",
            json!({"ids": [ids[0], ids[1]], "set": {"interval_seconds": 300}}),
        )
        .await;
    assert_eq!(r["updated"], 2, "{r}");
    let list = h
        .rest(Method::GET, "/api/v1/monitors", &h.admin, None)
        .await;
    let interval = |id: i64| {
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap()["interval_seconds"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(
        (interval(ids[0]), interval(ids[1]), interval(ids[2])),
        (300, 300, 60)
    );
    let r = h
        .tool_json(
            &h.editor,
            "bulk_edit",
            json!({"filter": {"type": "http"}, "set": {"retries": 3}}),
        )
        .await;
    assert_eq!(r["updated"], 3);
    for bad in [
        json!({"set": {"retries": 1}}),
        json!({"ids": [1]}),
        json!({"ids": [1], "set": "x"}),
    ] {
        let (err, text) = h.tool(&h.editor, "bulk_edit", bad).await;
        assert!(err && text.contains("Invalid arguments"), "{text}");
    }
}

#[tokio::test]
async fn incidents_and_maintenance_round_trip() {
    let h = harness().await;
    let page = h
        .rest(
            Method::POST,
            "/api/v1/status-pages",
            &h.editor,
            Some(json!({"title": "Acme"})),
        )
        .await;
    let pid = page["id"].as_i64().unwrap();
    let m = h
        .tool_json(
            &h.editor,
            "create_monitor",
            json!({"name": "Site", "monitor_type": "http", "target": "https://example.com"}),
        )
        .await;
    let mid = m["id"].as_i64().unwrap();

    let pages = h
        .tool_json(&h.admin_read, "list_status_pages", json!({}))
        .await;
    assert_eq!(pages["data"][0]["slug"], "acme");

    let inc = h.tool_json(&h.editor, "create_incident", json!({"status_page_id": pid, "title": "Slow API", "body": "Looking", "severity": "warning"})).await;
    let iid = inc["id"].as_i64().unwrap();
    let r = h
        .tool_json(&h.editor, "update_incident", json!({"status_page_id": pid, "incident_id": iid, "update": {"status": "identified", "body": "A bad deploy"}}))
        .await;
    assert_eq!(r["update"]["status"], "identified");
    assert_eq!(r["incident"]["updates"].as_array().unwrap().len(), 1);
    let open = h
        .tool_json(&h.admin_read, "list_incidents", json!({"open_only": true}))
        .await;
    assert_eq!(open["incidents"][0]["title"], "Slow API");
    assert_eq!(open["incidents"][0]["status_page_slug"], "acme");
    let r = h
        .tool_json(&h.editor, "update_incident", json!({"status_page_id": pid, "incident_id": iid, "severity": "success", "update": {"status": "resolved", "body": "Fixed"}}))
        .await;
    assert!(r["incident"]["resolved_at"].is_string(), "{r}");
    let open = h
        .tool_json(
            &h.admin_read,
            "list_incidents",
            json!({"status_page_id": pid, "open_only": true}),
        )
        .await;
    assert!(open["incidents"].as_array().unwrap().is_empty());
    let all = h
        .tool_json(
            &h.admin_read,
            "list_incidents",
            json!({"status_page_id": pid}),
        )
        .await;
    assert_eq!(all["incidents"].as_array().unwrap().len(), 1);
    let (err, text) = h
        .tool(
            &h.editor,
            "update_incident",
            json!({"status_page_id": pid, "incident_id": iid}),
        )
        .await;
    assert!(err && text.contains("give fields"), "{text}");
    let (err, _) = h
        .tool(
            &h.editor,
            "update_incident",
            json!({"status_page_id": pid, "incident_id": 99, "title": "x"}),
        )
        .await;
    assert!(err);

    // Maintenance: start puts the monitor into maintenance, end takes it out.
    let w = h
        .tool_json(
            &h.editor,
            "start_maintenance",
            json!({"title": "DB upgrade", "monitor_ids": [mid], "status_page_ids": [pid]}),
        )
        .await;
    let wid = w["id"].as_i64().unwrap();
    assert_eq!(
        (w["strategy"].as_str(), w["active"].as_bool()),
        (Some("manual"), Some(true)),
        "{w}"
    );
    let listed = h
        .tool_json(&h.admin_read, "list_maintenance", json!({}))
        .await;
    assert_eq!(listed["data"][0]["title"], "DB upgrade");
    assert_eq!(listed["data"][0]["monitor_ids"], json!([mid]));
    let ended = h
        .tool_json(&h.editor, "end_maintenance", json!({"id": wid}))
        .await;
    assert_eq!(ended["active"], false);
    let (err, text) = h
        .tool(
            &h.editor,
            "start_maintenance",
            json!({"title": "x", "monitor_ids": []}),
        )
        .await;
    assert!(err && text.contains("monitor_ids"), "{text}");
}

#[tokio::test]
async fn check_now_runs_a_check_and_refuses_what_the_rest_api_refuses() {
    let h = harness().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let m = h
        .tool_json(
            &h.editor,
            "create_monitor",
            json!({"name": "Local", "monitor_type": "tcp", "target": addr.to_string()}),
        )
        .await;
    let id = m["id"].as_i64().unwrap();
    let r = h.tool_json(&h.editor, "check_now", json!({"id": id})).await;
    assert_eq!(r["status"], "up", "{r}");
    h.tool_json(&h.editor, "pause_monitor", json!({"id": id}))
        .await;
    let (err, text) = h.tool(&h.editor, "check_now", json!({"id": id})).await;
    assert!(err && text.contains("409"), "{text}");
    drop(listener);
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resources_serve_the_api_reference_and_monitors() {
    let h = harness().await;
    let m = h
        .tool_json(
            &h.editor,
            "create_monitor",
            json!({"name": "Site", "monitor_type": "http", "target": "https://example.com"}),
        )
        .await;
    let id = m["id"].as_i64().unwrap();

    let list = h.rpc(&h.admin_read, "resources/list", json!({})).await;
    let uris: Vec<&str> = list["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert!(
        uris.contains(&"vigilant-vole://api-reference")
            && uris.contains(&format!("vigilant-vole://monitor/{id}").as_str()),
        "{uris:?}"
    );
    let templates = h
        .rpc(&h.admin_read, "resources/templates/list", json!({}))
        .await;
    assert_eq!(
        templates["result"]["resourceTemplates"][0]["uriTemplate"],
        "vigilant-vole://monitor/{id}"
    );

    let doc = h
        .rpc(
            &h.admin_read,
            "resources/read",
            json!({"uri": "vigilant-vole://api-reference"}),
        )
        .await;
    let content = &doc["result"]["contents"][0];
    assert_eq!(content["mimeType"], "text/markdown");
    let text = content["text"].as_str().unwrap();
    assert!(
        text.contains("/api/v1/monitors") && text.contains("http://vole.test/mcp"),
        "{}",
        &text[..text.len().min(300)]
    );

    let monitor = h
        .rpc(
            &h.admin_read,
            "resources/read",
            json!({"uri": format!("vigilant-vole://monitor/{id}")}),
        )
        .await;
    let body: Value =
        serde_json::from_str(monitor["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["name"], "Site");

    for uri in [
        "vigilant-vole://monitor/99999",
        "vigilant-vole://nope",
        "vigilant-vole://monitor/abc",
        "file:///etc/passwd",
    ] {
        let r = h
            .rpc(&h.admin_read, "resources/read", json!({"uri": uri}))
            .await;
        assert_eq!(r["error"]["code"], -32002, "{uri}: {r}");
    }
}
