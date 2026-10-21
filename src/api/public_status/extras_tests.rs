//! Router tests for status page extras: logo/favicon uploads, password
//! protection and analytics.

use std::net::SocketAddr;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use super::router_tests::{HeaderMapLite, TestApp, app, assert_no_leaks, get, login, seed, send};
use super::{invalidate_domains, invalidate_views};
use crate::db::models::Role;
use crate::db::status_pages::extras::{self, Analytics, AssetKind, Provider};
use crate::db::{settings as db_settings, status_pages};

const FIREFOX: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:131.0) Gecko/20100101 Firefox/131.0";

fn png() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(b"\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89");
    v
}

fn ico() -> Vec<u8> {
    let mut v = vec![0, 0, 1, 0, 1, 0, 16, 16, 0, 0, 1, 0, 32, 0];
    v.extend_from_slice(&[0u8; 32]);
    v
}

/// Sends `req` (optionally from `ip`) and collects status, headers and body.
async fn call(
    t: &TestApp,
    mut req: Request<Body>,
    ip: Option<&str>,
) -> (StatusCode, HeaderMapLite, String) {
    if let Some(ip) = ip {
        let addr: SocketAddr = format!("{ip}:40000").parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
    }
    let resp = t.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = HeaderMapLite(
        resp.headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
    );
    let body =
        String::from_utf8_lossy(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).to_string();
    (status, headers, body)
}

fn multipart(bytes: &[u8], filename: &str, content_type: &str) -> (String, Vec<u8>) {
    let boundary = "vvtestboundary7MA4YWxkTrZu0gW".to_string();
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (boundary, body)
}

async fn upload(
    t: &TestApp,
    cookie: &str,
    pid: i64,
    kind: &str,
    bytes: &[u8],
    filename: &str,
    htmx: bool,
) -> (StatusCode, HeaderMapLite, String) {
    let (boundary, body) = multipart(bytes, filename, "image/png");
    let mut req = Request::post(format!("/status-pages/{pid}/assets/{kind}"))
        .header(header::HOST, "localhost")
        .header(header::ORIGIN, "http://localhost")
        .header(header::COOKIE, cookie)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        );
    if htmx {
        req = req.header("hx-request", "true");
    }
    call(t, req.body(Body::from(body)).unwrap(), None).await
}

fn set_cookie(headers: &HeaderMapLite, prefix: &str) -> Option<String> {
    headers
        .0
        .iter()
        .find(|(k, v)| k == "set-cookie" && v.starts_with(prefix))
        .map(|(_, v)| v.clone())
}

/// `name=value` of a `Set-Cookie` header.
fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_string()
}

