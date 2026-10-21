//! Auth names and formats: cookie names and attributes, token prefixes and
//! shapes, the session/pending value encodings, CSRF policy, and that tokens
//! issued by every released version still authenticate.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use insta::assert_snapshot;
use vigilant_vole::api::page::FLASH_COOKIE;
use vigilant_vole::auth::session::{
    self, ABSOLUTE_TIMEOUT_SECS, IDLE_TIMEOUT_SECS, PENDING_2FA_COOKIE, PENDING_2FA_TTL_SECS,
    PendingLogin, REFRESH_AFTER_SECS, SessionClaims,
};
use vigilant_vole::auth::{self, csrf, tokens};
use vigilant_vole::db;

use crate::common::{self, DataDir, fixtures};
use crate::harness::{ADMIN, ADMIN_PASSWORD, App, ORIGIN, Res};
use crate::norm;

fn shape(token: &str) -> String {
    let (prefix, body) = token.split_at(token.find('_').map_or(0, |i| i + 1));
    let charset = if body.bytes().all(|b| b.is_ascii_alphanumeric()) {
        "base62"
    } else {
        "other"
    };
    format!("{prefix}<{} {charset}>", body.len())
}

#[test]
fn names_formats_and_cookie_attributes() {
    let mut out = String::new();
    let mut line = |k: &str, v: String| out.push_str(&format!("{k}: {v}\n"));

    line("session cookie", auth::SESSION_COOKIE.into());
    line("pending 2FA cookie", PENDING_2FA_COOKIE.into());
    line("flash cookie", FLASH_COOKIE.into());
    line("idle timeout secs", IDLE_TIMEOUT_SECS.to_string());
    line("absolute timeout secs", ABSOLUTE_TIMEOUT_SECS.to_string());
    line("refresh after secs", REFRESH_AFTER_SECS.to_string());
    line("pending 2FA ttl secs", PENDING_2FA_TTL_SECS.to_string());

    // API tokens.
    let t = tokens::generate();
    line("api token prefix", tokens::TOKEN_PREFIX.into());
    line("api token body length", tokens::TOKEN_BODY_LEN.to_string());
    line("api token generated", shape(&t));
    line(
        "api token well-formed",
        tokens::is_well_formed(&t).to_string(),
    );
    line(
        "api token display prefix",
        shape(&tokens::display_prefix(&t)),
    );
    line("api token hash of 'abc'", tokens::hash("abc"));
    for bad in [
        "vv_short",
        "vva_0000000000000000000000000000000000000000000",
        "",
    ] {
        line(
            &format!("api token well-formed({bad:?})"),
            tokens::is_well_formed(bad).to_string(),
        );
    }

    // Agent tokens.
    let a = db::agents::generate_token();
    line("agent token prefix", db::agents::TOKEN_PREFIX.into());
    line("agent token generated", shape(&a.token));
    line("agent token stored prefix", shape(&a.prefix));
    line(
        "agent token hash is sha256 hex",
        (a.hash == norm::sha256(a.token.as_bytes())).to_string(),
    );

    // Where tokens are read from.
    for value in [
        "Bearer vv_abc",
        "bearer   vv_abc  ",
        "Basic cHJvbWV0aGV1czp2dl9hYmM=",
        "Token vv_abc",
    ] {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static(value));
        line(
            &format!("token from Authorization {value:?}"),
            format!("{:?}", tokens::token_from_headers(&h)),
        );
    }

    // Cookie values and attributes, for a fixed login time.
    let claims = SessionClaims::new(7, 3, 1_700_000_000);
    line("session value", claims.encode());
    line(
        "session value (refreshed 2h later)",
        claims.refreshed(1_700_007_200).encode(),
    );
    line(
        "legacy v1 value accepted",
        format!("{:?}", SessionClaims::decode("v1.7.3.1700000000")),
    );
    let pending = PendingLogin {
        user_id: 7,
        version: 3,
        issued_at: 1_700_000_000,
        next: "/monitors/4?tab=history".into(),
    };
    line("pending value", pending.encode());
    for secure in [false, true] {
        line(
            &format!("Set-Cookie session (secure={secure})"),
            session::session_cookie(&claims, secure).to_string(),
        );
        line(
            &format!("Set-Cookie session 29 days in (secure={secure})"),
            session::session_cookie(&claims.refreshed(1_700_000_000 + 29 * 86_400), secure)
                .to_string(),
        );
        line(
            &format!("Set-Cookie pending 2FA (secure={secure})"),
            session::pending_cookie(&pending, secure).to_string(),
        );
    }
    line(
        "removal cookie",
        session::removal_cookie(auth::SESSION_COOKIE).to_string(),
    );

    // Secure is only ever set behind a trusted TLS proxy.
    for (proto, trust) in [
        (None, true),
        (Some("https"), false),
        (Some("https"), true),
        (Some("https, http"), true),
        (Some("http"), true),
    ] {
        let mut h = HeaderMap::new();
        if let Some(p) = proto {
            h.insert("x-forwarded-proto", HeaderValue::from_static(p));
        }
        line(
            &format!("secure cookies: x-forwarded-proto={proto:?} trust_proxy={trust}"),
            session::is_https(&h, trust).to_string(),
        );
    }
    for next in [
        None,
        Some("/monitors/4"),
        Some("//evil.example"),
        Some("https://evil.example/"),
        Some("/\\evil.example"),
        Some("/login?next=/"),
        Some("/settings?tab=a b"),
    ] {
        line(
            &format!("sanitize next {next:?}"),
            session::sanitize_next(next),
        );
    }
    assert_snapshot!("auth_names_and_formats", out);
}

