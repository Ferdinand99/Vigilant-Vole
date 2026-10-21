//! OAuth2 client credentials and mTLS end to end, against local servers.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};

use super::test_pki::{self, Pki};
use super::*;
use crate::db::models::{CheckOutcome, HeartbeatStatus, MonitorType};
use crate::monitor::checkers::http;
use crate::monitor::checkers::test_support::monitor;

const SECRET: &str = "s3cr3t-value";

/// A token endpoint and a protected API in one server.
#[derive(Default)]
struct Fake {
    token_requests: AtomicUsize,
    token_auth: Mutex<Option<String>>,
    token_body: Mutex<String>,
    api_auth: Mutex<Vec<Option<String>>>,
    expires_in: AtomicU64,
    /// Lowest token number the API still accepts (raise it to "revoke" older ones).
    min_token: AtomicUsize,
    reject_credentials: AtomicBool,
    /// Raw JSON the token endpoint answers with instead of a token.
    custom_response: Mutex<Option<String>>,
}

async fn token(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    let n = fake.token_requests.fetch_add(1, Ordering::SeqCst) + 1;
    *fake.token_auth.lock().unwrap() = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    *fake.token_body.lock().unwrap() = body;
    if fake.reject_credentials.load(Ordering::SeqCst) {
        // A careless provider that echoes the secret back.
        let body =
            format!(r#"{{"error":"invalid_client","error_description":"bad secret {SECRET}"}}"#);
        return (
            StatusCode::UNAUTHORIZED,
            [("content-type", "application/json")],
            body,
        )
            .into_response();
    }
    if let Some(raw) = fake.custom_response.lock().unwrap().clone() {
        return ([("content-type", "application/json")], raw).into_response();
    }
    let body = format!(
        r#"{{"access_token":"tok-{n}","token_type":"Bearer","expires_in":{}}}"#,
        fake.expires_in.load(Ordering::SeqCst)
    );
    ([("content-type", "application/json")], body).into_response()
}

async fn api(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> StatusCode {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    fake.api_auth.lock().unwrap().push(auth.clone());
    let min = fake.min_token.load(Ordering::SeqCst);
    let ok = auth
        .as_deref()
        .and_then(|a| a.strip_prefix("Bearer tok-"))
        .and_then(|n| n.parse::<usize>().ok())
        .is_some_and(|n| n >= min);
    if ok {
        StatusCode::OK
    } else {
        StatusCode::UNAUTHORIZED
    }
}

async fn serve() -> (SocketAddr, Arc<Fake>) {
    let fake = Arc::new(Fake {
        expires_in: AtomicU64::new(3600),
        ..Fake::default()
    });
    let app = Router::new()
        .route("/token", post(token))
        .route("/api", get(api))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, fake)
}

/// Every test uses its own monitor id: tokens are cached per id.
fn oauth(addr: SocketAddr, id: i64) -> Monitor {
    let mut m = monitor(MonitorType::Http, &format!("http://{addr}/api"));
    m.id = id;
    m.auth_method = "oauth2".into();
    m.oauth_token_url = Some(format!("http://{addr}/token"));
    m.oauth_client_id = Some("vole".into());
    m.oauth_client_secret = Some(SECRET.into());
    m
}

fn message(out: &CheckOutcome) -> String {
    out.message.clone().unwrap_or_default()
}

#[tokio::test]
async fn bearer_token_is_sent_and_reused_between_checks() {
    let (addr, fake) = serve().await;
    let mut m = oauth(addr, 9101);
    m.oauth_scope = Some("read write".into());
    m.oauth_audience = Some("https://api.example".into());

    for _ in 0..3 {
        let out = http::check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{}", message(&out));
    }
    assert_eq!(
        fake.token_requests.load(Ordering::SeqCst),
        1,
        "one token for three checks"
    );
    let seen = fake.api_auth.lock().unwrap().clone();
    assert_eq!(seen, vec![Some("Bearer tok-1".to_string()); 3]);

    // client_secret_basic: credentials in the header, not the body.
    assert_eq!(
        fake.token_auth.lock().unwrap().as_deref(),
        Some(client_secret_basic("vole", SECRET).as_str())
    );
    let body = fake.token_body.lock().unwrap().clone();
    assert!(body.contains("grant_type=client_credentials"), "{body}");
    assert!(body.contains("scope=read+write"), "{body}");
    assert!(
        body.contains("audience=https%3A%2F%2Fapi.example"),
        "{body}"
    );
    assert!(
        !body.contains(SECRET) && !body.contains("client_id"),
        "{body}"
    );
}

#[tokio::test]
async fn client_secret_post_puts_the_credentials_in_the_body() {
    let (addr, fake) = serve().await;
    let mut m = oauth(addr, 9102);
    m.oauth_client_auth = "client_secret_post".into();
    assert_eq!(http::check(&m).await.status, HeartbeatStatus::Up);
    assert_eq!(*fake.token_auth.lock().unwrap(), None);
    let body = fake.token_body.lock().unwrap().clone();
    assert!(
        body.contains("client_id=vole") && body.contains(&format!("client_secret={SECRET}")),
        "{body}"
    );
}

#[tokio::test]
async fn token_wins_over_a_hand_written_authorization_header() {
    let (addr, fake) = serve().await;
    let mut m = oauth(addr, 9103);
    m.http_headers = Some(r#"{"Authorization":"Basic eDp5"}"#.into());
    assert_eq!(http::check(&m).await.status, HeartbeatStatus::Up);
    assert_eq!(
        fake.api_auth.lock().unwrap().clone(),
        vec![Some("Bearer tok-1".to_string())]
    );
}

#[tokio::test]
async fn a_token_that_expires_immediately_is_fetched_every_time() {
    let (addr, fake) = serve().await;
    fake.expires_in.store(0, Ordering::SeqCst);
    let m = oauth(addr, 9104);
    assert_eq!(http::check(&m).await.status, HeartbeatStatus::Up);
    assert_eq!(http::check(&m).await.status, HeartbeatStatus::Up);
    assert_eq!(fake.token_requests.load(Ordering::SeqCst), 2);
    let seen = fake.api_auth.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            Some("Bearer tok-1".to_string()),
            Some("Bearer tok-2".to_string())
        ]
    );
}

