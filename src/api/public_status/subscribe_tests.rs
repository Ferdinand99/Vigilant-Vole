//! Router tests for status page subscriptions: sign-up, double opt-in,
//! unsubscribe, abuse limits, password pages and the editor tab.

use std::net::SocketAddr;

use super::invalidate_views;
use super::router_tests::{TestApp, app, get, login, seed, send};
use crate::db::models::Role;
use crate::db::status_pages::extras;
use crate::db::subscribers::{self as db, Settings};
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

const EMAIL_CONFIG: &str = r#"{"host":"127.0.0.1","port":1,"security":"none","from":"status@example.com","to":"x@example.com"}"#;

/// The public URL (Settings → General) links point at, or none.
async fn set_base(t: &TestApp, base: Option<&'static str>) {
    t.state
        .with_conn(move |c| match base {
            Some(b) => crate::db::settings::set(c, crate::db::settings::BASE_URL, b),
            None => crate::db::settings::delete(c, crate::db::settings::BASE_URL),
        })
        .await
        .unwrap();
    invalidate_views();
}

/// Subscriptions on for page `pid`, with an email channel and webhooks, and
/// a public URL for the links.
async fn enable(t: &TestApp, pid: i64, email: bool, webhooks: bool) {
    set_base(t, Some("https://status.example.com")).await;
    t.state
        .with_conn(move |c| {
            let channel = if email {
                Some(crate::db::notifications::insert_channel(
                    c,
                    "Mail",
                    "email",
                    EMAIL_CONFIG,
                    false,
                )?)
            } else {
                None
            };
            db::set_settings(
                c,
                pid,
                &Settings {
                    enabled: true,
                    channel_id: channel,
                    webhooks,
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    invalidate_views();
}

async fn subscribers(t: &TestApp, pid: i64) -> Vec<db::Subscriber> {
    t.state.with_conn(move |c| db::list(c, pid)).await.unwrap()
}

/// The link token of the page's newest subscriber (what its mail carries).
async fn newest_token(t: &TestApp, pid: i64) -> String {
    let key = t.state.session_key.clone();
    t.state
        .with_conn(move |c| {
            let sub = db::list(c, pid)?.remove(0);
            crate::subscribers::link_token(c, &key, &sub)
        })
        .await
        .unwrap()
}

/// A sign-up from `ip` (as the TCP peer), with a same-origin `Origin` and
/// any `Host`.
async fn sign_up_from(t: &TestApp, body: &str, ip: &str, host: &str) -> (StatusCode, String) {
    let mut req = Request::post("/status/acme/subscribe")
        .header(header::HOST, host)
        .header(header::ORIGIN, format!("http://{host}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap();
    let addr: SocketAddr = SocketAddr::new(ip.parse().unwrap(), 40000);
    req.extensions_mut().insert(ConnectInfo(addr));
    let resp = t.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body =
        String::from_utf8_lossy(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).to_string();
    (status, body)
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
    assert!(
        body.contains("name=\"email\"") && !body.contains("name=\"webhook\""),
        "{body}"
    );
    assert_eq!(headers.get("cache-control"), Some("no-store"));
    assert!(headers.get("x-robots-tag").is_some());
    let (_, _, page) = get(&t, "/status/acme", None, None).await;
    assert!(page.contains("href=\"/status/acme/subscribe\""), "{page}");

    // Webhooks only: no email field.
    t.state
        .with_conn(move |c| {
            db::set_settings(
                c,
                pid,
                &Settings {
                    enabled: true,
                    channel_id: None,
                    webhooks: true,
                },
            )
        })
        .await
        .unwrap();
    let (_, _, body) = get(&t, "/status/acme/subscribe", None, None).await;
    assert!(
        !body.contains("name=\"email\"") && body.contains("name=\"webhook\""),
        "{body}"
    );

    // Unpublished pages offer nothing.
    t.state
        .with_conn(move |c| c.execute("UPDATE status_pages SET published = 0 WHERE id = ?1", [pid]))
        .await
        .unwrap();
    invalidate_views();
    let (status, _, _) = get(&t, "/status/acme/subscribe", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn email_sign_up_needs_confirmation_and_leaks_nothing() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, false).await;

    let (status, _, first) = send(
        &t,
        "POST",
        "/status/acme/subscribe",
        "",
        "email=Jo%40Example.com",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert!(first.contains("Check your inbox"), "{first}");
    let subs = subscribers(&t, pid).await;
    assert_eq!(subs.len(), 1);
    assert_eq!(
        (
            subs[0].target.as_str(),
            subs[0].kind.as_str(),
            subs[0].is_confirmed()
        ),
        ("jo@example.com", "email", false)
    );
    let token = newest_token(&t, pid).await;
    assert!(token.starts_with("vvs_"), "{token}");
    assert_ne!(subs[0].token_hash, token, "only the hash is stored");

    // Pending: no announcements go to it.
    let confirmed = t
        .state
        .with_conn(move |c| db::list_confirmed(c, pid))
        .await
        .unwrap();
    assert!(confirmed.is_empty());

    // Opening the link (a mail scanner would) confirms nothing.
    let confirm_url = format!("/status/acme/subscribe/confirm/{token}");
    let (status, headers, body) = get(&t, &confirm_url, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("method=\"post\"") && body.contains("Confirm subscription"),
        "{body}"
    );
    // The token never leaks in a Referer to other sites, but the form still
    // works: under "no-referrer" browsers POST with `Origin: null`, which the
    // CSRF check refuses (the forms were unusable in a real browser).
    assert!(
        body.contains("<meta name=\"referrer\" content=\"same-origin\">"),
        "{body}"
    );
    let _ = headers;
    assert!(
        body.contains("jo***@example.com") && !body.contains("jo@example.com"),
        "masked: {body}"
    );
    assert!(!subscribers(&t, pid).await[0].is_confirmed());

    let (status, _, body) = send(&t, "POST", &confirm_url, "", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("You&#39;re subscribed") || body.contains("You're subscribed"),
        "{body}"
    );
    assert!(subscribers(&t, pid).await[0].is_confirmed());

    // Signing up again looks exactly the same and changes nothing.
    let (status, _, again) = send(
        &t,
        "POST",
        "/status/acme/subscribe",
        "",
        "email=jo%40example.com",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, again, "a known address must not look different");
    assert_eq!(subscribers(&t, pid).await.len(), 1);

    // Unsubscribe is a button too.
    let unsub = format!("/status/acme/subscribe/unsubscribe/{token}");
    let (status, _, body) = get(&t, &unsub, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("method=\"post\""), "{body}");
    assert_eq!(subscribers(&t, pid).await.len(), 1, "a GET removes nobody");
    // A mail client's one-click POST (RFC 8058) carries no Origin and no
    // cookie; the token is the proof.
    let req = Request::post(&unsub)
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("List-Unsubscribe=One-Click"))
        .unwrap();
    let resp = t.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body =
        String::from_utf8_lossy(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).to_string();
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
    let (status, _, body) = send(
        &t,
        "POST",
        "/status/acme/subscribe",
        "",
        "email=a%40example.com&website=http%3A%2F%2Fspam",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Check your inbox"));
    assert!(subscribers(&t, pid).await.is_empty());

    // A webhook subscription is refused when the page doesn't take them.
    t.state
        .with_conn(move |c| {
            db::set_settings(
                c,
                pid,
                &Settings {
                    enabled: true,
                    channel_id: None,
                    webhooks: false,
                },
            )
        })
        .await
        .unwrap();
    let (status, _, _) = send(
        &t,
        "POST",
        "/status/acme/subscribe",
        "",
        "webhook=https%3A%2F%2F93.184.216.34%2Fh",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn webhooks_wait_for_an_editor_and_cannot_confirm_themselves() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, false, true).await;
    let (status, _, body) = send(
        &t,
        "POST",
        "/status/acme/subscribe",
        "",
        "webhook=https%3A%2F%2F93.184.216.34%2Fhook%3Fk%3Dsecret",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Waiting for approval"), "{body}");
    let subs = subscribers(&t, pid).await;
    assert_eq!(
        (
            subs[0].kind.as_str(),
            subs[0].target.as_str(),
            subs[0].awaits_approval()
        ),
        ("webhook", "https://93.184.216.34/hook?k=secret", true)
    );
    // The link token (which only an approved webhook ever receives) does not
    // confirm a webhook: neither the page nor the POST.
    let token = newest_token(&t, pid).await;
    let confirm = format!("/status/acme/subscribe/confirm/{token}");
    let (status, _, body) = get(&t, &confirm, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!body.contains("secret"), "{body}");
    let (status, _, _) = send(&t, "POST", &confirm, "", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(subscribers(&t, pid).await[0].awaits_approval());

    // An editor approves it in the Subscribers tab.
    let ed = login(&t, "ed", Role::Editor).await;
    let viewer = login(&t, "view", Role::Viewer).await;
    let url = format!("/status-pages/{pid}/subscribers");
    let (_, _, tab) = get(&t, &url, None, Some(&ed)).await;
    let sid = subs[0].id;
    let approve = format!("{url}/{sid}/approve");
    assert!(
        tab.contains("Awaiting approval") && tab.contains(&approve),
        "{tab}"
    );
    let (status, _, _) = send(&t, "POST", &approve, &viewer, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(&t, "POST", &approve, &ed, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(subscribers(&t, pid).await[0].is_confirmed());
    let (status, _, _) = send(&t, "POST", &approve, &ed, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "only once");
    let (_, _, tab) = get(&t, &url, None, Some(&ed)).await;
    assert!(!tab.contains(&approve), "{tab}");
}

#[tokio::test]
async fn without_a_public_url_nothing_is_offered_and_the_host_header_is_never_used() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, true).await;
    set_base(&t, None).await;

    // The public page offers nothing; a sign-up through a forged Host is
    // refused and stores nothing (it used to keep that Host for its links).
    let (_, _, page) = get(&t, "/status/acme", None, None).await;
    assert!(!page.contains("/subscribe"), "{page}");
    let (status, _, _) = get(&t, "/status/acme/subscribe", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = sign_up_from(
        &t,
        "email=victim%40example.com",
        "198.51.100.7",
        "attacker.example",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(subscribers(&t, pid).await.is_empty());

    // The editor is told why, and admins get the way to Settings → General.
    let ed = login(&t, "ed", Role::Editor).await;
    let admin = login(&t, "boss", Role::Admin).await;
    let url = format!("/status-pages/{pid}/subscribers");
    let (_, _, tab) = get(&t, &url, None, Some(&ed)).await;
    assert!(
        tab.contains("Subscriptions need a public URL")
            && !tab.contains("href=\"/settings/general\""),
        "{tab}"
    );
    assert!(!tab.contains("Open the sign-up page"), "{tab}");
    let (_, _, tab) = get(&t, &url, None, Some(&admin)).await;
    assert!(tab.contains("href=\"/settings/general\""), "{tab}");

    // The page's own custom domain is an address the server vouches for.
    t.state
        .with_conn(move |c| {
            c.execute(
                "UPDATE status_pages SET custom_domain = 'status.acme.test' WHERE id = ?1",
                [pid],
            )
        })
        .await
        .unwrap();
    invalidate_views();
    let (status, _, _) = get(&t, "/status/acme/subscribe", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, tab) = get(&t, &url, None, Some(&ed)).await;
    assert!(!tab.contains("Subscriptions need a public URL"), "{tab}");

    // With a public URL the form is back, whatever Host a visitor sends.
    set_base(&t, Some("https://status.example.com")).await;
    let (status, _) = sign_up_from(
        &t,
        "email=reader%40example.com",
        "198.51.100.8",
        "attacker.example",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(subscribers(&t, pid).await.len(), 1);
}

#[tokio::test]
async fn ipv6_sign_ups_are_limited_per_64() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, false).await;
    for n in 1..=20 {
        let (status, _) = sign_up_from(
            &t,
            &format!("email=v6-{n}%40example.com"),
            &format!("2001:db8:1:2::{n:x}"),
            "localhost",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "attempt {n}");
    }
    // Another address of the same /64 is the same party.
    let (status, _) = sign_up_from(
        &t,
        "email=v6-late%40example.com",
        "2001:db8:1:2:ffff::1",
        "localhost",
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    // Another /64 is not.
    let (status, _) = sign_up_from(
        &t,
        "email=v6-other%40example.com",
        "2001:db8:1:3::1",
        "localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_page_takes_a_limited_number_of_sign_ups_per_hour() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, false).await;
    let budget = super::subscribe::PAGE_SIGNUPS_PER_HOUR;
    for n in 0..budget {
        // Every one from its own address: the per-address limit never hits.
        let (status, _) = sign_up_from(
            &t,
            &format!("email=p{n}%40example.com"),
            &format!("198.51.{}.{}", n / 200, n % 200 + 1),
            "localhost",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "sign-up {n}");
    }
    let (status, body) =
        sign_up_from(&t, "email=late%40example.com", "203.0.113.250", "localhost").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(subscribers(&t, pid).await.len(), budget as usize);
}

#[tokio::test]
async fn sign_ups_are_rate_limited() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, false).await;
    for n in 0..20 {
        let (status, _, _) = send(
            &t,
            "POST",
            "/status/acme/subscribe",
            "",
            &format!("email=u{n}%40example.com"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "attempt {n}");
    }
    let (status, _, body) = send(
        &t,
        "POST",
        "/status/acme/subscribe",
        "",
        "email=late%40example.com",
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(subscribers(&t, pid).await.len(), 20);
}

#[tokio::test]
async fn tokens_only_work_on_their_own_page_and_are_not_guessable() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, false).await;
    send(
        &t,
        "POST",
        "/status/acme/subscribe",
        "",
        "email=a%40example.com",
    )
    .await;
    let token = newest_token(&t, pid).await;
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
    t.state
        .with_conn(move |c| extras::set_password_hash(c, pid, Some(&hash)))
        .await
        .unwrap();
    invalidate_views();
    let (status, _, body) = get(&t, "/status/acme/subscribe", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!body.contains("name=\"email\""), "{body}");
    let (status, _, _) = send(
        &t,
        "POST",
        "/status/acme/subscribe",
        "",
        "email=a%40example.com",
    )
    .await;
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
    set_base(&t, Some("https://status.example.com")).await;
    let (status, _, body) = get(&t, &url, None, Some(&ed)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("Let visitors subscribe") && body.contains("No subscribers yet"),
        "{body}"
    );

    // Only email channels can be picked.
    let (email_id, other_id) = t
        .state
        .with_conn(|c| {
            Ok((
                crate::db::notifications::insert_channel(
                    c,
                    "Ops mail",
                    "email",
                    EMAIL_CONFIG,
                    false,
                )?,
                crate::db::notifications::insert_channel(
                    c,
                    "Hook",
                    "webhook",
                    r#"{"url":"https://example.com/h"}"#,
                    false,
                )?,
            ))
        })
        .await
        .unwrap();
    let (status, _, _) = send(&t, "POST", &url, &viewer, "enabled=true").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(
        &t,
        "POST",
        &url,
        &ed,
        &format!("enabled=true&channel_id={other_id}"),
    )
    .await;
    assert!(status.is_redirection());
    assert!(
        !t.state
            .with_conn(move |c| db::settings(c, pid))
            .await
            .unwrap()
            .enabled,
        "a non-email channel is refused"
    );

    let (status, _, _) = send(
        &t,
        "POST",
        &url,
        &ed,
        &format!("enabled=true&webhooks=true&channel_id={email_id}"),
    )
    .await;
    assert!(status.is_redirection());
    let s = t
        .state
        .with_conn(move |c| db::settings(c, pid))
        .await
        .unwrap();
    assert_eq!(
        (s.enabled, s.webhooks, s.channel_id),
        (true, true, Some(email_id))
    );
    let (_, _, body) = get(&t, &url, None, Some(&ed)).await;
    assert!(
        body.contains("Ops mail") && !body.contains(">Hook<"),
        "{body}"
    );

    // The public page picks the change up right away.
    let (_, _, page) = get(&t, "/status/acme", None, None).await;
    assert!(page.contains("/status/acme/subscribe"), "{page}");

    // The list shows full addresses to editors and lets them remove one.
    send(
        &t,
        "POST",
        "/status/acme/subscribe",
        "",
        "email=visitor%40example.com",
    )
    .await;
    let sid = subscribers(&t, pid).await[0].id;
    let (_, _, body) = get(&t, &url, None, Some(&ed)).await;
    assert!(
        body.contains("visitor@example.com") && body.contains("Pending"),
        "{body}"
    );
    let (status, _, _) = send(&t, "DELETE", &format!("{url}/{sid}"), &viewer, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(&t, "DELETE", &format!("{url}/{sid}"), &ed, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(subscribers(&t, pid).await.is_empty());
    let (status, _, _) = send(&t, "DELETE", &format!("{url}/{sid}"), &ed, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn setting_the_public_url_shows_the_subscribe_link_right_away() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    enable(&t, pid, true, false).await;
    set_base(&t, None).await;
    let (_, _, page) = get(&t, "/status/acme", None, None).await;
    assert!(!page.contains("/status/acme/subscribe"), "{page}");
    // Saved through Settings → General (no manual cache reset): the cached
    // public view must not hide the link for its lifetime.
    let admin = login(&t, "boss", Role::Admin).await;
    let (status, _, body) = send(
        &t,
        "POST",
        "/settings/general",
        &admin,
        "site_name=Vole&base_url=https%3A%2F%2Fstatus.example.com&timezone=UTC&language=en&retention_days=90",
    )
    .await;
    assert!(status.is_redirection(), "{status}: {body}");
    let (_, _, page) = get(&t, "/status/acme", None, None).await;
    assert!(page.contains("href=\"/status/acme/subscribe\""), "{page}");
}