/// `(method, path, request headers, configured base URL)`.
type CsrfCase = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
    Option<&'static str>,
);

#[test]
fn csrf_policy_table() {
    let mut out = String::new();
    let cases: &[CsrfCase] = &[
        ("GET", "/settings", &[], None),
        ("HEAD", "/settings", &[], None),
        ("OPTIONS", "/settings", &[], None),
        ("POST", "/settings/general", &[], None),
        ("POST", "/settings/general", &[("hx-request", "true")], None),
        ("POST", "/settings/general", &[("hx-request", "TRUE")], None),
        (
            "POST",
            "/settings/general",
            &[("hx-request", "false")],
            None,
        ),
        (
            "POST",
            "/settings/general",
            &[("host", "vv.test"), ("origin", "http://vv.test")],
            None,
        ),
        (
            "POST",
            "/settings/general",
            &[("host", "vv.test:80"), ("origin", "http://vv.test")],
            None,
        ),
        (
            "POST",
            "/settings/general",
            &[("host", "vv.test:3001"), ("origin", "http://vv.test:3001")],
            None,
        ),
        (
            "POST",
            "/settings/general",
            &[("host", "vv.test"), ("origin", "https://vv.test")],
            None,
        ),
        (
            "POST",
            "/settings/general",
            &[("host", "vv.test"), ("origin", "http://evil.test")],
            None,
        ),
        (
            "POST",
            "/settings/general",
            &[("host", "vv.test"), ("origin", "null")],
            None,
        ),
        (
            "POST",
            "/settings/general",
            &[("host", "vv.test"), ("referer", "http://vv.test/settings")],
            None,
        ),
        (
            "POST",
            "/settings/general",
            &[("host", "vv.test"), ("referer", "http://evil.test/")],
            None,
        ),
        (
            "POST",
            "/settings/general",
            &[
                ("host", "internal:3001"),
                ("origin", "https://status.example.com"),
            ],
            Some("https://status.example.com"),
        ),
        (
            "POST",
            "/settings/general",
            &[
                ("host", "internal:3001"),
                ("origin", "http://status.example.com"),
            ],
            Some("https://status.example.com"),
        ),
        (
            "DELETE",
            "/settings/tokens/1",
            &[("host", "vv.test"), ("origin", "http://evil.test")],
            None,
        ),
        ("POST", "/api/push/abc", &[], None),
        ("POST", "/api/agent/v1/results", &[], None),
        ("POST", "/api/v1/monitors", &[], None),
        ("POST", "/api/v1", &[], None),
        ("POST", "/api/v1x", &[], None),
        ("POST", "/api/badge/1/status", &[], None),
        ("POST", "/metrics", &[], None),
        ("POST", "/mcp", &[], None),
        ("POST", "/mcpx", &[], None),
        (
            "POST",
            "/status/acme/subscribe/unsubscribe/tok123",
            &[],
            None,
        ),
        ("POST", "/status/acme/subscribe", &[], None),
        ("POST", "/api/status-page/acme", &[], None),
    ];
    for (method, path, headers, base_url) in cases {
        let mut h = HeaderMap::new();
        for (k, v) in *headers {
            h.insert(*k, HeaderValue::from_static(v));
        }
        let method: Method = method.parse().unwrap();
        let allowed = csrf::is_allowed(&method, path, &h, false, *base_url);
        let shown: Vec<String> = headers.iter().map(|(k, v)| format!("{k}={v}")).collect();
        out.push_str(&format!(
            "{} {method} {path} [{}]{}\n",
            if allowed { "allow" } else { "BLOCK" },
            shown.join(", "),
            base_url
                .map(|b| format!(" base_url={b}"))
                .unwrap_or_default(),
        ));
    }
    // Behind a trusted proxy the forwarded host counts too.
    let mut h = HeaderMap::new();
    h.insert("host", HeaderValue::from_static("127.0.0.1:3001"));
    h.insert("x-forwarded-host", HeaderValue::from_static("vv.example"));
    h.insert("origin", HeaderValue::from_static("https://vv.example"));
    for trust in [false, true] {
        out.push_str(&format!(
            "trust_proxy={trust}: forwarded host vv.example, origin https://vv.example -> {}\n",
            csrf::is_allowed(&Method::POST, "/logout", &h, trust, None)
        ));
    }
    assert_snapshot!("auth_csrf_policy", out);
}

