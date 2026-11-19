//! Boards against a fake Discord webhook and a fake Fluxer instance
//! (discovery document and webhook endpoints), on loopback.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::params;
use serde_json::{Value, json};

use super::runner::{self, Mode, Outcome};
use super::webhook::Client;
use super::*;
use crate::api::AppState;
use crate::db::live_boards::{self as boards, NewBoard};

const TOKEN: &str = "SECRETtoken123";

// ---------------------------------------------------------------------------
// The fake webhook server
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Fake {
    base: String,
    prefix: String,
    fluxer: bool,
    next: u64,
    messages: HashMap<String, Value>,
    deleted: HashSet<String>,
    /// "POST", "PATCH id", "DELETE id", "GET discovery".
    log: Vec<String>,
    /// Bodies of the posts, in order.
    posts: Vec<Value>,
    /// The next creates answer 429 with these `retry_after` values.
    rate_limits: Vec<f64>,
    /// Every create answers this (status, body).
    fail_create: Option<(u16, Value)>,
}

type Shared = Arc<Mutex<Fake>>;

async fn create(
    State(f): State<Shared>,
    Path((_id, token)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut f = f.lock().unwrap();
    assert_eq!(token, TOKEN);
    f.log.push("POST".into());
    f.posts.push(body.clone());
    if let Some((status, b)) = f.fail_create.clone() {
        return (StatusCode::from_u16(status).unwrap(), Json(b));
    }
    if !f.rate_limits.is_empty() {
        let wait = f.rate_limits.remove(0);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"message": "You are being rate limited.", "retry_after": wait})),
        );
    }
    f.next += 1;
    let id = (1000 + f.next).to_string();
    f.messages.insert(id.clone(), body);
    if q.get("wait").map(String::as_str) == Some("true") {
        (StatusCode::OK, Json(json!({"id": id})))
    } else {
        (StatusCode::NO_CONTENT, Json(Value::Null))
    }
}

async fn edit(
    State(f): State<Shared>,
    Path((_id, _token, mid)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut f = f.lock().unwrap();
    f.log.push(format!("PATCH {mid}"));
    if f.deleted.contains(&mid) || !f.messages.contains_key(&mid) {
        let code = if f.fluxer {
            json!("UNKNOWN_MESSAGE")
        } else {
            json!(10008)
        };
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"message": "Unknown Message", "code": code})),
        );
    }
    f.messages.insert(mid.clone(), body);
    (StatusCode::OK, Json(json!({"id": mid})))
}

async fn delete(
    State(f): State<Shared>,
    Path((_id, _token, mid)): Path<(String, String, String)>,
) -> StatusCode {
    let mut f = f.lock().unwrap();
    f.log.push(format!("DELETE {mid}"));
    f.messages.remove(&mid);
    StatusCode::NO_CONTENT
}

async fn discovery(State(f): State<Shared>) -> Json<Value> {
    let mut f = f.lock().unwrap();
    f.log.push("GET discovery".into());
    Json(json!({
        "api_code_version": 7,
        "endpoints": {"api_client": format!("{}{}", f.base, f.prefix), "api_public": format!("{}{}", f.base, f.prefix)},
    }))
}

