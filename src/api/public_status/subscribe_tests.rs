//! Router tests for status page subscriptions: sign-up, double opt-in,
//! unsubscribe, abuse limits, password pages and the editor tab.

use super::router_tests::{TestApp, app, get, login, seed, send};
use super::invalidate_views;
use crate::db::models::Role;
use crate::db::status_pages::extras;
use crate::db::subscribers::{self as db, Settings};
use axum::http::StatusCode;

const EMAIL_CONFIG: &str = r#"{"host":"127.0.0.1","port":1,"security":"none","from":"status@example.com","to":"x@example.com"}"#;

/// Subscriptions on for page `pid`, with an email channel and webhooks.
async fn enable(t: &TestApp, pid: i64, email: bool, webhooks: bool) {
    t.state
        .with_conn(move |c| {
            let channel = if email {
                Some(crate::db::notifications::insert_channel(c, "Mail", "email", EMAIL_CONFIG, false)?)
            } else {
                None
            };
            db::set_settings(c, pid, &Settings { enabled: true, channel_id: channel, webhooks })?;
            Ok(())
        })
        .await
        .unwrap();
    invalidate_views();
}

async fn subscribers(t: &TestApp, pid: i64) -> Vec<db::Subscriber> {
    t.state.with_conn(move |c| db::list(c, pid)).await.unwrap()
}