fn exchange(label: &str, res: &Res) -> String {
    let mut out = format!("== {label}\nstatus: {}\n", res.status.as_u16());
    for name in ["location", "content-type"] {
        if res.headers.contains_key(name) {
            out.push_str(&format!("{name}: {}\n", res.header(name)));
        }
    }
    // The cookie jar emits its changes in no fixed order.
    let mut cookies: Vec<String> = res
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| norm::set_cookie(v.to_str().unwrap()))
        .collect();
    cookies.sort();
    for c in cookies {
        out.push_str(&format!("set-cookie: {c}\n"));
    }
    out
}

/// The browser flow over HTTP: what each step sets and where it goes.
#[tokio::test]
async fn login_logout_and_two_factor_cookies() {
    const SECRET: &str = "K5YMSTXGCAMWB5SHRO24JENVW2TLI4VP";
    let mut app = App::seeded("auth-flow").await;
    let hash = auth::hash_password(ADMIN_PASSWORD);
    app.state
        .with_conn(move |c| {
            let id = db::users::insert_user(c, "eve", &hash, auth::Role::Editor)?;
            db::users::set_pending_totp_secret(c, id, SECRET)?;
            db::users::enable_totp(c, id, 0, &[])?;
            Ok(())
        })
        .await
        .unwrap_or_else(|_| panic!("seed eve"));
    let mut out = String::new();

    let res = app.post_form("/logout", &[]).await;
    out.push_str(&exchange("POST /logout", &res));
    out.push_str(&format!("cookies held: {:?}\n", app.cookie_names()));

    let res = app.get("/").await;
    out.push_str(&exchange("GET / signed out", &res));
    let res = app.get("/login").await;
    out.push_str(&exchange("GET /login", &res));

    let res = app
        .post_form(
            "/login",
            &[("username", ADMIN), ("password", "wrong-password-1")],
        )
        .await;
    out.push_str(&exchange("POST /login wrong password", &res));

    let res = app
        .post_form(
            "/login",
            &[
                ("username", ADMIN),
                ("password", ADMIN_PASSWORD),
                ("next", "/settings/tokens"),
            ],
        )
        .await;
    out.push_str(&exchange("POST /login admin", &res));
    out.push_str(&format!("cookies held: {:?}\n", app.cookie_names()));
    let res = app.get("/settings/tokens").await;
    out.push_str(&exchange("GET /settings/tokens signed in", &res));

    app.clear_cookies();
    let res = app
        .post_form(
            "/login",
            &[("username", "eve"), ("password", ADMIN_PASSWORD)],
        )
        .await;
    out.push_str(&exchange("POST /login eve (2FA)", &res));
    out.push_str(&format!("cookies held: {:?}\n", app.cookie_names()));
    let res = app.get("/login/2fa").await;
    out.push_str(&exchange("GET /login/2fa", &res));
    let code = common::totp_now(SECRET, "eve");
    let res = app.post_form("/login/2fa", &[("code", &code)]).await;
    out.push_str(&exchange("POST /login/2fa", &res));
    out.push_str(&format!("cookies held: {:?}\n", app.cookie_names()));

    assert_snapshot!("auth_browser_flow", out);
}