#[tokio::test]
async fn editing_the_oauth_settings_never_reuses_the_old_token() {
    let (addr, fake) = serve().await;
    let mut m = oauth(addr, 9105);
    assert_eq!(http::check(&m).await.status, HeartbeatStatus::Up);
    m.oauth_scope = Some("admin".into());
    assert_eq!(http::check(&m).await.status, HeartbeatStatus::Up);
    assert_eq!(fake.token_requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_revoked_cached_token_is_replaced_once() {
    let (addr, fake) = serve().await;
    let m = oauth(addr, 9106);
    assert_eq!(http::check(&m).await.status, HeartbeatStatus::Up);
    // The provider revokes everything issued so far.
    fake.min_token.store(2, Ordering::SeqCst);
    let out = http::check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{}", message(&out));
    assert_eq!(fake.token_requests.load(Ordering::SeqCst), 2);
    let seen = fake.api_auth.lock().unwrap().clone();
    assert_eq!(seen.last().unwrap().as_deref(), Some("Bearer tok-2"));
    // A fresh token that the API still refuses is a plain 401, not a retry loop.
    fake.min_token.store(99, Ordering::SeqCst);
    let out = http::check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert_eq!(message(&out), "HTTP 401 Unauthorized");
    assert_eq!(
        fake.token_requests.load(Ordering::SeqCst),
        3,
        "one refresh for the failing check"
    );
}

#[tokio::test]
async fn token_endpoint_rejection_is_a_clear_down_that_hides_the_secret() {
    let (addr, fake) = serve().await;
    fake.reject_credentials.store(true, Ordering::SeqCst);
    let out = http::check(&oauth(addr, 9107)).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    let msg = message(&out);
    assert!(
        msg.starts_with("OAuth2 token request failed: HTTP 401 Unauthorized"),
        "{msg}"
    );
    assert!(msg.contains("invalid_client"), "{msg}");
    assert!(
        !msg.contains(SECRET),
        "the provider echoed the secret, the message must not: {msg}"
    );
    assert!(
        fake.api_auth.lock().unwrap().is_empty(),
        "the target is not contacted without a token"
    );
}

#[tokio::test]
async fn unreachable_token_endpoint_is_a_clear_down() {
    let (addr, _) = serve().await;
    let closed = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let mut m = oauth(addr, 9108);
    m.oauth_token_url = Some(format!("http://{closed}/token?key=querytoken"));
    let out = http::check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    let msg = message(&out);
    assert!(msg.starts_with("OAuth2 token request failed"), "{msg}");
    assert!(
        !msg.contains(SECRET) && !msg.contains("querytoken"),
        "{msg}"
    );
}

#[tokio::test]
async fn unusable_token_responses_are_explained() {
    let (addr, fake) = serve().await;
    for (id, raw, expect) in [
        (9109, r#"{"token_type":"Bearer"}"#, "no access_token"),
        (9110, "not json", "not JSON"),
        (
            9111,
            r#"{"access_token":"x","token_type":"mac"}"#,
            "token type “mac” is not supported",
        ),
    ] {
        *fake.custom_response.lock().unwrap() = Some(raw.into());
        let out = http::check(&oauth(addr, id)).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        let msg = message(&out);
        assert!(
            msg.starts_with("OAuth2 token request failed:") && msg.contains(expect),
            "{msg}"
        );
    }
}

#[tokio::test]
async fn incomplete_oauth_settings_are_reported_as_down() {
    let (addr, _) = serve().await;
    let mut m = oauth(addr, 9112);
    m.oauth_client_secret = None;
    let out = http::check(&m).await;
    assert_eq!(
        (out.status, message(&out).as_str()),
        (HeartbeatStatus::Down, "No OAuth2 client secret configured")
    );
}

// ---------------------------------------------------------------------------
// mTLS
// ---------------------------------------------------------------------------

fn mtls(addr: SocketAddr, pki: &Pki) -> Monitor {
    let mut m = monitor(
        MonitorType::Http,
        &format!("https://127.0.0.1:{}/", addr.port()),
    );
    m.auth_method = "mtls".into();
    m.tls_client_cert = Some(pki.client_cert_pem.clone());
    m.tls_client_key = Some(pki.client_key_pem.clone());
    m.tls_ca_cert = Some(pki.ca_pem.clone());
    m
}

#[tokio::test]
async fn client_certificate_is_presented_to_a_server_that_requires_one() {
    let pki = Pki::new();
    let addr = test_pki::serve_requiring_client_cert(&pki).await;

    let out = http::check(&mtls(addr, &pki)).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{}", message(&out));
    assert_eq!(message(&out), "HTTP 200 OK");

    // Without the certificate the same server refuses the connection.
    let mut plain = mtls(addr, &pki);
    plain.auth_method = "none".into();
    plain.ignore_tls = true;
    assert_eq!(http::check(&plain).await.status, HeartbeatStatus::Down);

    // A certificate from another CA is refused too.
    let (cert, key) = test_pki::stranger_client();
    let mut stranger = mtls(addr, &pki);
    (stranger.tls_client_cert, stranger.tls_client_key) = (Some(cert), Some(key));
    assert_eq!(http::check(&stranger).await.status, HeartbeatStatus::Down);

    // And the extra CA is what makes the server trusted: without it the
    // check fails on the server certificate, not on the client side.
    let mut no_ca = mtls(addr, &pki);
    no_ca.tls_ca_cert = None;
    let out = http::check(&no_ca).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert!(!message(&out).contains("PRIVATE KEY"), "{}", message(&out));
}

#[test]
fn validate_mtls_reports_a_clear_error_per_mistake() {
    let pki = Pki::new();
    let (cert, key, ca) = (&pki.client_cert_pem, &pki.client_key_pem, &pki.ca_pem);
    assert_eq!(validate_mtls(cert, key, Some(ca)), Ok(()));
    assert_eq!(validate_mtls(cert, key, None), Ok(()));
    // Key from another certificate.
    assert!(matches!(
        validate_mtls(cert, &test_pki::unrelated_key_pem(), None),
        Err(MtlsError::Rejected(_))
    ));
    // Fields swapped / missing / garbage.
    assert_eq!(validate_mtls(key, cert, None), Err(MtlsError::CertNotPem));
    assert_eq!(validate_mtls("", key, None), Err(MtlsError::MissingCert));
    assert_eq!(validate_mtls(cert, "", None), Err(MtlsError::MissingKey));
    assert_eq!(validate_mtls(cert, cert, None), Err(MtlsError::KeyNotPem));
    assert_eq!(
        validate_mtls(cert, key, Some("not pem")),
        Err(MtlsError::CaNotPem)
    );
    let broken_ca = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";
    assert!(matches!(
        validate_mtls(cert, key, Some(broken_ca)),
        Err(MtlsError::CaNotPem | MtlsError::CaRejected(_))
    ));
}