/// Starts a fake server: `/{prefix}/webhooks/{id}/{token}` and, for Fluxer,
/// `/.well-known/fluxer`. Returns the shared state; its `base` is the
/// server's origin.
async fn fake(prefix: &str, fluxer: bool) -> Shared {
    let shared: Shared = Arc::new(Mutex::new(Fake {
        prefix: prefix.to_string(),
        fluxer,
        ..Fake::default()
    }));
    let hook = format!("{prefix}/webhooks/{{id}}/{{token}}");
    let mut app = Router::new().route(&hook, post(create)).route(
        &format!("{hook}/messages/{{mid}}"),
        axum::routing::patch(edit).delete(delete),
    );
    if fluxer {
        app = app.route("/.well-known/fluxer", get(discovery));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    shared.lock().unwrap().base = format!("http://{}", listener.local_addr().unwrap());
    let app = app.with_state(shared.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    shared
}

fn hook_url(f: &Shared) -> String {
    let f = f.lock().unwrap();
    format!("{}{}/webhooks/123456789/{TOKEN}", f.base, f.prefix)
}

fn log(f: &Shared) -> Vec<String> {
    f.lock().unwrap().log.clone()
}

fn clear_log(f: &Shared) {
    f.lock().unwrap().log.clear();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn state() -> AppState {
    crate::api::agents::test_support::test_state().await
}

async fn add_monitors(state: &AppState, n: usize, name_len: usize) -> Vec<i64> {
    state
        .with_conn(move |c| {
            let mut ids = Vec::new();
            for i in 0..n {
                c.execute(
                    "INSERT INTO monitors (name, monitor_type, target) VALUES (?1, 'http', 'https://secret-host.internal/x?token=abc')",
                    params![format!("Monitor {i:03} {}", "n".repeat(name_len))],
                )?;
                let id = c.last_insert_rowid();
                c.execute(
                    "INSERT INTO heartbeats (monitor_id, status, response_time_ms) VALUES (?1, 'up', 80)",
                    params![id],
                )?;
                ids.push(id);
            }
            Ok(ids)
        })
        .await
        .unwrap()
}

fn base_board(state: &AppState, url: &str, platform: &str, items: Vec<i64>) -> NewBoard {
    NewBoard {
        name: "Support".into(),
        webhook_url: crate::kit::notify::sealed::seal_value(&state.session_key, url),
        platform: platform.into(),
        source: "items".into(),
        status_page_id: None,
        show_uptime_24h: true,
        show_uptime_30d: true,
        show_response: true,
        show_incidents: true,
        title: "Support status".into(),
        description: String::new(),
        color: String::new(),
        refresh_seconds: 300,
        notify_down: false,
        mention: String::new(),
        lang: "en".into(),
        items,
    }
}

async fn add_board(state: &AppState, b: NewBoard) -> i64 {
    state
        .with_conn(move |c| boards::insert(c, &b))
        .await
        .unwrap()
}

async fn board(state: &AppState, id: i64) -> boards::Board {
    state
        .with_conn(move |c| boards::get(c, id))
        .await
        .unwrap()
        .unwrap()
}

async fn messages(state: &AppState, id: i64) -> Vec<boards::Message> {
    state
        .with_conn(move |c| boards::messages(c, id))
        .await
        .unwrap()
}

async fn set_down(state: &AppState, monitor: i64) {
    state
        .with_conn(move |c| {
            c.execute(
                "INSERT INTO heartbeats (monitor_id, status, response_time_ms, checked_at) VALUES (?1, 'down', 0, datetime('now', '+1 second'))",
                params![monitor],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn run(state: &AppState, client: &Client, id: i64, mode: Mode) -> Outcome {
    runner::run_board(state, client, id, mode).await
}

// ---------------------------------------------------------------------------
// Create, edit in place, 404, 429, delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_board_posts_once_and_then_edits_that_message_in_place() {
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 3, 5).await;
    let id = add_board(
        &state,
        base_board(&state, &hook_url(&fake), "discord", monitors),
    )
    .await;
    let client = Client::for_tests();

    assert_eq!(run(&state, &client, id, Mode::Manual).await, Outcome::Ok);
    assert_eq!(log(&fake), vec!["POST"]);
    let stored = messages(&state, id).await;
    assert_eq!(stored.len(), 1);
    let first_id = stored[0].message_id.clone();
    let posted = fake.lock().unwrap().posts[0].clone();
    assert_eq!(posted["allowed_mentions"], json!({"parse": []}));
    assert_eq!(posted["embeds"][0]["title"], "Support status");
    let text = posted.to_string();
    assert!(
        text.contains("Monitor 000")
            && !text.contains("secret-host")
            && !text.contains("token=abc")
    );

    // The interval edits the same message; nothing is posted.
    clear_log(&fake);
    assert_eq!(run(&state, &client, id, Mode::Interval).await, Outcome::Ok);
    assert_eq!(log(&fake), vec![format!("PATCH {first_id}")]);
    assert_eq!(messages(&state, id).await[0].message_id, first_id);

    // A change-driven refresh with nothing changed sends nothing.
    clear_log(&fake);
    assert_eq!(run(&state, &client, id, Mode::Change).await, Outcome::Ok);
    assert!(log(&fake).is_empty(), "{:?}", log(&fake));

    // Something changed: one edit, the message id stays.
    set_down(&state, monitors_first(&state).await).await;
    assert_eq!(run(&state, &client, id, Mode::Change).await, Outcome::Ok);
    assert_eq!(log(&fake), vec![format!("PATCH {first_id}")]);
    let b = board(&state, id).await;
    assert!(b.last_ok_at.is_some() && b.last_error.is_none() && b.failures == 0);
}

async fn monitors_first(state: &AppState) -> i64 {
    state
        .with_conn(|c| c.query_row("SELECT MIN(id) FROM monitors", [], |r| r.get(0)))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_deleted_message_is_posted_again_and_the_new_id_stored() {
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 2, 3).await;
    let id = add_board(
        &state,
        base_board(&state, &hook_url(&fake), "discord", monitors),
    )
    .await;
    let client = Client::for_tests();
    run(&state, &client, id, Mode::Manual).await;
    let old = messages(&state, id).await[0].message_id.clone();

    fake.lock().unwrap().deleted.insert(old.clone());
    clear_log(&fake);
    assert_eq!(run(&state, &client, id, Mode::Interval).await, Outcome::Ok);
    assert_eq!(log(&fake), vec![format!("PATCH {old}"), "POST".to_string()]);
    let new = messages(&state, id).await[0].message_id.clone();
    assert_ne!(new, old);
    // And it keeps editing the new one.
    clear_log(&fake);
    run(&state, &client, id, Mode::Interval).await;
    assert_eq!(log(&fake), vec![format!("PATCH {new}")]);
}

#[tokio::test]
async fn a_big_board_uses_several_messages_keeps_them_in_order_and_shrinks_again() {
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 160, 40).await;
    let id = add_board(
        &state,
        base_board(&state, &hook_url(&fake), "discord", monitors.clone()),
    )
    .await;
    let client = Client::for_tests();
    run(&state, &client, id, Mode::Manual).await;
    let stored = messages(&state, id).await;
    assert!(stored.len() >= 2, "{} messages", stored.len());
    for (i, m) in stored.iter().enumerate() {
        assert_eq!(m.slot, i as i64);
    }
    for body in &fake.lock().unwrap().posts {
        assert!(body["embeds"].as_array().unwrap().len() <= layout::MAX_EMBEDS);
        assert!(layout::message_chars(body) <= layout::MAX_MESSAGE_CHARS);
    }

    // Losing the first message posts it and everything after it anew, so the
    // channel keeps the board's order.
    let first = stored[0].message_id.clone();
    fake.lock().unwrap().deleted.insert(first.clone());
    clear_log(&fake);
    run(&state, &client, id, Mode::Interval).await;
    let l = log(&fake);
    assert_eq!(l[0], format!("PATCH {first}"));
    assert_eq!(
        l.iter().filter(|e| e.as_str() == "POST").count(),
        stored.len()
    );
    assert!(l.iter().any(|e| e.starts_with("DELETE")), "{l:?}");

    // Fewer monitors: the extra messages are deleted.
    let keep = monitors[..2].to_vec();
    let mut small = base_board(&state, &hook_url(&fake), "discord", keep);
    small.name = "Support".into();
    state
        .with_conn(move |c| boards::update(c, id, &small))
        .await
        .unwrap();
    clear_log(&fake);
    run(&state, &client, id, Mode::Interval).await;
    assert_eq!(messages(&state, id).await.len(), 1);
    assert!(
        log(&fake)
            .iter()
            .filter(|e| e.starts_with("DELETE"))
            .count()
            >= 1
    );
}

#[tokio::test]
async fn a_short_rate_limit_is_waited_out_and_a_long_one_defers_the_board() {
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 1, 3).await;
    let id = add_board(
        &state,
        base_board(&state, &hook_url(&fake), "discord", monitors),
    )
    .await;
    let client = Client::for_tests();

    fake.lock().unwrap().rate_limits = vec![0.2];
    let started = std::time::Instant::now();
    assert_eq!(run(&state, &client, id, Mode::Manual).await, Outcome::Ok);
    assert!(started.elapsed() >= Duration::from_millis(150));
    assert_eq!(log(&fake), vec!["POST", "POST"]);
    assert_eq!(messages(&state, id).await.len(), 1);

    // Delete it so the next run has to post, then limit for a minute.
    state
        .with_conn(move |c| boards::forget_messages(c, id))
        .await
        .unwrap();
    fake.lock().unwrap().rate_limits = vec![60.0];
    match run(&state, &client, id, Mode::Manual).await {
        Outcome::Deferred(wait) => assert!(wait >= Duration::from_secs(59), "{wait:?}"),
        other => panic!("{other:?}"),
    }
    // A rate limit is not a failure.
    let b = board(&state, id).await;
    assert_eq!(
        (b.failures, b.last_error.clone(), b.is_paused()),
        (0, None, false)
    );
}

#[tokio::test]
async fn a_webhook_that_is_gone_pauses_the_board_with_an_error_and_retry_starts_it() {
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 1, 3).await;
    let id = add_board(
        &state,
        base_board(&state, &hook_url(&fake), "discord", monitors),
    )
    .await;
    let client = Client::for_tests();
    fake.lock().unwrap().fail_create = Some((
        404,
        json!({"message": format!("Unknown Webhook {TOKEN}"), "code": 10015}),
    ));
    match run(&state, &client, id, Mode::Manual).await {
        Outcome::Failed { message, paused } => {
            assert!(paused);
            assert!(message.contains("404"), "{message}");
            assert!(!message.contains(TOKEN), "the token leaked: {message}");
        }
        other => panic!("{other:?}"),
    }
    let b = board(&state, id).await;
    assert!(b.is_paused());
    let error = b.last_error.clone().unwrap();
    assert!(
        !error.contains(TOKEN) && !error.contains("127.0.0.1"),
        "{error}"
    );
    // A paused board is left alone, except when somebody asks.
    clear_log(&fake);
    assert_eq!(
        run(&state, &client, id, Mode::Interval).await,
        Outcome::Skipped
    );
    assert!(log(&fake).is_empty());

    fake.lock().unwrap().fail_create = None;
    state
        .with_conn(move |c| boards::retry(c, id))
        .await
        .unwrap();
    assert_eq!(run(&state, &client, id, Mode::Manual).await, Outcome::Ok);
    let b = board(&state, id).await;
    assert!(!b.is_paused() && b.last_error.is_none());
}

#[tokio::test]
async fn repeated_server_errors_pause_after_five_failures() {
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 1, 3).await;
    let id = add_board(
        &state,
        base_board(&state, &hook_url(&fake), "discord", monitors),
    )
    .await;
    let client = Client::for_tests();
    fake.lock().unwrap().fail_create = Some((500, json!({"message": "oops"})));
    for n in 1..=boards::MAX_FAILURES {
        let out = run(&state, &client, id, Mode::Manual).await;
        let paused = matches!(out, Outcome::Failed { paused: true, .. });
        assert_eq!(paused, n == boards::MAX_FAILURES, "failure {n}: {out:?}");
    }
    assert!(board(&state, id).await.is_paused());
}

#[tokio::test]
async fn deleting_a_board_deletes_its_messages() {
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 150, 40).await;
    let id = add_board(
        &state,
        base_board(&state, &hook_url(&fake), "discord", monitors),
    )
    .await;
    let client = Client::for_tests();
    run(&state, &client, id, Mode::Manual).await;
    let stored = messages(&state, id).await;
    assert!(!stored.is_empty());
    clear_log(&fake);
    let b = board(&state, id).await;
    runner::delete_messages(&state, &client, &b).await;
    let l = log(&fake);
    assert_eq!(l.len(), stored.len());
    assert!(l.iter().all(|e| e.starts_with("DELETE")));
    assert!(fake.lock().unwrap().messages.is_empty());
}