/// CSRF over HTTP: which proofs a state-changing request needs.
#[tokio::test]
async fn csrf_guard_over_http() {
    let mut app = App::seeded("csrf").await;
    let mut out = String::new();
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("no Origin or Referer", &[]),
        ("HX-Request", &[("hx-request", "true")]),
        ("same Origin", &[("origin", ORIGIN)]),
        (
            "same Referer",
            &[("referer", "http://vv.test/settings/general")],
        ),
        ("foreign Origin", &[("origin", "http://evil.test")]),
        ("Origin null", &[("origin", "null")]),
    ];
    for (label, headers) in cases {
        let mut req = Request::post("/settings/general")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        for (k, v) in *headers {
            req = req.header(*k, *v);
        }
        let res = app
            .send(req.body(Body::from("site_name=Changed")).unwrap())
            .await;
        let blocked = res.status == StatusCode::FORBIDDEN;
        out.push_str(&format!(
            "{label}: {} ({})\n",
            res.status.as_u16(),
            if blocked {
                "blocked"
            } else {
                "passed the guard"
            }
        ));
    }
    // Exempt machine endpoints skip the guard; they fail later, on their own
    // checks (validation, an unknown push token, MCP's Origin rule).
    for path in ["/api/v1/tags", "/api/push/not-a-token", "/mcp"] {
        let req = Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, "http://evil.test")
            .body(Body::from("{}"))
            .unwrap();
        let res = app.send(req).await;
        out.push_str(&format!(
            "exempt {path} with foreign Origin: {}\n",
            res.status.as_u16()
        ));
    }
    // What a blocked browser request gets back.
    let req = Request::post("/settings/general")
        .header(header::ORIGIN, "http://evil.test")
        .header(header::ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();
    let res = app.send(req).await;
    out.push_str(&format!(
        "\n== blocked HTML response\nstatus: {}\n{}\n{}\n",
        res.status.as_u16(),
        norm::headers(&res.headers),
        norm::text(&res.text())
    ));
    assert_snapshot!("auth_csrf_http", out);
}

/// How the API answers missing, malformed and unknown credentials.
#[tokio::test]
async fn api_rejections() {
    let mut app = App::seeded("api-auth").await;
    app.clear_cookies();
    let mut out = String::new();
    let cases: &[(&str, Option<&str>)] = &[
        ("no Authorization", None),
        ("malformed token", Some("Bearer nope")),
        (
            "unknown token",
            Some("Bearer vv_0000000000000000000000000000000000000000000"),
        ),
        (
            "agent token on the REST API",
            Some("Bearer vva_0000000000000000000000000000000000000000"),
        ),
        // base64("x:" + the seeded admin token), Prometheus/Kuma style.
        (
            "Basic with the admin token as password",
            Some("Basic eDp2dl9Hb2xkZW5BZG1pbldyaXRlVG9rZW4wMDAwMDAwMDAwMDAwMDAwMDAwMDAw"),
        ),
    ];
    for (label, auth) in cases {
        let mut req = Request::get("/api/v1/me");
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, *a);
        }
        let res = app.send(req.body(Body::empty()).unwrap()).await;
        out.push_str(&format!(
            "== {label}\nstatus: {}\n{}{}\n\n",
            res.status.as_u16(),
            norm::headers(&res.headers),
            norm::dates(&norm::text(&res.text()), vigilant_vole::i18n::Lang::EN)
        ));
    }
    let res = app.get_with("/settings", &[("accept", "text/html")]).await;
    out.push_str(&exchange("GET /settings signed out", &res));
    assert_snapshot!("auth_api_rejections", out);
}

/// Tokens saved by every released version (the upgrade fixtures) still
/// authenticate, with their scope, after this build's migrations.
#[tokio::test]
async fn release_tokens_still_authenticate() {
    let mut out = String::new();
    for f in fixtures() {
        let mut app = App::start(DataDir::with_fixture(&f)).await;
        out.push_str(&format!("== {}\n", f.version));
        for (label, pointer) in [("admin", "/admin_token"), ("viewer", "/viewer_token")] {
            let token = f.credential(pointer);
            let me = app.api_as(&token, Method::GET, "/api/v1/me", None).await;
            let write = app
                .api_as(
                    &token,
                    Method::POST,
                    "/api/v1/tags",
                    Some(serde_json::json!({"name": format!("golden-{label}")})),
                )
                .await;
            out.push_str(&format!(
                "{label} token {}: well-formed={} me={} as {} admin={} write={}\n",
                shape(&token),
                tokens::is_well_formed(&token),
                me.status.as_u16(),
                me.json()["username"].as_str().unwrap_or("-"),
                me.json()["is_admin"],
                write.status.as_u16(),
            ));
        }
        let agent = f.credential("/agent_token");
        let req = Request::get("/api/agent/v1/monitors")
            .header(header::AUTHORIZATION, format!("Bearer {agent}"))
            .body(Body::empty())
            .unwrap();
        let res = app.send(req).await;
        out.push_str(&format!(
            "agent token {}: monitors={}\n",
            shape(&agent),
            res.status.as_u16()
        ));
        let push = f.credential("/push_token");
        let res = app
            .get(&format!("/api/push/{push}?status=up&msg=golden"))
            .await;
        out.push_str(&format!(
            "push token <{} chars>: push={}\n",
            push.len(),
            res.status.as_u16()
        ));
    }
    assert_snapshot!("auth_release_tokens", out);
}
