//! Router-level tests for login, 2FA, sessions, CSRF and API tokens.
//! Each test gets its own on-disk database and a tiny cookie-keeping client.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::response::Response;
use axum::routing::get;
use axum_extra::extract::cookie::Key;
use tower::ServiceExt;

use super::*;
use crate::api::build_router;
use crate::auth::Role;
use crate::config::Config;
use crate::db::api_tokens::TokenScope;
use crate::monitor::Scheduler;

const HOST: &str = "vole.test";
const ORIGIN: &str = "http://vole.test";
const PASSWORD: &str = "correct horse battery";

/// One Argon2 hash of [`PASSWORD`] shared by every test user (hashing is slow in debug builds).
static PASSWORD_HASH: LazyLock<String> =
    LazyLock::new(|| auth::try_hash_password(PASSWORD).unwrap());
const TOTP_SECRET: &str = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TestDir(PathBuf);

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn test_state() -> (AppState, TestDir) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("vv-auth-test-{}-{n}", std::process::id()));
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
        config: Arc::new(Config {
            data_dir: dir.clone(),
            bind_addr: "127.0.0.1:0".into(),
        }),
    };
    (state, TestDir(dir))
}

fn unique(prefix: &str) -> String {
    format!("{prefix}{}", COUNTER.fetch_add(1, Ordering::SeqCst))
}

async fn add_user(state: &AppState, username: &str, role: Role) -> i64 {
    let (name, hash) = (username.to_string(), PASSWORD_HASH.clone());
    state
        .with_conn(move |c| db_users::insert_user(c, &name, &hash, role))
        .await
        .unwrap()
}

async fn enable_totp(state: &AppState, id: i64, recovery: &[&str]) {
    let hashes =
        totp::hash_recovery_codes(&recovery.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .unwrap();
    state
        .with_conn(move |c| {
            db_users::set_pending_totp_secret(c, id, TOTP_SECRET)?;
            db_users::enable_totp(c, id, 0, &hashes)
        })
        .await
        .unwrap();
}

fn current_code() -> String {
    let t = totp::build(TOTP_SECRET, "x", "y").unwrap();
    totp::code_at(&t, u64::try_from(session::now_unix()).unwrap())
}

/// Keeps cookies between requests, like a browser would.
struct Client {
    app: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(app: Router) -> Self {
        Self {
            app,
            cookies: HashMap::new(),
        }
    }

    fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    async fn send(&mut self, req: axum::http::request::Builder, body: Body) -> Response {
        let req = req
            .header(header::HOST, HOST)
            .header(header::COOKIE, self.cookie_header());
        let response = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        for value in response.headers().get_all(header::SET_COOKIE) {
            let value = value.to_str().unwrap();
            let pair = value.split(';').next().unwrap();
            let (name, val) = pair.split_once('=').unwrap();
            let expired = value.to_ascii_lowercase().contains("max-age=0") || val.is_empty();
            if expired {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.to_string(), val.to_string());
            }
        }
        response
    }

    async fn get(&mut self, uri: &str) -> Response {
        self.send(Request::builder().uri(uri), Body::empty()).await
    }

    async fn post(&mut self, uri: &str, form: &[(&str, &str)]) -> Response {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        self.send(req, Body::from(body)).await
    }

    async fn login(&mut self, username: &str, password: &str) -> Response {
        self.post("/login", &[("username", username), ("password", password)])
            .await
    }
}

