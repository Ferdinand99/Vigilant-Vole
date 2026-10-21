//! The complete response header set (CSP, framing, referrer, sniffing,
//! caching, compression) for each kind of response the app serves.

use axum::http::Method;
use insta::assert_snapshot;

use crate::harness::{App, PAGE_DOMAIN, PAGE_SLUG, admin_token};
use crate::norm;

/// `(label, path, extra request headers, signed in)`.
type Case<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>, bool);

#[tokio::test]
async fn response_headers_by_kind() {
    let mut app = App::seeded("headers").await;
    let token = admin_token();
    let bearer = format!("Bearer {token}");
    let status_page = format!("/status/{PAGE_SLUG}");
    let status_json = format!("/api/status-page/{PAGE_SLUG}");
    let cases: Vec<Case> = vec![
        ("app page (signed in)", "/", vec![], true),
        ("app page (signed out, login)", "/login", vec![], false),
        (
            "app page, gzip accepted",
            "/login",
            vec![("accept-encoding", "gzip")],
            false,
        ),
        ("redirect to login", "/settings", vec![], false),
        ("htmx request", "/", vec![("hx-request", "true")], true),
        ("public status page", status_page.as_str(), vec![], false),
        (
            "public status page on its custom domain",
            "/",
            vec![("host", PAGE_DOMAIN)],
            false,
        ),
        ("public status JSON", status_json.as_str(), vec![], false),
        (
            "REST API",
            "/api/v1/me",
            vec![("authorization", bearer.as_str())],
            false,
        ),
        ("REST API unauthorised", "/api/v1/me", vec![], false),
        (
            "OpenAPI document",
            "/api/v1/openapi.json",
            vec![("authorization", bearer.as_str())],
            false,
        ),
        ("static stylesheet", "/static/style.css", vec![], false),
        (
            "static script, gzip accepted",
            "/static/app.js",
            vec![("accept-encoding", "gzip")],
            false,
        ),
        (
            "static font",
            "/static/fonts/InterVariable.woff2",
            vec![],
            false,
        ),
        ("static missing", "/static/nope.css", vec![], false),
        ("health check", "/healthz", vec![], false),
        (
            "not found (HTML)",
            "/no-such-page",
            vec![("accept", "text/html")],
            true,
        ),
        (
            "not found (JSON)",
            "/no-such-page",
            vec![("accept", "application/json")],
            true,
        ),
        ("MCP GET", "/mcp", vec![], false),
    ];
    let mut out = String::new();
    for (label, path, headers, signed_in) in cases {
        let res = if signed_in {
            app.get_with(path, &headers).await
        } else {
            app.get_anon(path, &headers).await
        };
        let mut shown = norm::headers(&res.headers);
        if path.starts_with("/static/") && res.status == 200 {
            // Debug builds serve static files from disk and disable caching.
            let expected = if cfg!(debug_assertions) {
                "no-cache"
            } else {
                "public, max-age=3600"
            };
            assert!(shown.contains(&format!("cache-control: {expected}\n")));
            shown = shown.replace(
                &format!("cache-control: {expected}\n"),
                "cache-control: <no-cache in debug builds, public, max-age=3600 in release>\n",
            );
        }
        out.push_str(&format!(
            "== {label}: GET {path}{}\nstatus: {}\n{}\n",
            if headers.is_empty() {
                String::new()
            } else {
                format!(
                    " [{}]",
                    headers
                        .iter()
                        .map(|(k, v)| format!(
                            "{k}: {}",
                            if *k == "authorization" {
                                "Bearer <token>"
                            } else {
                                *v
                            }
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
            res.status.as_u16(),
            shown
        ));
    }
    // A state-changing API call.
    let res = app
        .api(
            Method::POST,
            "/api/v1/tags",
            Some(serde_json::json!({"name": "golden"})),
        )
        .await;
    out.push_str(&format!(
        "== REST API create: POST /api/v1/tags\nstatus: {}\n{}\n",
        res.status.as_u16(),
        norm::headers(&res.headers)
    ));
    assert_snapshot!("security_headers", out);
}