#[tokio::test]
async fn the_form_only_exists_when_the_page_offers_subscriptions() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    let (status, _, _) = get(&t, "/status/acme/subscribe", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, _, page) = get(&t, "/status/acme", None, None).await;
    assert!(!page.contains("/subscribe"), "no link while off: {page}");

    enable(&t, pid, true, false).await;
    let (status, headers, body) = get(&t, "/status/acme/subscribe", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("name=\"email\"") && !body.contains("name=\"webhook\""), "{body}");
    assert_eq!(headers.get("cache-control"), Some("no-store"));
    assert!(headers.get("x-robots-tag").is_some());
    let (_, _, page) = get(&t, "/status/acme", None, None).await;
    assert!(page.contains("href=\"/status/acme/subscribe\""), "{page}");

    // Webhooks only: no email field.
    t.state
        .with_conn(move |c| db::set_settings(c, pid, &Settings { enabled: true, channel_id: None, webhooks: true }))
        .await
        .unwrap();
    let (_, _, body) = get(&t, "/status/acme/subscribe", None, None).await;
    assert!(!body.contains("name=\"email\"") && body.contains("name=\"webhook\""), "{body}");

    // Unpublished pages offer nothing.
    t.state.with_conn(move |c| c.execute("UPDATE status_pages SET published = 0 WHERE id = ?1", [pid])).await.unwrap();
    invalidate_views();
    let (status, _, _) = get(&t, "/status/acme/subscribe", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn email_sign_up_needs_confirmation_and_leaks_nothing() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, false).await;

    let (status, _, first) = send(&t, "POST", "/status/acme/subscribe", "", "email=Jo%40Example.com").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert!(first.contains("Check your inbox"), "{first}");
    let subs = subscribers(&t, pid).await;
    assert_eq!(subs.len(), 1);
    assert_eq!((subs[0].target.as_str(), subs[0].kind.as_str(), subs[0].is_confirmed()), ("jo@example.com", "email", false));
    let token = subs[0].token.clone();

    // Pending: no announcements go to it.
    let confirmed = t.state.with_conn(move |c| db::list_confirmed(c, pid)).await.unwrap();
    assert!(confirmed.is_empty());

    // Opening the link (a mail scanner would) confirms nothing.
    let confirm_url = format!("/status/acme/subscribe/confirm/{token}");
    let (status, headers, body) = get(&t, &confirm_url, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("method=\"post\"") && body.contains("Confirm subscription"), "{body}");
    assert!(body.contains("<meta name=\"referrer\" content=\"no-referrer\">"), "{body}");
    let _ = headers;
    assert!(body.contains("jo***@example.com") && !body.contains("jo@example.com"), "masked: {body}");
    assert!(!subscribers(&t, pid).await[0].is_confirmed());

    let (status, _, body) = send(&t, "POST", &confirm_url, "", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("You&#39;re subscribed") || body.contains("You're subscribed"), "{body}");
    assert!(subscribers(&t, pid).await[0].is_confirmed());

    // Signing up again looks exactly the same and changes nothing.
    let (status, _, again) = send(&t, "POST", "/status/acme/subscribe", "", "email=jo%40example.com").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, again, "a known address must not look different");
    assert_eq!(subscribers(&t, pid).await.len(), 1);

    // Unsubscribe is a button too.
    let unsub = format!("/status/acme/subscribe/unsubscribe/{token}");
    let (status, _, body) = get(&t, &unsub, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("method=\"post\""), "{body}");
    assert_eq!(subscribers(&t, pid).await.len(), 1, "a GET removes nobody");
    let (status, _, body) = send(&t, "POST", &unsub, "", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No more updates"), "{body}");
    assert!(subscribers(&t, pid).await.is_empty());
    // Doing it twice is fine.
    let (status, _, _) = send(&t, "POST", &unsub, "", "").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn bad_sign_ups_are_turned_back() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, true).await;

    for bad in [
        "email=not-an-address",
        "email=a%40b.com+c%40d.com",
        "email=",
        "",
        "webhook=ftp%3A%2F%2Fexample.com%2Fh",
        "webhook=http%3A%2F%2Flocalhost%3A9%2Fh",
        "webhook=http%3A%2F%2F127.0.0.1%2Fh",
        "webhook=http%3A%2F%2F10.0.0.5%2Fh",
        "webhook=http%3A%2F%2F169.254.169.254%2Flatest",
        "webhook=http%3A%2F%2F%5B%3A%3A1%5D%2Fh",
        "webhook=https%3A%2F%2Fuser%3Apw%40example.com%2Fh",
    ] {
        let (status, _, body) = send(&t, "POST", "/status/acme/subscribe", "", bad).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
        assert!(body.contains("role=\"alert\""), "{bad}: {body}");
    }
    assert!(subscribers(&t, pid).await.is_empty());

    // The hidden field is for bots: they get the success page and nothing is stored.
    let (status, _, body) = send(&t, "POST", "/status/acme/subscribe", "", "email=a%40example.com&website=http%3A%2F%2Fspam").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Check your inbox"));
    assert!(subscribers(&t, pid).await.is_empty());

    // A webhook subscription is refused when the page doesn't take them.
    t.state.with_conn(move |c| db::set_settings(c, pid, &Settings { enabled: true, channel_id: None, webhooks: false })).await.unwrap();
    let (status, _, _) = send(&t, "POST", "/status/acme/subscribe", "", "webhook=https%3A%2F%2F93.184.216.34%2Fh").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn public_webhooks_can_subscribe() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, false, true).await;
    let (status, _, body) = send(&t, "POST", "/status/acme/subscribe", "", "webhook=https%3A%2F%2F93.184.216.34%2Fhook%3Fk%3Dsecret").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let subs = subscribers(&t, pid).await;
    assert_eq!((subs[0].kind.as_str(), subs[0].target.as_str()), ("webhook", "https://93.184.216.34/hook?k=secret"));
    // The confirmation page shows the host, never the secret path or query.
    let (_, _, body) = get(&t, &format!("/status/acme/subscribe/confirm/{}", subs[0].token), None, None).await;
    assert!(body.contains("https://93.184.216.34/…") && !body.contains("secret"), "{body}");
}

#[tokio::test]
async fn sign_ups_are_rate_limited() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, false).await;
    for n in 0..20 {
        let (status, _, _) = send(&t, "POST", "/status/acme/subscribe", "", &format!("email=u{n}%40example.com")).await;
        assert_eq!(status, StatusCode::OK, "attempt {n}");
    }
    let (status, _, body) = send(&t, "POST", "/status/acme/subscribe", "", "email=late%40example.com").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(subscribers(&t, pid).await.len(), 20);
}