// ---------------------------------------------------------------------------
// "Post a separate message when something goes down"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_down_message_is_posted_once_per_outage_with_only_the_chosen_role_mentioned() {
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 2, 3).await;
    let mut nb = base_board(&state, &hook_url(&fake), "discord", monitors.clone());
    nb.notify_down = true;
    nb.mention = "987654321".into();
    let id = add_board(&state, nb).await;
    let client = Client::for_tests();

    // The first refresh posts the board and says nothing about what is
    // already down.
    set_down(&state, monitors[0]).await;
    run(&state, &client, id, Mode::Manual).await;
    assert_eq!(fake.lock().unwrap().posts.len(), 1);
    assert_eq!(board(&state, id).await.known_down, vec![monitors[0]]);

    // Another monitor goes down: one extra message.
    set_down(&state, monitors[1]).await;
    run(&state, &client, id, Mode::Manual).await;
    let posts = fake.lock().unwrap().posts.clone();
    assert_eq!(posts.len(), 2);
    let notice = &posts[1];
    assert_eq!(notice["content"], "<@&987654321>");
    assert_eq!(
        notice["allowed_mentions"],
        json!({"parse": [], "roles": ["987654321"]})
    );
    assert!(
        notice["embeds"][0]["title"]
            .as_str()
            .unwrap()
            .contains("is down")
    );

    // Still down: nothing more.
    run(&state, &client, id, Mode::Manual).await;
    assert_eq!(fake.lock().unwrap().posts.len(), 2);
}