async fn unlock(
    t: &TestApp,
    host: &str,
    password: &str,
    cookie: Option<&str>,
    ip: &str,
) -> (StatusCode, HeaderMapLite, String) {
    let mut req = Request::post("/status/acme/unlock")
        .header(header::HOST, host)
        .header(header::ORIGIN, format!("https://{host}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    let body = format!("password={}", password.replace(' ', "+"));
    call(t, req.body(Body::from(body)).unwrap(), Some(ip)).await
}

async fn set_password(t: &TestApp, pid: i64, password: Option<&str>) {
    let hash = password.map(crate::auth::hash_password);
    t.state
        .with_conn(move |c| extras::set_password_hash(c, pid, hash.as_deref()))
        .await
        .unwrap();
    invalidate_views();
}

async fn browse(
    t: &TestApp,
    uri: &str,
    ua: &str,
    ip: &str,
    cookie: Option<&str>,
    referer: Option<&str>,
) -> (StatusCode, HeaderMapLite, String) {
    let mut req = Request::get(uri)
        .header(header::HOST, "localhost")
        .header(header::USER_AGENT, ua);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    if let Some(r) = referer {
        req = req.header(header::REFERER, r);
    }
    call(t, req.body(Body::empty()).unwrap(), Some(ip)).await
}

// ---------------------------------------------------------------------------
// Logo and favicon
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uploads_are_validated_by_content_and_served_safely() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    let ed = login(&t, "ed", Role::Editor).await;

    let (status, headers, body) = upload(&t, &ed, pid, "logo", &png(), "logo.png", true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("id=\"sp-assets\""));
    assert!(body.contains("/status/acme/logo?v="));
    assert!(
        headers
            .get("hx-trigger")
            .is_some_and(|h| h.contains("Logo saved"))
    );

    // SVG, text dressed up as PNG and oversized files are refused (200 + error for htmx to swap).
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>";
    let (_, _, body) = upload(&t, &ed, pid, "favicon", svg, "icon.png", true).await;
    assert!(body.contains("SVG images aren"), "{body}");
    let (_, _, body) = upload(
        &t,
        &ed,
        pid,
        "favicon",
        b"<html>hi</html>",
        "icon.png",
        true,
    )
    .await;
    assert!(
        body.contains("Use a PNG, JPEG, WebP or ICO image."),
        "{body}"
    );
    let mut big = png();
    big.resize(extras::MAX_ASSET_BYTES + 10, 0);
    let (_, _, body) = upload(&t, &ed, pid, "favicon", &big, "big.png", true).await;
    assert!(body.contains("1 MB or less"), "{body}");
    big.resize(3 * extras::MAX_ASSET_BYTES, 0);
    let (status, _, body) = upload(&t, &ed, pid, "favicon", &big, "huge.png", true).await;
    assert!(
        status == StatusCode::OK && body.contains("1 MB or less")
            || status == StatusCode::PAYLOAD_TOO_LARGE,
        "{status}"
    );
    let stored = t
        .state
        .with_conn(move |c| extras::get_asset(c, pid, AssetKind::Favicon))
        .await
        .unwrap();
    assert!(stored.is_none());

    // Without htmx: flash + redirect back to the settings tab.
    let (status, headers, _) = upload(&t, &ed, pid, "favicon", &ico(), "favicon.ico", false).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get("location"),
        Some(format!("/status-pages/{pid}/edit?tab=settings").as_str())
    );

    // Viewers can't upload; unknown kinds don't exist.
    let viewer = login(&t, "vi", Role::Viewer).await;
    let (status, _, _) = upload(&t, &viewer, pid, "logo", &png(), "logo.png", true).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = upload(&t, &ed, pid, "banner", &png(), "logo.png", true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Serving: sniffed type, nosniff, locked-down CSP, ETag + 304, versioned = immutable.
    let (status, headers, _) = get(&t, "/status/acme/logo", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type"), Some("image/png"));
    assert_eq!(headers.get("x-content-type-options"), Some("nosniff"));
    assert_eq!(
        headers.get("content-security-policy"),
        Some("default-src 'none'; sandbox")
    );
    assert_eq!(headers.get("cache-control"), Some("public, no-cache"));
    let etag = headers.get("etag").unwrap().to_string();
    let req = Request::get("/status/acme/logo")
        .header(header::IF_NONE_MATCH, &etag)
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = call(&t, req, None).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
    let page = t
        .state
        .with_conn(move |c| status_pages::get(c, pid))
        .await
        .unwrap()
        .unwrap();
    let (_, headers, _) = get(&t, &page.logo_url().unwrap(), None, None).await;
    assert_eq!(
        headers.get("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    let (_, headers, _) = get(&t, "/status/acme/favicon", None, None).await;
    assert_eq!(headers.get("content-type"), Some("image/x-icon"));

    // The public page shows both.
    let (_, _, body) = get(&t, "/status/acme", None, None).await;
    assert!(
        body.contains(&format!(
            "<img class=\"public-logo\" src=\"{}\" alt=\"Acme status\"",
            page.logo_url().unwrap()
        )),
        "{body}"
    );
    assert!(body.contains(&format!(
        "<link rel=\"icon\" href=\"{}\">",
        page.favicon_url().unwrap()
    )));
    assert!(!body.contains("/static/favicon.svg"));

    // Remove → gone.
    let (status, _, body) = send(
        &t,
        "DELETE",
        &format!("/status-pages/{pid}/assets/logo"),
        &ed,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("/status/acme/logo?v="));
    let (status, _, _) = get(&t, "/status/acme/logo", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The settings tab renders the section.
    let (status, _, body) = get(
        &t,
        &format!("/status-pages/{pid}/edit?tab=settings"),
        None,
        Some(&ed),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("id=\"sp-assets\"") && body.contains("hx-encoding=\"multipart/form-data\"")
    );
}

#[tokio::test]
async fn custom_domains_serve_logo_and_favicon() {
    let t = app().await;
    let (pid, _) = seed(&t, true, Some("status.acme.test")).await;
    t.state
        .with_conn(move |c| {
            let a = extras::validate_image(png()).unwrap();
            extras::set_asset(c, pid, AssetKind::Logo, &a)?;
            extras::set_asset(
                c,
                pid,
                AssetKind::Favicon,
                &extras::validate_image(ico()).unwrap(),
            )
        })
        .await
        .unwrap();
    invalidate_domains();
    let (status, headers, _) = get(&t, "/status/acme/logo", Some("status.acme.test"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type"), Some("image/png"));
    let (status, headers, _) = get(&t, "/favicon.ico", Some("status.acme.test"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type"), Some("image/x-icon"));
    let (_, _, body) = get(&t, "/", Some("status.acme.test"), None).await;
    assert!(body.contains("class=\"public-logo\""));
    // Other hosts don't get the page's icon at /favicon.ico.
    let (_, headers, _) = get(&t, "/favicon.ico", Some("localhost"), None).await;
    assert_ne!(headers.get("content-type"), Some("image/x-icon"));
}

#[tokio::test]
async fn draft_page_assets_are_hidden_from_visitors() {
    let t = app().await;
    let (pid, _) = seed(&t, false, None).await;
    t.state
        .with_conn(move |c| {
            extras::set_asset(
                c,
                pid,
                AssetKind::Logo,
                &extras::validate_image(png()).unwrap(),
            )
        })
        .await
        .unwrap();
    let (status, _, _) = get(&t, "/status/acme/logo", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let ed = login(&t, "ed", Role::Editor).await;
    let (status, _, _) = get(&t, "/status/acme/logo", None, Some(&ed)).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Password protection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn password_unlock_flow_cookie_scope_and_revocation() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    let ed = login(&t, "ed", Role::Editor).await;

    // Too short is refused; a good one is saved through the editor.
    let base =
        "title=Acme+status&slug=acme&theme=auto&refresh_seconds=60&published=true&show_uptime=true";
    let (status, _, body) = send(
        &t,
        "POST",
        &format!("/status-pages/{pid}/edit"),
        &ed,
        &format!("{base}&page_password=short"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("Use at least 8 characters."));
    let (status, _, _) = send(
        &t,
        "POST",
        &format!("/status-pages/{pid}/edit"),
        &ed,
        &format!("{base}&page_password=correct+horse"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let page = t
        .state
        .with_conn(move |c| status_pages::get(c, pid))
        .await
        .unwrap()
        .unwrap();
    assert!(
        page.password_hash
            .as_deref()
            .is_some_and(|h| h.starts_with("$argon2id$"))
    );
    // Saving other settings with an empty password field keeps it.
    let (status, _, _) = send(&t, "POST", &format!("/status-pages/{pid}/edit"), &ed, base).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, body) = get(
        &t,
        &format!("/status-pages/{pid}/edit?tab=settings"),
        None,
        Some(&ed),
    )
    .await;
    assert!(body.contains("Remove the password"));

    // Visitors see the unlock form, nothing else, and it isn't indexed.
    let (status, headers, body) =
        browse(&t, "/status/acme", FIREFOX, "198.51.100.1", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("This page is password protected"));
    assert!(body.contains("action=\"/status/acme/unlock\""));
    assert!(!body.contains("Website") && !body.contains("Heads up"));
    assert_eq!(headers.get("x-robots-tag"), Some("noindex"));
    assert_eq!(headers.get("cache-control"), Some("no-store"));
    assert_no_leaks(&body);

    // Wrong password.
    let (status, headers, body) =
        unlock(&t, "localhost", "wrong horse", None, "198.51.100.1").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("That password isn"));
    assert!(set_cookie(&headers, "vv_sp_").is_none());

    // Right password: signed cookie scoped to the page, then the page shows.
    let (status, headers, _) = unlock(&t, "localhost", "correct horse", None, "198.51.100.1").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get("location"), Some("/status/acme"));
    let sc = set_cookie(&headers, &format!("vv_sp_{pid}=")).expect("unlock cookie");
    for attr in [
        "Path=/status/acme",
        "HttpOnly",
        "SameSite=Lax",
        "Max-Age=2592000",
    ] {
        assert!(sc.contains(attr), "{sc} lacks {attr}");
    }
    let unlocked = cookie_pair(&sc);
    let (status, headers, body) = browse(
        &t,
        "/status/acme",
        FIREFOX,
        "198.51.100.1",
        Some(&unlocked),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Website"));
    assert!(body.contains("<meta name=\"robots\" content=\"noindex\">"));
    assert_eq!(headers.get("x-robots-tag"), Some("noindex"));
    assert_eq!(headers.get("cache-control"), Some("private, no-store"));
    let (status, _, body) = browse(
        &t,
        "/status/acme/fragment",
        FIREFOX,
        "198.51.100.1",
        Some(&unlocked),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Website"));

    // A forged (unsigned) cookie doesn't work.
    let forged = format!(
        "vv_sp_{pid}={pid}.{}.{}",
        page.password_version,
        crate::auth::session::now_unix()
    );
    let (status, _, _) = browse(
        &t,
        "/status/acme",
        FIREFOX,
        "198.51.100.1",
        Some(&forged),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Logged-in users bypass the lock.
    let (status, _, body) = get(&t, "/status/acme", None, Some(&ed)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Website"));

    // A new password revokes the cookie; removing the password makes the page public.
    let (status, _, _) = send(
        &t,
        "POST",
        &format!("/status-pages/{pid}/edit"),
        &ed,
        &format!("{base}&page_password=battery+staple"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, _) = browse(
        &t,
        "/status/acme",
        FIREFOX,
        "198.51.100.1",
        Some(&unlocked),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send(
        &t,
        "POST",
        &format!("/status-pages/{pid}/edit"),
        &ed,
        &format!("{base}&remove_password=true"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, headers, body) =
        browse(&t, "/status/acme", FIREFOX, "198.51.100.1", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Website"));
    assert_eq!(headers.get("x-robots-tag"), None);
}

#[tokio::test]
async fn unlock_is_rate_limited_per_page_and_ip_and_same_origin_only() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    set_password(&t, pid, Some("correct horse")).await;
    let ip = "198.51.100.77";
    let mut throttled = false;
    for _ in 0..8 {
        let (status, _, body) = unlock(&t, "localhost", "nope nope", None, ip).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            assert!(
                body.contains("Too many") || body.contains("try again"),
                "{body}"
            );
            throttled = true;
            break;
        }
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert!(throttled, "repeated wrong passwords are throttled");
    // Even the right password waits while throttled …
    let (status, _, _) = unlock(&t, "localhost", "correct horse", None, ip).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    // … but another visitor isn't affected.
    let (status, _, _) = unlock(&t, "localhost", "correct horse", None, "198.51.100.78").await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Cross-site posts are refused by the CSRF guard.
    let req = Request::post("/status/acme/unlock")
        .header(header::HOST, "localhost")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("password=correct+horse"))
        .unwrap();
    let (status, _, _) = call(&t, req, Some("198.51.100.79")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn custom_domain_unlock_cookie_covers_the_root() {
    let t = app().await;
    let (pid, _) = seed(&t, true, Some("status.acme.test")).await;
    set_password(&t, pid, Some("correct horse")).await;
    invalidate_domains();
    let (status, _, body) = get(&t, "/", Some("status.acme.test"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("action=\"/status/acme/unlock\""));
    assert!(!body.contains("Website"));
    let (status, headers, _) = unlock(
        &t,
        "status.acme.test",
        "correct horse",
        None,
        "198.51.100.90",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers.get("location"), Some("/"));
    let sc = set_cookie(&headers, &format!("vv_sp_{pid}=")).unwrap();
    assert!(sc.contains("Path=/;") || sc.ends_with("Path=/"), "{sc}");
    let (status, _, body) = get(&t, "/", Some("status.acme.test"), Some(&cookie_pair(&sc))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Website"));
}

#[tokio::test]
async fn locked_pages_leak_nothing_through_fragment_json_or_badges() {
    let t = app().await;
    let (pid, mid) = seed(&t, true, None).await;
    // Public before.
    let (status, _, _) = get(&t, &format!("/api/badge/{mid}/status"), None, None).await;
    assert_eq!(status, StatusCode::OK);

    set_password(&t, pid, Some("correct horse")).await;
    let (status, headers, body) = get(&t, "/status/acme/fragment", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.is_empty());
    assert_eq!(headers.get("cache-control"), Some("no-store"));
    let (status, _, body) = get(&t, "/api/status-page/acme", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["error"], "password_required");
    assert!(!body.contains("Website"));
    // The monitor is only on a protected page now: its badges are private …
    for kind in ["status", "uptime", "ping", "cert-exp"] {
        let (status, _, _) =
            get(&t, &format!("/api/badge/{mid}/{kind}?v={kind}"), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{kind}");
    }
    // … unless badges are public for every monitor.
    t.state
        .with_conn(|c| db_settings::set_bool(c, db_settings::BADGES_PUBLIC, true))
        .await
        .unwrap();
    let (status, _, _) = get(
        &t,
        &format!("/api/badge/{mid}/status?label=again"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Logged-in users get the JSON.
    let ed = login(&t, "ed", Role::Editor).await;
    let (status, _, body) = get(&t, "/api/status-page/acme", None, Some(&ed)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Website"));
}

// ---------------------------------------------------------------------------
// Analytics
// ---------------------------------------------------------------------------

async fn counters(t: &TestApp, pid: i64) -> (i64, i64) {
    t.state
        .with_conn(move |c| {
            c.query_row(
                "SELECT COALESCE(SUM(views), 0), COALESCE(SUM(visitors), 0) FROM status_page_views WHERE page_id = ?1",
                [pid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn views_are_counted_without_storing_ips_and_skip_bots_and_editors() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    let ip = "203.0.113.57";
    let (status, _, _) = browse(
        &t,
        "/status/acme",
        FIREFOX,
        ip,
        None,
        Some("https://news.example/post/1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    browse(
        &t,
        "/status/acme",
        FIREFOX,
        ip,
        None,
        Some("http://localhost/status/acme"),
    )
    .await;
    assert_eq!(
        counters(&t, pid).await,
        (2, 1),
        "same visitor twice = 2 views, 1 visitor"
    );
    browse(
        &t,
        "/status/acme",
        "Mozilla/5.0 (Macintosh) Safari/605.1.15",
        "203.0.113.58",
        None,
        None,
    )
    .await;
    assert_eq!(counters(&t, pid).await, (3, 2));

    // Bots, the refresh fragment, prefetches and signed-in users don't count.
    browse(
        &t,
        "/status/acme",
        "Mozilla/5.0 (compatible; Googlebot/2.1)",
        ip,
        None,
        None,
    )
    .await;
    browse(&t, "/status/acme", "curl/8.4.0", ip, None, None).await;
    browse(&t, "/status/acme/fragment", FIREFOX, ip, None, None).await;
    let req = Request::get("/status/acme")
        .header(header::USER_AGENT, FIREFOX)
        .header("sec-purpose", "prefetch")
        .body(Body::empty())
        .unwrap();
    call(&t, req, Some(ip)).await;
    let ed = login(&t, "ed", Role::Editor).await;
    browse(&t, "/status/acme", FIREFOX, ip, Some(&ed), None).await;
    assert_eq!(counters(&t, pid).await, (3, 2));

    // Only the foreign referrer is kept; no IP address or user agent anywhere.
    let dump = t
        .state
        .with_conn(|c| {
            let mut out = String::new();
            for table in ["status_page_views", "status_page_referrers"] {
                let mut stmt = c.prepare(&format!("SELECT * FROM {table}"))?;
                let n = stmt.column_count();
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    for i in 0..n {
                        match r.get_ref(i)? {
                            rusqlite::types::ValueRef::Text(t)
                            | rusqlite::types::ValueRef::Blob(t) => {
                                out.push_str(&String::from_utf8_lossy(t));
                            }
                            other => out.push_str(&format!("{other:?}")),
                        }
                        out.push('|');
                    }
                    out.push('\n');
                }
            }
            Ok(out)
        })
        .await
        .unwrap();
    assert!(dump.contains("news.example"), "{dump}");
    assert!(!dump.contains("localhost"));
    for secret in ["203.0.113", "Firefox", "Mozilla"] {
        assert!(!dump.contains(secret), "{secret} stored: {dump}");
    }

    // Unique counting can be turned off; built-in analytics too.
    t.state
        .with_conn(move |c| {
            extras::set_analytics(
                c,
                pid,
                &Analytics {
                    unique: false,
                    ..Analytics::default()
                },
            )
        })
        .await
        .unwrap();
    invalidate_views();
    browse(&t, "/status/acme", FIREFOX, "203.0.113.99", None, None).await;
    assert_eq!(counters(&t, pid).await, (4, 2));
    t.state
        .with_conn(move |c| {
            extras::set_analytics(
                c,
                pid,
                &Analytics {
                    enabled: false,
                    ..Analytics::default()
                },
            )
        })
        .await
        .unwrap();
    invalidate_views();
    browse(&t, "/status/acme", FIREFOX, "203.0.113.100", None, None).await;
    assert_eq!(counters(&t, pid).await, (4, 2));

    // Locked visitors aren't counted.
    t.state
        .with_conn(move |c| extras::set_analytics(c, pid, &Analytics::default()))
        .await
        .unwrap();
    set_password(&t, pid, Some("correct horse")).await;
    browse(&t, "/status/acme", FIREFOX, "203.0.113.101", None, None).await;
    assert_eq!(counters(&t, pid).await, (4, 2));
}

#[tokio::test]
async fn third_party_analytics_only_widens_csp_for_its_page() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    let (_, headers, body) = get(&t, "/status/acme", None, None).await;
    let default_csp = headers.get("content-security-policy").unwrap().to_string();
    assert!(!body.contains("data-domain"));

    let plausible = Analytics {
        provider: Provider::Plausible,
        site_id: "status.acme.test".into(),
        url: "https://plausible.io/js/script.js".into(),
        ..Analytics::default()
    }
    .validated()
    .unwrap();
    t.state
        .with_conn(move |c| extras::set_analytics(c, pid, &plausible))
        .await
        .unwrap();
    invalidate_views();
    let (_, headers, body) = get(&t, "/status/acme", None, None).await;
    let csp = headers.get("content-security-policy").unwrap();
    assert!(
        csp.contains("script-src 'self' https://plausible.io;"),
        "{csp}"
    );
    assert!(
        csp.contains("connect-src 'self' https://plausible.io;"),
        "{csp}"
    );
    assert!(
        !csp.contains("frame-ancestors"),
        "status pages stay embeddable"
    );
    // Same policy as the app's default, plus exactly that origin.
    assert_eq!(csp.replace(" https://plausible.io", ""), default_csp);
    assert!(body.contains("<script defer src=\"https://plausible.io/js/script.js\" data-domain=\"status.acme.test\"></script>"), "{body}");
    // Other pages (and the app) keep the default policy.
    let (_, headers, _) = get(&t, "/status/nope", None, None).await;
    assert_eq!(
        headers.get("content-security-policy"),
        Some(default_csp.as_str())
    );
    let (_, headers, _) = get(&t, "/login", None, None).await;
    assert!(
        !headers
            .get("content-security-policy")
            .unwrap_or_default()
            .contains("plausible")
    );

    // Google Analytics: first-party setup script, CSP for Google's hosts.
    let ga = Analytics {
        provider: Provider::Google,
        site_id: "G-ABCDEF1234".into(),
        ..Analytics::default()
    }
    .validated()
    .unwrap();
    t.state
        .with_conn(move |c| extras::set_analytics(c, pid, &ga))
        .await
        .unwrap();
    invalidate_views();
    let (_, headers, body) = get(&t, "/status/acme", None, None).await;
    let csp = headers.get("content-security-policy").unwrap();
    assert!(
        csp.contains("https://www.googletagmanager.com")
            && csp.contains("https://*.google-analytics.com"),
        "{csp}"
    );
    assert!(!csp.contains("plausible"));
    assert!(
        body.contains("<script defer src=\"/status/acme/analytics.js?v="),
        "{body}"
    );
    let (status, headers, js) = get(&t, "/status/acme/analytics.js", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert!(js.contains("gtag('config', \"G-ABCDEF1234\")"));

    // Drafts previewed by editors never load third-party scripts.
    let mut p = t
        .state
        .with_conn(move |c| status_pages::get(c, pid))
        .await
        .unwrap()
        .unwrap()
        .to_new();
    p.published = false;
    t.state
        .with_conn(move |c| status_pages::update(c, pid, &p))
        .await
        .unwrap();
    invalidate_views();
    let ed = login(&t, "ed", Role::Editor).await;
    let (status, headers, body) = get(&t, "/status/acme", None, Some(&ed)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("analytics.js"));
    assert_eq!(
        headers.get("content-security-policy"),
        Some(default_csp.as_str())
    );
}

#[tokio::test]
async fn editor_analytics_tab_shows_views_and_saves_settings() {
    let t = app().await;
    let (pid, _) = seed(&t, true, None).await;
    browse(
        &t,
        "/status/acme",
        FIREFOX,
        "192.0.2.10",
        None,
        Some("https://blog.example/"),
    )
    .await;
    let ed = login(&t, "ed", Role::Editor).await;
    let (status, _, body) = get(
        &t,
        &format!("/status-pages/{pid}/edit?tab=analytics"),
        None,
        Some(&ed),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Views last 30 days"));
    assert!(body.contains("class=\"chart-line\""));
    assert!(body.contains("blog.example"));
    assert!(body.contains("aria-current=\"page\">Analytics"));

    let uri = format!("/status-pages/{pid}/analytics");
    let (status, _, body) = send(&t, "POST", &uri, &ed, "analytics_enabled=true&analytics_provider=umami&analytics_site_id=nope&analytics_url=http%3A%2F%2Fx").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("aria-invalid=\"true\""));
    let ok = "analytics_enabled=true&analytics_provider=matomo&analytics_site_id=4&analytics_url=https%3A%2F%2Fstats.acme.test%2F";
    let (status, headers, _) = send(&t, "POST", &uri, &ed, ok).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get("location"),
        Some(format!("/status-pages/{pid}/edit?tab=analytics").as_str())
    );
    let a = t
        .state
        .with_conn(move |c| status_pages::get(c, pid))
        .await
        .unwrap()
        .unwrap()
        .analytics;
    assert_eq!(
        (a.provider, a.site_id.as_str(), a.unique),
        (Provider::Matomo, "4", false)
    );
    let viewer = login(&t, "vi", Role::Viewer).await;
    let (status, _, _) = send(&t, "POST", &uri, &viewer, ok).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