fn location(response: &Response) -> &str {
    response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

async fn body_text(response: Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fresh_install_redirects_protected_pages_to_setup() {
    let (state, _dir) = test_state().await;
    let mut c = Client::new(build_router(state));
    let r = c.get("/settings/security").await;
    assert_eq!(location(&r), "/setup");
}

#[tokio::test]
async fn setup_creates_the_first_admin_and_signs_in() {
    let (state, _dir) = test_state().await;
    let mut c = Client::new(build_router(state.clone()));
    let r = c
        .post(
            "/setup",
            &[
                ("username", "admin"),
                ("password", PASSWORD),
                ("confirm", PASSWORD),
                ("site_name", "Ops"),
                ("timezone", "Europe/Oslo"),
            ],
        )
        .await;
    assert_eq!((r.status(), location(&r)), (StatusCode::SEE_OTHER, "/"));
    let s = state.settings().await.unwrap();
    assert_eq!(
        (s.site_name.as_str(), s.timezone.as_str()),
        ("Ops", "Europe/Oslo")
    );
    assert_eq!(c.get("/settings/security").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn setup_rejects_weak_or_mismatched_passwords() {
    let (state, _dir) = test_state().await;
    let mut c = Client::new(build_router(state.clone()));
    let r = c
        .post(
            "/setup",
            &[
                ("username", "admin"),
                ("password", "short"),
                ("confirm", "short"),
            ],
        )
        .await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let r = c
        .post(
            "/setup",
            &[
                ("username", "admin"),
                ("password", PASSWORD),
                ("confirm", "different horse battery"),
            ],
        )
        .await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        state.with_conn(|c| db_users::count_users(c)).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn setup_is_closed_once_a_user_exists() {
    let (state, _dir) = test_state().await;
    add_user(&state, "owner", Role::Admin).await;
    let mut c = Client::new(build_router(state.clone()));
    let r = c
        .post(
            "/setup",
            &[
                ("username", "intruder"),
                ("password", PASSWORD),
                ("confirm", PASSWORD),
            ],
        )
        .await;
    assert_eq!(location(&r), "/login");
    assert_eq!(
        state.with_conn(|c| db_users::count_users(c)).await.unwrap(),
        1
    );
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_page_redirects_to_login_with_next() {
    let (state, _dir) = test_state().await;
    add_user(&state, "a", Role::Admin).await;
    let mut c = Client::new(build_router(state));
    let r = c.get("/settings/security?tab=x").await;
    assert_eq!(location(&r), "/login?next=%2Fsettings%2Fsecurity%3Ftab%3Dx");
}

#[tokio::test]
async fn login_sets_a_hardened_session_cookie() {
    let (state, _dir) = test_state().await;
    let name = unique("cookie");
    add_user(&state, &name, Role::Viewer).await;
    let mut c = Client::new(build_router(state));
    let r = c.login(&name, PASSWORD).await;
    let cookie = r
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("vv_session="))
        .unwrap()
        .to_string();
    for attr in ["HttpOnly", "SameSite=Lax", "Path=/", "Max-Age=2592000"] {
        assert!(cookie.contains(attr), "{cookie} lacks {attr}");
    }
    assert!(
        !cookie.contains("Secure"),
        "plain http must not set Secure: {cookie}"
    );
}

#[tokio::test]
async fn login_failure_message_is_generic() {
    let (state, _dir) = test_state().await;
    let name = unique("generic");
    add_user(&state, &name, Role::Viewer).await;
    let mut c = Client::new(build_router(state));
    let wrong_password = c.login(&name, "not the password!").await;
    assert_eq!(wrong_password.status(), StatusCode::UNAUTHORIZED);
    let a = body_text(wrong_password).await;
    let unknown_user = c.login(&unique("nobody"), "not the password!").await;
    assert_eq!(unknown_user.status(), StatusCode::UNAUTHORIZED);
    let b = body_text(unknown_user).await;
    let failed = crate::i18n::t("auth.login_failed");
    assert!(a.contains(failed) && b.contains(failed));
}

#[tokio::test]
async fn login_follows_same_site_next_only() {
    let (state, _dir) = test_state().await;
    let name = unique("next");
    add_user(&state, &name, Role::Viewer).await;
    let mut c = Client::new(build_router(state));
    let r = c
        .post(
            "/login",
            &[
                ("username", &name),
                ("password", PASSWORD),
                ("next", "/monitors/3"),
            ],
        )
        .await;
    assert_eq!(location(&r), "/monitors/3");
    let mut c2 = Client::new(c.app.clone());
    let r = c2
        .post(
            "/login",
            &[
                ("username", &name),
                ("password", PASSWORD),
                ("next", "//evil.example/x"),
            ],
        )
        .await;
    assert_eq!(location(&r), "/");
}

#[tokio::test]
async fn usernames_match_case_insensitively() {
    let (state, _dir) = test_state().await;
    let name = unique("Case");
    add_user(&state, &name, Role::Viewer).await;
    let mut c = Client::new(build_router(state));
    assert_eq!(
        location(&c.login(&name.to_uppercase(), PASSWORD).await),
        "/"
    );
}

#[tokio::test]
async fn disabled_users_cannot_log_in() {
    let (state, _dir) = test_state().await;
    add_user(&state, "admin", Role::Admin).await;
    let name = unique("disabled");
    let id = add_user(&state, &name, Role::Viewer).await;
    state
        .with_conn(move |c| db_users::set_active(c, id, false))
        .await
        .unwrap()
        .unwrap();
    let mut c = Client::new(build_router(state));
    assert_eq!(
        c.login(&name, PASSWORD).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn repeated_failures_are_throttled_even_for_the_right_password() {
    let (state, _dir) = test_state().await;
    let name = unique("throttle");
    add_user(&state, &name, Role::Viewer).await;
    let mut c = Client::new(build_router(state));
    for _ in 0..6 {
        c.login(&name, "wrong password 123").await;
    }
    let r = c.login(&name, PASSWORD).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(body_text(r).await.contains("Too many attempts"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_login_burst_cannot_exceed_the_attempt_budget() {
    let (state, _dir) = test_state().await;
    let name = unique("burst");
    add_user(&state, &name, Role::Viewer).await;
    let app = build_router(state);
    let attempts = (0..12).map(|_| {
        let mut c = Client::new(app.clone());
        let name = name.clone();
        tokio::spawn(async move { c.login(&name, "wrong password 123").await.status() })
    });
    let mut checked = 0;
    for handle in attempts.collect::<Vec<_>>() {
        if handle.await.unwrap() != StatusCode::TOO_MANY_REQUESTS {
            checked += 1;
        }
    }
    // 5 free failures + the one that starts the backoff; the rest are refused
    // before Argon2 even though none of them had finished yet (one more only
    // if the test machine stalls for over 2 s mid-burst). Before the fix all
    // 12 were checked.
    assert!(
        (6..=7).contains(&checked),
        "{checked} attempts reached Argon2"
    );
}

#[tokio::test]
async fn logout_clears_the_session() {
    let (state, _dir) = test_state().await;
    let name = unique("logout");
    add_user(&state, &name, Role::Viewer).await;
    let mut c = Client::new(build_router(state));
    c.login(&name, PASSWORD).await;
    assert_eq!(c.post("/logout", &[]).await.status(), StatusCode::SEE_OTHER);
    assert!(location(&c.get("/settings/security").await).starts_with("/login"));
}

// ---------------------------------------------------------------------------
// Two-factor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_factor_login_flow() {
    let (state, _dir) = test_state().await;
    let name = unique("tfa");
    let id = add_user(&state, &name, Role::Viewer).await;
    enable_totp(&state, id, &[]).await;
    let mut c = Client::new(build_router(state));

    let r = c
        .post(
            "/login",
            &[
                ("username", &name),
                ("password", PASSWORD),
                ("next", "/settings/security"),
            ],
        )
        .await;
    assert_eq!(location(&r), "/login/2fa");
    // The password alone grants nothing.
    assert!(location(&c.get("/settings/security").await).starts_with("/login"));
    assert_eq!(c.get("/login/2fa").await.status(), StatusCode::OK);

    let r = c.post("/login/2fa", &[("code", "000000")]).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    let r = c.post("/login/2fa", &[("code", &current_code())]).await;
    assert_eq!(location(&r), "/settings/security");
    assert_eq!(c.get("/settings/security").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn totp_code_cannot_be_replayed() {
    let (state, _dir) = test_state().await;
    let name = unique("replay");
    let id = add_user(&state, &name, Role::Viewer).await;
    enable_totp(&state, id, &[]).await;
    let app = build_router(state);
    let code = current_code();

    let mut first = Client::new(app.clone());
    first.login(&name, PASSWORD).await;
    assert_eq!(
        location(&first.post("/login/2fa", &[("code", &code)]).await),
        "/"
    );

    let mut second = Client::new(app);
    second.login(&name, PASSWORD).await;
    let r = second.post("/login/2fa", &[("code", &code)]).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn recovery_codes_work_once() {
    let (state, _dir) = test_state().await;
    let name = unique("recovery");
    let id = add_user(&state, &name, Role::Viewer).await;
    enable_totp(&state, id, &["abcde-fghjk", "mnpqr-stuvw"]).await;
    let app = build_router(state);

    let mut first = Client::new(app.clone());
    first.login(&name, PASSWORD).await;
    assert_eq!(
        location(&first.post("/login/2fa", &[("code", "ABCDE FGHJK")]).await),
        "/"
    );

    let mut second = Client::new(app);
    second.login(&name, PASSWORD).await;
    assert_eq!(
        second
            .post("/login/2fa", &[("code", "abcde-fghjk")])
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn two_factor_step_without_password_step_is_refused() {
    let (state, _dir) = test_state().await;
    add_user(&state, "a", Role::Admin).await;
    let mut c = Client::new(build_router(state));
    let r = c.post("/login/2fa", &[("code", &current_code())]).await;
    assert_eq!(location(&r), "/login");
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn password_change_signs_out_other_sessions_but_not_this_one() {
    let (state, _dir) = test_state().await;
    let name = unique("pwchange");
    add_user(&state, &name, Role::Viewer).await;
    let app = build_router(state);
    let mut here = Client::new(app.clone());
    let mut elsewhere = Client::new(app);
    here.login(&name, PASSWORD).await;
    elsewhere.login(&name, PASSWORD).await;

    let new = "a brand new passphrase";
    let r = here
        .post(
            "/settings/security/password",
            &[
                ("current_password", PASSWORD),
                ("new_password", new),
                ("confirm_password", new),
            ],
        )
        .await;
    assert_eq!(location(&r), "/settings/security");
    assert_eq!(
        here.get("/settings/security").await.status(),
        StatusCode::OK
    );
    assert!(location(&elsewhere.get("/settings/security").await).starts_with("/login"));
}

#[tokio::test]
async fn sign_out_other_sessions_keeps_the_current_one() {
    let (state, _dir) = test_state().await;
    let name = unique("others");
    add_user(&state, &name, Role::Viewer).await;
    let app = build_router(state);
    let mut here = Client::new(app.clone());
    let mut elsewhere = Client::new(app);
    here.login(&name, PASSWORD).await;
    elsewhere.login(&name, PASSWORD).await;
    here.post("/settings/security/sessions", &[]).await;
    assert_eq!(
        here.get("/settings/security").await.status(),
        StatusCode::OK
    );
    assert!(location(&elsewhere.get("/settings/security").await).starts_with("/login"));
}

#[tokio::test]
async fn disabling_a_user_ends_their_session() {
    let (state, _dir) = test_state().await;
    add_user(&state, "admin", Role::Admin).await;
    let name = unique("victim");
    let id = add_user(&state, &name, Role::Viewer).await;
    let mut c = Client::new(build_router(state.clone()));
    c.login(&name, PASSWORD).await;
    state
        .with_conn(move |c| db_users::set_active(c, id, false))
        .await
        .unwrap()
        .unwrap();
    assert!(location(&c.get("/settings/security").await).starts_with("/login"));
}

#[tokio::test]
async fn forged_or_legacy_session_cookie_is_rejected() {
    let (state, _dir) = test_state().await;
    add_user(&state, "a", Role::Admin).await;
    let mut c = Client::new(build_router(state));
    c.cookies.insert("vv_session".into(), "1".into());
    assert!(location(&c.get("/settings/security").await).starts_with("/login"));
}

// ---------------------------------------------------------------------------
// CSRF
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cross_site_posts_are_blocked() {
    let (state, _dir) = test_state().await;
    let name = unique("csrf");
    add_user(&state, &name, Role::Viewer).await;
    let mut c = Client::new(build_router(state));
    c.login(&name, PASSWORD).await;

    let no_origin = c
        .send(
            Request::builder()
                .method("POST")
                .uri("/settings/security/sessions"),
            Body::empty(),
        )
        .await;
    assert_eq!(no_origin.status(), StatusCode::FORBIDDEN);

    let evil = c
        .send(
            Request::builder()
                .method("POST")
                .uri("/settings/security/sessions")
                .header(header::ORIGIN, "http://evil.example"),
            Body::empty(),
        )
        .await;
    assert_eq!(evil.status(), StatusCode::FORBIDDEN);

    let htmx = c
        .send(
            Request::builder()
                .method("POST")
                .uri("/settings/security/sessions")
                .header("hx-request", "true"),
            Body::empty(),
        )
        .await;
    assert_eq!(htmx.status(), StatusCode::SEE_OTHER);
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

#[tokio::test]
async fn viewers_cannot_manage_users_or_general_settings() {
    let (state, _dir) = test_state().await;
    let name = unique("viewer");
    add_user(&state, &name, Role::Viewer).await;
    let mut c = Client::new(build_router(state));
    c.login(&name, PASSWORD).await;
    assert_eq!(
        c.get("/settings/users").await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        c.get("/settings/general").await.status(),
        StatusCode::FORBIDDEN
    );
    let r = c
        .post(
            "/settings/users",
            &[("username", "x"), ("role", "admin"), ("password", PASSWORD)],
        )
        .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_pages_render() {
    let (state, _dir) = test_state().await;
    let name = unique("admin");
    let id = add_user(&state, &name, Role::Admin).await;
    add_user(&state, &unique("other"), Role::Editor).await;
    enable_totp(&state, id, &["abcde-fghjk"]).await;
    let mut c = Client::new(build_router(state));
    // Log in via 2FA to make sure the full path works for admins too.
    c.login(&name, PASSWORD).await;
    c.post("/login/2fa", &[("code", "abcde-fghjk")]).await;
    for page in [
        "/settings/general",
        "/settings/users",
        "/settings/security",
        "/settings/tokens",
    ] {
        let r = c.get(page).await;
        assert_eq!(r.status(), StatusCode::OK, "{page}");
    }
    assert_eq!(location(&c.get("/settings").await), "/settings/general");
}

#[tokio::test]
async fn general_settings_save_the_agent_confirmation_default() {
    let (state, _dir) = test_state().await;
    let name = unique("admin");
    add_user(&state, &name, Role::Admin).await;
    let mut c = Client::new(build_router(state.clone()));
    c.login(&name, PASSWORD).await;
    let base = [
        ("site_name", "Vole"),
        ("base_url", ""),
        ("timezone", "UTC"),
        ("language", "en"),
        ("retention_days", "90"),
    ];
    let mut on = base.to_vec();
    on.push(("agent_confirmation_default", "true"));
    let r = c.post("/settings/general", &on).await;
    assert_eq!(location(&r), "/settings/general");
    assert!(state.settings().await.unwrap().agent_confirmation_default);
    assert!(
        body_text(c.get("/settings/general").await)
            .await
            .contains("name=\"agent_confirmation_default\"")
    );

    c.post("/settings/general", &base).await;
    assert!(!state.settings().await.unwrap().agent_confirmation_default);
}

#[tokio::test]
async fn admin_cannot_delete_or_disable_themselves() {
    let (state, _dir) = test_state().await;
    let name = unique("selfdel");
    let id = add_user(&state, &name, Role::Admin).await;
    let mut c = Client::new(build_router(state.clone()));
    c.login(&name, PASSWORD).await;
    c.post(&format!("/settings/users/{id}/delete"), &[]).await;
    c.post(&format!("/settings/users/{id}/disable"), &[]).await;
    let user = state
        .with_conn(move |c| db_users::get_user(c, id))
        .await
        .unwrap()
        .unwrap();
    assert!(user.active);
}

#[tokio::test]
async fn admin_creates_user_who_can_log_in() {
    let (state, _dir) = test_state().await;
    let admin = unique("boss");
    add_user(&state, &admin, Role::Admin).await;
    let app = build_router(state);
    let mut c = Client::new(app.clone());
    c.login(&admin, PASSWORD).await;
    let newbie = unique("newbie");
    let r = c
        .post(
            "/settings/users",
            &[
                ("username", &newbie),
                ("display_name", "New Bie"),
                ("role", "editor"),
                ("password", "temporary passphrase"),
            ],
        )
        .await;
    assert_eq!(location(&r), "/settings/users");
    let mut n = Client::new(app);
    assert_eq!(
        location(&n.login(&newbie, "temporary passphrase").await),
        "/"
    );
}

// ---------------------------------------------------------------------------
// API tokens
// ---------------------------------------------------------------------------

/// A protected `/api/` route that reports who the request runs as.
fn whoami_app(state: AppState) -> Router {
    Router::new()
        .route(
            "/api/v1/whoami",
            get(|user: CurrentUser| async move { format!("{}:{}", user.username, user.role) }),
        )
        .route_layer(from_fn_with_state(state.clone(), require_auth))
        .layer(axum::middleware::from_fn(crate::api::error::negotiate))
        .with_state(state)
}

async fn create_token(
    state: &AppState,
    user_id: i64,
    scope: TokenScope,
    days: Option<i64>,
) -> (i64, String) {
    let token = tokens::generate();
    let (hash, prefix) = (tokens::hash(&token), tokens::display_prefix(&token));
    let id = state
        .with_conn(move |c| db_tokens::insert(c, user_id, "t", &prefix, &hash, scope, days))
        .await
        .unwrap();
    (id, token)
}

async fn whoami(app: &Router, auth_header: Option<String>) -> (StatusCode, String) {
    let mut req = Request::builder().uri("/api/v1/whoami");
    if let Some(h) = auth_header {
        req = req.header(header::AUTHORIZATION, h);
    }
    let r = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = r.status();
    (status, body_text(r).await)
}

#[tokio::test]
async fn bearer_token_authenticates_with_scope_cap() {
    let (state, _dir) = test_state().await;
    let id = add_user(&state, "root", Role::Admin).await;
    let (_, read) = create_token(&state, id, TokenScope::Read, None).await;
    let (_, write) = create_token(&state, id, TokenScope::Write, Some(30)).await;
    let app = whoami_app(state);
    assert_eq!(
        whoami(&app, Some(format!("Bearer {read}"))).await,
        (StatusCode::OK, "root:viewer".into())
    );
    assert_eq!(
        whoami(&app, Some(format!("Bearer {write}"))).await,
        (StatusCode::OK, "root:admin".into())
    );
}

#[tokio::test]
async fn basic_auth_password_token_is_accepted() {
    use base64::Engine as _;
    let (state, _dir) = test_state().await;
    let id = add_user(&state, "prom", Role::Viewer).await;
    let (_, token) = create_token(&state, id, TokenScope::Read, None).await;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("prometheus:{token}"));
    let app = whoami_app(state);
    assert_eq!(
        whoami(&app, Some(format!("Basic {basic}"))).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn invalid_expired_and_revoked_tokens_are_rejected() {
    let (state, _dir) = test_state().await;
    let id = add_user(&state, "root", Role::Admin).await;
    let (expired_id, expired) = create_token(&state, id, TokenScope::Write, Some(1)).await;
    state
        .with_conn(move |c| {
            c.execute(
                "UPDATE api_tokens SET expires_at = datetime('now', '-1 minute') WHERE id = ?1",
                [expired_id],
            )
        })
        .await
        .unwrap();
    let (revoked_id, revoked) = create_token(&state, id, TokenScope::Write, None).await;
    state
        .with_conn(move |c| db_tokens::delete(c, revoked_id))
        .await
        .unwrap();
    let app = whoami_app(state);
    for header in [
        format!("Bearer {expired}"),
        format!("Bearer {revoked}"),
        format!("Bearer {}", tokens::generate()),
        "Bearer not-a-token".into(),
        "Digest x".into(),
    ] {
        assert_eq!(
            whoami(&app, Some(header.clone())).await.0,
            StatusCode::UNAUTHORIZED,
            "{header}"
        );
    }
    assert_eq!(whoami(&app, None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn token_use_is_recorded() {
    let (state, _dir) = test_state().await;
    let id = add_user(&state, "root", Role::Admin).await;
    let (token_id, token) = create_token(&state, id, TokenScope::Read, None).await;
    let app = whoami_app(state.clone());
    whoami(&app, Some(format!("Bearer {token}"))).await;
    let t = state
        .with_conn(move |c| db_tokens::get(c, token_id))
        .await
        .unwrap()
        .unwrap();
    assert!(t.last_used_at.is_some());
}

#[tokio::test]
async fn authenticate_token_helper_resolves_users() {
    let (state, _dir) = test_state().await;
    let id = add_user(&state, "helper", Role::Editor).await;
    let (_, token) = create_token(&state, id, TokenScope::Write, None).await;
    let user = authenticate_token(&state, &token).await.unwrap().unwrap();
    assert_eq!((user.id, user.role), (id, Role::Editor));
    assert!(
        authenticate_token(&state, "vv_nope")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn token_creation_page_shows_the_token_once() {
    let (state, _dir) = test_state().await;
    let name = unique("tokener");
    add_user(&state, &name, Role::Editor).await;
    let mut c = Client::new(build_router(state.clone()));
    c.login(&name, PASSWORD).await;
    let r = c
        .post(
            "/settings/tokens",
            &[("name", "CI"), ("scope", "write"), ("expires", "90")],
        )
        .await;
    assert_eq!(r.status(), StatusCode::OK);
    let html = body_text(r).await;
    let start = html.find("vv_").unwrap();
    let token: String = html[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    assert!(tokens::is_well_formed(&token), "{token}");
    let user = authenticate_token(&state, &token).await.unwrap().unwrap();
    assert_eq!(user.role, Role::Editor);
    // The list afterwards only shows the prefix.
    let list = body_text(c.get("/settings/tokens").await).await;
    assert!(!list.contains(&token));
}

#[tokio::test]
async fn users_cannot_revoke_other_peoples_tokens() {
    let (state, _dir) = test_state().await;
    let owner = add_user(&state, &unique("owner"), Role::Editor).await;
    let (token_id, _) = create_token(&state, owner, TokenScope::Read, None).await;
    let name = unique("snoop");
    add_user(&state, &name, Role::Editor).await;
    let mut c = Client::new(build_router(state.clone()));
    c.login(&name, PASSWORD).await;
    let r = c
        .send(
            Request::builder()
                .method("DELETE")
                .uri(format!("/settings/tokens/{token_id}"))
                .header("hx-request", "true"),
            Body::empty(),
        )
        .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert!(
        state
            .with_conn(move |c| db_tokens::get(c, token_id))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn token_header_never_falls_back_to_the_session_cookie() {
    let (state, _dir) = test_state().await;
    let name = unique("mixed");
    add_user(&state, &name, Role::Admin).await;
    let app = build_router(state.clone());
    let mut c = Client::new(app);
    c.login(&name, PASSWORD).await;
    let app = whoami_app(state);
    let cookie = c.cookie_header();
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/whoami")
                .header(header::COOKIE, &cookie)
                .header(header::AUTHORIZATION, "Bearer vv_wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // The cookie alone still works on /api/ routes.
    let r = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/whoami")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn session_survives_a_server_restart() {
    let (state, dir) = test_state().await;
    // Both "processes" load the persisted key from DATA_DIR, like server::run.
    let key = auth::load_or_create_key(&dir.0);
    let first = AppState {
        session_key: key,
        ..state.clone()
    };
    let mut c = Client::new(build_router(first));
    let r = c
        .post(
            "/setup",
            &[
                ("username", "admin"),
                ("password", PASSWORD),
                ("confirm", PASSWORD),
            ],
        )
        .await;
    assert_eq!(location(&r), "/");

    // "Restart": fresh pool on the same database file, key re-read from disk.
    let pool = crate::db::build_pool(&dir.0.join("test.db"));
    crate::db::run_migrations(&pool, &dir.0.join("backups")).await;
    let restarted = AppState {
        db: pool,
        session_key: auth::load_or_create_key(&dir.0),
        ..state
    };
    c.app = build_router(restarted);
    assert_eq!(c.get("/settings/security").await.status(), StatusCode::OK);
}