#[tokio::test]
async fn nothing_extra_is_posted_when_the_option_is_off() {
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 1, 3).await;
    let id = add_board(
        &state,
        base_board(&state, &hook_url(&fake), "discord", monitors.clone()),
    )
    .await;
    let client = Client::for_tests();
    run(&state, &client, id, Mode::Manual).await;
    set_down(&state, monitors[0]).await;
    run(&state, &client, id, Mode::Manual).await;
    assert_eq!(fake.lock().unwrap().posts.len(), 1);
    assert_eq!(board(&state, id).await.known_down, vec![monitors[0]]);
}

// ---------------------------------------------------------------------------
// Fluxer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fluxer_board_edits_in_place_after_the_instance_is_confirmed_once() {
    // Self-hosted shape: the API sits under /api.
    let fake = fake("/api", true).await;
    let state = state().await;
    let monitors = add_monitors(&state, 2, 3).await;
    let id = add_board(
        &state,
        base_board(&state, &hook_url(&fake), "fluxer", monitors),
    )
    .await;
    let client = Client::for_tests();

    assert_eq!(run(&state, &client, id, Mode::Manual).await, Outcome::Ok);
    assert_eq!(log(&fake), vec!["GET discovery", "POST"]);
    let mid = messages(&state, id).await[0].message_id.clone();

    clear_log(&fake);
    assert_eq!(run(&state, &client, id, Mode::Interval).await, Outcome::Ok);
    // Edited in place; the discovery answer is reused.
    assert_eq!(log(&fake), vec![format!("PATCH {mid}")]);

    // Fluxer's unknown-message error is told from a missing webhook too.
    fake.lock().unwrap().deleted.insert(mid.clone());
    clear_log(&fake);
    assert_eq!(run(&state, &client, id, Mode::Interval).await, Outcome::Ok);
    assert_eq!(log(&fake), vec![format!("PATCH {mid}"), "POST".to_string()]);
}

