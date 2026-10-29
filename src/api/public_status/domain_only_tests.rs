//! Router tests for `custom_domain_only`: a custom domain that serves only
//! its status page.

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;

use super::router_tests::{TestApp, app, seed};
use super::{domain_only_allows, invalidate_domains};
use crate::db::status_pages::extras::{self, AssetKind};
use crate::db::status_pages::{self, NewStatusPage};

const HOST: &str = "status.acme.test";

fn ico() -> Vec<u8> {
    let mut v = vec![0, 0, 1, 0, 1, 0, 16, 16, 0, 0, 1, 0, 32, 0];
    v.extend_from_slice(&[0u8; 32]);
    v
}

/// Sends a request with the given method and `Host`.
async fn call(
    t: &TestApp,
    method: Method,
    uri: &str,
    host: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::empty())
        .unwrap();
    let resp = t.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body =
        String::from_utf8_lossy(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).to_string();
    (status, headers, body)
}

/// Page "acme" on `HOST` with a favicon, plus a second published page
/// "other" without a domain.
async fn setup(only: bool) -> TestApp {
    let t = app().await;
    let (pid, _) = seed(&t, true, Some(HOST)).await;
    t.state
        .with_conn(move |c| {
            extras::set_asset(
                c,
                pid,
                AssetKind::Favicon,
                &extras::validate_image(ico()).unwrap(),
            )?;
            status_pages::insert(
                c,
                &NewStatusPage {
                    slug: "other".into(),
                    title: "Other status".into(),
                    ..NewStatusPage::default()
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    set_only(&t, only).await;
    t
}

async fn set_only(t: &TestApp, only: bool) {
    t.state
        .with_conn(move |c| {
            let page = status_pages::get_by_slug(c, "acme")?.unwrap();
            let mut new = page.to_new();
            new.custom_domain_only = only;
            status_pages::update(c, page.id, &new)
        })
        .await
        .unwrap();
    invalidate_domains(&t.state);
}

/// The plain answer of a status-page-only domain.
fn is_domain_404(status: StatusCode, headers: &axum::http::HeaderMap, body: &str) -> bool {
    status == StatusCode::NOT_FOUND
        && body == "Not found"
        && headers[header::CONTENT_TYPE] == "text/plain; charset=utf-8"
        && !headers.contains_key(header::LOCATION)
}

#[tokio::test]
async fn domain_only_serves_what_the_page_needs() {
    let t = setup(true).await;
    for (method, uri) in [
        (Method::GET, "/"),
        (Method::HEAD, "/"),
        (Method::GET, "/favicon.ico"),
        (Method::GET, "/static/style.css"),
        (Method::GET, "/static/favicon.svg"),
        (Method::GET, "/status/acme"),
        (Method::GET, "/status/acme/fragment"),
        (Method::GET, "/status/acme/feed.xml"),
        (Method::GET, "/status/acme/subscribe"),
        (Method::POST, "/status/acme/subscribe"),
        (Method::POST, "/status/acme/unlock"),
        (Method::GET, "/api/status-page/acme"),
    ] {
        let (status, headers, body) = call(&t, method.clone(), uri, HOST).await;
        assert!(
            !is_domain_404(status, &headers, &body),
            "{method} {uri} was refused ({status})"
        );
    }
    let (status, _, body) = call(&t, Method::GET, "/", HOST).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Acme status"));
    let (status, headers, _) = call(&t, Method::GET, "/favicon.ico", HOST).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/x-icon");
    let (status, _, body) = call(&t, Method::GET, "/status/acme/fragment", HOST).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("id=\"sp-live\""));
    let (status, _, body) = call(&t, Method::GET, "/api/status-page/acme", HOST).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"slug\":\"acme\""));
    // The host is matched like before: case and port don't matter.
    let (status, _, body) = call(&t, Method::GET, "/", "Status.Acme.Test:8443").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Acme status"));
}

#[tokio::test]
async fn domain_only_refuses_everything_else() {
    let t = setup(true).await;
    for (method, uri) in [
        (Method::GET, "/login"),
        (Method::POST, "/login"),
        (Method::GET, "/setup"),
        (Method::GET, "/logout"),
        (Method::GET, "/dashboard"),
        (Method::GET, "/monitors"),
        (Method::GET, "/status-pages"),
        (Method::GET, "/status-pages/1/edit"),
        (Method::GET, "/settings/general"),
        (Method::GET, "/api/v1/monitors"),
        (Method::POST, "/api/v1/monitors"),
        (Method::GET, "/api-docs"),
        (Method::GET, "/mcp"),
        (Method::POST, "/mcp"),
        (Method::GET, "/metrics"),
        (Method::GET, "/healthz"),
        (Method::GET, "/api/badge/1/status"),
        (Method::GET, "/api/push/token"),
        (Method::POST, "/api/push/token"),
        (Method::GET, "/status/other"),
        (Method::GET, "/status/other/fragment"),
        (Method::POST, "/status/other/subscribe"),
        (Method::GET, "/status/acmex"),
        (Method::GET, "/status/acme%2Fx"),
        (Method::GET, "/status/ac%6De"),
        (Method::GET, "/status"),
        (Method::GET, "/api/status-page/other"),
        (Method::GET, "/api/status-page/acme/x"),
        (Method::GET, "/api/status-page"),
        (Method::GET, "/static"),
        (Method::POST, "/"),
        (Method::DELETE, "/favicon.ico"),
        (Method::GET, "/index.html"),
    ] {
        let (status, headers, body) = call(&t, method.clone(), uri, HOST).await;
        assert!(
            is_domain_404(status, &headers, &body),
            "{method} {uri} -> {status}"
        );
    }
}

#[tokio::test]
async fn other_pages_still_work_on_the_normal_host() {
    let t = setup(true).await;
    // A host that matches no page behaves as before.
    for host in ["localhost:3001", "status.other.test"] {
        let (status, _, body) = call(&t, Method::GET, "/status/other", host).await;
        assert_eq!(status, StatusCode::OK, "{host}");
        assert!(body.contains("Other status"));
        let (status, headers2, body2) = call(&t, Method::GET, "/login", host).await;
        assert!(!is_domain_404(status, &headers2, &body2), "{host}");
        let (status, _, _) = call(&t, Method::GET, "/api/v1/monitors", host).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{host}");
    }
}

#[tokio::test]
async fn without_the_setting_the_domain_passes_other_paths_through() {
    let t = setup(false).await;
    let (status, _, body) = call(&t, Method::GET, "/", HOST).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Acme status"));
    let (status, _, body) = call(&t, Method::GET, "/status/other", HOST).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Other status"));
    let (status, headers, body) = call(&t, Method::GET, "/login", HOST).await;
    assert!(!is_domain_404(status, &headers, &body));
    let (status, _, _) = call(&t, Method::GET, "/api/v1/monitors", HOST).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = call(&t, Method::GET, "/healthz", HOST).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_setting_follows_the_cached_domain_list() {
    let t = setup(false).await;
    let (status, _, _) = call(&t, Method::GET, "/healthz", HOST).await;
    assert_eq!(status, StatusCode::OK);
    set_only(&t, true).await;
    let (status, headers, body) = call(&t, Method::GET, "/healthz", HOST).await;
    assert!(is_domain_404(status, &headers, &body));
    set_only(&t, false).await;
    let (status, _, _) = call(&t, Method::GET, "/healthz", HOST).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn an_unpublished_page_does_not_hold_its_domain_only() {
    let t = setup(true).await;
    t.state
        .with_conn(|c| {
            let page = status_pages::get_by_slug(c, "acme")?.unwrap();
            let mut new = page.to_new();
            new.published = false;
            status_pages::update(c, page.id, &new)
        })
        .await
        .unwrap();
    invalidate_domains(&t.state);
    // Unpublished: the host is no status page host any more.
    let (status, headers, body) = call(&t, Method::GET, "/healthz", HOST).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!is_domain_404(status, &headers, &body));
}

#[test]
fn the_allow_list() {
    let allowed = |m: Method, p: &str| domain_only_allows("acme", &m, p);
    for p in [
        "/",
        "/favicon.ico",
        "/static/style.css",
        "/static/fonts/InterVariable.woff2",
        "/status/acme",
        "/status/acme/",
        "/status/acme/fragment",
        "/status/acme/analytics.js",
        "/api/status-page/acme",
    ] {
        assert!(allowed(Method::GET, p), "{p}");
    }
    assert!(allowed(Method::HEAD, "/"));
    assert!(allowed(Method::POST, "/status/acme/unlock"));
    assert!(allowed(Method::POST, "/status/acme/subscribe/confirm/abc"));
    for p in [
        "/login",
        "/status",
        "/status/",
        "/status/acm",
        "/status/acmeX",
        "/status/other",
        "/status/other/status/acme",
        "/api/status-page/",
        "/api/status-page/acmeX",
        "/api/status-page/acme/",
        "/api/v1/monitors",
        "/staticx",
        "/mcp",
    ] {
        assert!(!allowed(Method::GET, p), "{p}");
    }
    assert!(!allowed(Method::POST, "/"));
    assert!(!allowed(Method::POST, "/favicon.ico"));
}