#[tokio::test]
async fn tokens_only_work_on_their_own_page_and_are_not_guessable() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, false).await;
    send(&t, "POST", "/status/acme/subscribe", "", "email=a%40example.com").await;
    let token = subscribers(&t, pid).await[0].token.clone();
    for uri in [
        format!("/status/acme/subscribe/confirm/{}x", token),
        "/status/acme/subscribe/confirm/short".to_string(),
        format!("/status/other/subscribe/confirm/{token}"),
    ] {
        let (status, _, body) = get(&t, &uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
    }
    let (status, _, body) = get(&t, "/status/acme/subscribe/confirm/short", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("invalid or has expired"), "{body}");
    assert!(!subscribers(&t, pid).await[0].is_confirmed());
}

#[tokio::test]
async fn password_pages_ask_for_the_password_first() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, false).await;
    let hash = crate::auth::hash_password("correct horse");
    t.state.with_conn(move |c| extras::set_password_hash(c, pid, Some(&hash))).await.unwrap();
    invalidate_views();
    let (status, _, body) = get(&t, "/status/acme/subscribe", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!body.contains("name=\"email\""), "{body}");
    let (status, _, _) = send(&t, "POST", "/status/acme/subscribe", "", "email=a%40example.com").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(subscribers(&t, pid).await.is_empty());
}

#[tokio::test]
async fn the_editor_tab_manages_settings_and_subscribers() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    let ed = login(&t, "ed", Role::Editor).await;
    let viewer = login(&t, "view", Role::Viewer).await;
    let url = format!("/status-pages/{pid}/subscribers");

    let (status, _, _) = get(&t, &url, None, Some(&viewer)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, body) = get(&t, &url, None, Some(&ed)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Let visitors subscribe") && body.contains("No subscribers yet"), "{body}");

    // Only email channels can be picked.
    let (email_id, other_id) = t
        .state
        .with_conn(|c| {
            Ok((
                crate::db::notifications::insert_channel(c, "Ops mail", "email", EMAIL_CONFIG, false)?,
                crate::db::notifications::insert_channel(c, "Hook", "webhook", r#"{"url":"https://example.com/h"}"#, false)?,
            ))
        })
        .await
        .unwrap();
    let (status, _, _) = send(&t, "POST", &url, &viewer, "enabled=true").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(&t, "POST", &url, &ed, &format!("enabled=true&channel_id={other_id}")).await;
    assert!(status.is_redirection());
    assert!(!t.state.with_conn(move |c| db::settings(c, pid)).await.unwrap().enabled, "a non-email channel is refused");

    let (status, _, _) = send(&t, "POST", &url, &ed, &format!("enabled=true&webhooks=true&channel_id={email_id}")).await;
    assert!(status.is_redirection());
    let s = t.state.with_conn(move |c| db::settings(c, pid)).await.unwrap();
    assert_eq!((s.enabled, s.webhooks, s.channel_id), (true, true, Some(email_id)));
    let (_, _, body) = get(&t, &url, None, Some(&ed)).await;
    assert!(body.contains("Ops mail") && !body.contains(">Hook<"), "{body}");

    // The public page picks the change up right away.
    let (_, _, page) = get(&t, "/status/acme", None, None).await;
    assert!(page.contains("/status/acme/subscribe"), "{page}");

    // The list shows full addresses to editors and lets them remove one.
    send(&t, "POST", "/status/acme/subscribe", "", "email=visitor%40example.com").await;
    let sid = subscribers(&t, pid).await[0].id;
    let (_, _, body) = get(&t, &url, None, Some(&ed)).await;
    assert!(body.contains("visitor@example.com") && body.contains("Pending"), "{body}");
    let (status, _, _) = send(&t, "DELETE", &format!("{url}/{sid}"), &viewer, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(&t, "DELETE", &format!("{url}/{sid}"), &ed, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(subscribers(&t, pid).await.is_empty());
    let (status, _, _) = send(&t, "DELETE", &format!("{url}/{sid}"), &ed, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