#[tokio::test]
async fn a_host_that_is_not_a_fluxer_instance_is_refused_and_the_board_pauses() {
    // A server with the webhook routes but no discovery document.
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 1, 3).await;
    let id = add_board(
        &state,
        base_board(&state, &hook_url(&fake), "fluxer", monitors),
    )
    .await;
    let client = Client::for_tests();
    match run(&state, &client, id, Mode::Manual).await {
        Outcome::Failed { message, paused } => {
            assert!(paused);
            assert!(message.contains("not a Fluxer instance"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // Nothing was posted.
    assert!(log(&fake).is_empty());
}

#[tokio::test]
async fn confirm_instance_checks_the_discovery_document_against_the_urls_api_base() {
    let client = Client::for_tests();
    let good = fake("/api", true).await;
    let t = client
        .parse(webhook::Platform::Fluxer, &hook_url(&good))
        .unwrap();
    assert!(client.confirm_instance(&t).await.is_ok());

    // Discovery names another API base: not this URL's instance.
    let other = fake("/other", true).await;
    let wrong = format!("{}/api/webhooks/1/{TOKEN}", other.lock().unwrap().base);
    let t = client.parse(webhook::Platform::Fluxer, &wrong).unwrap();
    assert!(matches!(
        client.confirm_instance(&t).await,
        Err(webhook::Failure::Permanent(_))
    ));
}

#[tokio::test]
async fn fluxer_addresses_that_name_the_host_itself_are_refused() {
    // (The strict client, not the test one. A name that resolves inside is
    // refused when it is resolved: see `webhook::tests`.)
    let strict = webhook::client();
    assert_eq!(
        strict
            .parse(
                webhook::Platform::Fluxer,
                &format!("https://localhost/api/webhooks/1/{TOKEN}")
            )
            .unwrap_err(),
        webhook::UrlError::Host
    );
}

// ---------------------------------------------------------------------------
// Validation, secrecy
// ---------------------------------------------------------------------------

fn valid_input() -> Input {
    Input {
        name: "Sylo".into(),
        webhook: Some("https://discord.com/api/webhooks/123456789/SECRETtoken123".into()),
        status_page_id: None,
        source: "items".into(),
        items: vec![1],
        ..Input::defaults()
    }
}

#[tokio::test]
async fn validation_seals_the_webhook_and_keeps_it_on_edit() {
    let state = state().await;
    add_monitors(&state, 1, 3).await;
    let key = state.session_key.clone();
    let k = key.clone();
    let stored = state
        .with_conn(move |c| validate(c, &k, None, &valid_input()))
        .await
        .unwrap()
        .expect("valid");
    assert!(crate::kit::crypto::is_sealed(&stored.webhook_url));
    assert!(!stored.webhook_url.contains(TOKEN));
    let id = add_board(&state, stored).await;

    // Editing without a webhook (or with the mask) keeps the saved one.
    let b = board(&state, id).await;
    for webhook in [None, Some(String::new()), Some(SECRET_MASK.to_string())] {
        let mut input = Input::from_board(&b, vec![1]);
        input.webhook = webhook;
        input.name = "Renamed".into();
        let (k, b2) = (key.clone(), b.clone());
        let kept = state
            .with_conn(move |c| validate(c, &k, Some(&b2), &input))
            .await
            .unwrap()
            .expect("valid");
        assert_eq!(kept.webhook_url, b.webhook_url);
    }

    // Under another instance key the saved webhook can't be kept.
    let other = axum_extra::extract::cookie::Key::generate();
    let input = Input::from_board(&b, vec![1]);
    let b2 = b.clone();
    let errors = state
        .with_conn(move |c| validate(c, &other, Some(&b2), &input))
        .await
        .unwrap()
        .expect_err("unreadable");
    assert!(errors.contains_key("webhook"));
}

#[tokio::test]
async fn validation_refuses_bad_hosts_pages_items_and_values() {
    let state = state().await;
    let key = state.session_key.clone();
    add_monitors(&state, 1, 3).await;
    let check = |input: Input| {
        let key = key.clone();
        let state = state.clone();
        async move {
            state
                .with_conn(move |c| validate(c, &key, None, &input))
                .await
                .unwrap()
        }
    };
    let mut bad = valid_input();
    bad.webhook = Some("https://evil.example/api/webhooks/1/x".into());
    assert!(check(bad).await.unwrap_err().contains_key("webhook"));

    let mut bad = valid_input();
    bad.webhook = None;
    assert!(check(bad).await.unwrap_err().contains_key("webhook"));

    let mut bad = valid_input();
    bad.items = vec![];
    assert!(check(bad).await.unwrap_err().contains_key("items"));

    let mut bad = valid_input();
    bad.items = vec![9999];
    assert!(check(bad).await.unwrap_err().contains_key("items"));

    let mut bad = valid_input();
    bad.source = "page".into();
    bad.status_page_id = Some(77);
    assert!(check(bad).await.unwrap_err().contains_key("status_page_id"));

    // A draft or password-protected status page can't be a source.
    state
        .with_conn(|c| {
            c.execute(
                "INSERT INTO status_pages (id, slug, title, published) VALUES (5, 'draft', 'Draft', 0)",
                [],
            )
        })
        .await
        .unwrap();
    let mut bad = valid_input();
    bad.source = "page".into();
    bad.status_page_id = Some(5);
    assert!(check(bad).await.unwrap_err().contains_key("status_page_id"));

    for (field, mutate) in [
        (
            "refresh_seconds",
            (|i: &mut Input| i.refresh_seconds = 59) as fn(&mut Input),
        ),
        ("refresh_seconds", |i| i.refresh_seconds = 100_000),
        ("mention", |i| i.mention = "@everyone".into()),
        ("color", |i| i.color = "#12".into()),
        ("lang", |i| i.lang = "xx".into()),
        ("name", |i| i.name = " ".into()),
        ("platform", |i| i.platform = "slack".into()),
        ("title", |i| i.title = "t".repeat(300)),
    ] {
        let mut bad = valid_input();
        mutate(&mut bad);
        assert!(check(bad).await.unwrap_err().contains_key(field), "{field}");
    }
    assert!(check(valid_input()).await.is_ok());
}

#[tokio::test]
async fn the_webhook_never_appears_in_the_error_a_board_shows() {
    // The address is refused for good (here: an address this platform does
    // not accept, stored under the wrong platform): the error names no URL.
    let fake = fake("/api", false).await;
    let state = state().await;
    let monitors = add_monitors(&state, 1, 3).await;
    let url = hook_url(&fake);
    let id = add_board(&state, base_board(&state, &url, "discord", monitors)).await;
    let strict = Client::for_tests();
    let _ = strict;
    // The production client refuses plain http: that is the board's error.
    let prod = webhook::client();
    match runner::run_board(&state, prod, id, Mode::Manual).await {
        Outcome::Failed { message, paused } => {
            assert!(paused);
            for secret in [TOKEN, "127.0.0.1", "webhooks"] {
                assert!(!message.contains(secret), "{message}");
            }
        }
        other => panic!("{other:?}"),
    }
}
