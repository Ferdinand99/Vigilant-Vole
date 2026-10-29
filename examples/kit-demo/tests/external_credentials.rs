//! A product that signs people in on its own pages
//! (`CredentialMode::External`): the kit's sign-in, setup and password
//! forms point there, the places that asked for the current password ask
//! for a fresh confirmation instead, and the passkey sign-in stays. This is
//! its own test binary because the kit's configuration is installed once
//! per process.
//!
//! The "product" here is a few routes in this file, standing in for what a
//! real one does after its own checks: create the first admin, sign a user
//! in, grant a confirmation.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use vole_kit::auth::guard::{self, PeerAddr, login_jar, secure_cookies};
use vole_kit::auth::pages::{check_setup_token, insert_first_admin};
use vole_kit::auth::{Role, reauth, session};
use vole_kit::db;
use vole_kit::state::Core;

static N: AtomicUsize = AtomicUsize::new(0);
const ADMIN: &str = "ada";
const SECRET: &str = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";
const REFUSED: &str = "Confirm it’s you to make this change.";

// ---------------------------------------------------------------------------
// The product's own routes
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct Q {
    user: Option<String>,
    next: Option<String>,
    token: Option<String>,
}

/// Its setup page: checks the setup token, creates the first admin with a
/// hash nobody can log in with, and signs them in.
async fn vault_setup(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    Query(q): Query<Q>,
) -> Response {
    check_setup_token(&core, &headers, peer, q.token.as_deref()).expect("setup is open in tests");
    let name = q.user.unwrap_or_else(|| ADMIN.to_string());
    let id = insert_first_admin(&core, &name, "!external", "Kit Demo", "UTC")
        .await
        .unwrap()
        .expect("the first admin");
    let user = core
        .with_conn(move |c| db::users::get_user(c, id))
        .await
        .unwrap()
        .unwrap();
    let jar = login_jar(jar, user.id, user.session_version, secure_cookies(&headers));
    (jar, Redirect::to("/")).into_response()
}

/// Its sign-in page, after its own checks passed.
async fn vault_login(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> Response {
    let name = q.user.unwrap_or_else(|| ADMIN.to_string());
    let user = core
        .with_conn(move |c| db::users::get_user_by_username(c, &name))
        .await
        .unwrap()
        .expect("a user to sign in");
    let jar = login_jar(jar, user.id, user.session_version, secure_cookies(&headers));
    let next = session::sanitize_next(q.next.as_deref());
    (jar, Redirect::to(&next)).into_response()
}

/// Its confirmation page, after its own checks passed: grants a fresh
/// confirmation for the signed-in session.
async fn vault_reauth(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> Response {
    let (user, claims) = guard::resolve_session(&core, &jar)
        .await
        .unwrap()
        .expect("signed in");
    let jar = reauth::grant(jar, user.id, claims.version, secure_cookies(&headers));
    let next = session::sanitize_next(q.next.as_deref());
    (jar, Redirect::to(&next)).into_response()
}

fn vault(core: &Core) -> Router {
    Router::new()
        .route("/vault/setup", get(vault_setup))
        .route("/vault/login", get(vault_login))
        .route("/vault/reauth", get(vault_reauth))
        .with_state(core.clone())
}

// ---------------------------------------------------------------------------
// A tiny browser
// ---------------------------------------------------------------------------

struct Res {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Res {
    fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    /// The `Set-Cookie` lines for one cookie.
    fn set_cookies(&self, name: &str) -> Vec<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter(|v| v.starts_with(&format!("{name}=")))
            .map(str::to_string)
            .collect()
    }
}

struct App {
    core: Core,
    router: Router,
    cookies: BTreeMap<String, String>,
    dir: PathBuf,
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl App {
    /// A fresh instance without users.
    async fn fresh() -> App {
        let dir = std::env::temp_dir().join(format!(
            "kit-demo-external-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let demo = kit_demo::start_with(&dir, &kit_demo::KIT_EXTERNAL)
            .await
            .unwrap();
        let router = kit_demo::router(&demo).merge(vault(&demo.core));
        App {
            core: demo.core.clone(),
            router,
            cookies: BTreeMap::new(),
            dir,
        }
    }

    /// An instance whose admin the product created and signed in.
    async fn set_up() -> App {
        let mut app = App::fresh().await;
        let res = app.get("/vault/setup").await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        assert!(app.cookies.contains_key("kd_session"));
        app
    }

    async fn send(&mut self, mut req: Request<Body>) -> Res {
        let h = req.headers_mut();
        h.insert(header::HOST, HeaderValue::from_static("demo.test"));
        if !self.cookies.is_empty() {
            let cookie = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            h.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        }
        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));
        let res = self.router.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        for line in headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
        {
            let pair = line.split(';').next().unwrap();
            let (name, value) = pair.split_once('=').unwrap();
            if value.is_empty() || line.contains("Max-Age=0") {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.to_string(), value.to_string());
            }
        }
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        Res {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    async fn get(&mut self, path: &str) -> Res {
        self.send(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    async fn post(&mut self, path: &str, form: &[(&str, &str)]) -> Res {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let req = Request::post(path)
            .header(header::ORIGIN, "http://demo.test")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        self.send(req).await
    }

    async fn post_json(&mut self, path: &str, body: &Value) -> Res {
        let req = Request::post(path)
            .header(header::ORIGIN, "http://demo.test")
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(req).await
    }

    async fn session_version(&self, id: i64) -> i64 {
        self.core
            .with_conn(move |c| db::users::get_user(c, id))
            .await
            .unwrap()
            .unwrap()
            .session_version
    }

    async fn token_count(&self) -> usize {
        self.core
            .with_conn(|c| db::api_tokens::list_for_user(c, 1))
            .await
            .unwrap()
            .len()
    }

    /// Two-factor on for user `id`, so the Security page has its sheets.
    async fn enable_two_factor(&self, id: i64) {
        self.core
            .with_conn(move |c| {
                db::users::set_pending_totp_secret(c, id, SECRET)?;
                db::users::enable_totp(c, id, 0, &[])
            })
            .await
            .unwrap();
    }

    /// The value of a grant cookie issued at `issued_at` for another
    /// user or session, as the browser would send it back.
    fn forged_grant(&self, user_id: i64, version: i64, issued_at: i64) -> String {
        let jar = SignedCookieJar::new(self.core.session_key.clone());
        let jar = reauth::grant_at(jar, user_id, version, issued_at, false);
        let res = (jar, ()).into_response();
        let line = res.headers().get(header::SET_COOKIE).unwrap();
        let pair = line.to_str().unwrap().split(';').next().unwrap();
        pair.split_once('=').unwrap().1.to_string()
    }

    const NEW_TOKEN: &[(&str, &str)] = &[("name", "ci"), ("scope", "write"), ("expires", "90")];
}

fn now() -> i64 {
    session::now_unix()
}

// ---------------------------------------------------------------------------
// Sign-in and setup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sign_in_and_setup_lead_to_the_products_pages() {
    let mut app = App::fresh().await;
    let go = |loc: &str| (StatusCode::SEE_OTHER, loc.to_string());
    for (path, expected) in [
        ("/login", "/vault/login"),
        (
            "/login?next=/settings/security",
            "/vault/login?next=%2Fsettings%2Fsecurity",
        ),
        (
            "/login?next=%2Fsettings%2Ftokens%3Fa%3Db",
            "/vault/login?next=%2Fsettings%2Ftokens%3Fa%3Db",
        ),
        // A next the kit would not follow is dropped, not forwarded.
        ("/login?next=//evil.test", "/vault/login"),
        ("/login?next=https://evil.test/", "/vault/login"),
        ("/login?next=/setup", "/vault/login"),
        ("/setup", "/vault/setup"),
        ("/setup?token=abc123", "/vault/setup?token=abc123"),
    ] {
        let res = app.get(path).await;
        assert_eq!(
            (res.status, res.location().to_string()),
            go(expected),
            "{path}"
        );
    }
    // POST /setup leads there too; POST /login is gone.
    let res = app
        .post("/setup", &[("username", ADMIN), ("password", "x")])
        .await;
    assert_eq!(
        (res.status, res.location()),
        (StatusCode::SEE_OTHER, "/vault/setup")
    );
    let res = app
        .post("/login", &[("username", ADMIN), ("password", "x")])
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    // Nobody is set up: a protected page still goes to /setup, then on.
    let res = app.get("/").await;
    assert_eq!(
        (res.status, res.location()),
        (StatusCode::SEE_OTHER, "/setup")
    );
    assert!(app.cookies.is_empty());

    // The product creates the first admin with its own hash.
    let res = app.get("/vault/setup").await;
    assert_eq!((res.status, res.location()), (StatusCode::SEE_OTHER, "/"));
    assert!(app.cookies.contains_key("kd_session"));
    let user = app
        .core
        .with_conn(|c| db::users::get_user(c, 1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((user.username.as_str(), user.role), (ADMIN, Role::Admin));
    assert_eq!(user.password_hash, "!external");
    assert_eq!(app.get("/").await.status, StatusCode::OK);
    // The kit's /setup keeps pointing to the product's page.
    let res = app.get("/setup").await;
    assert_eq!(res.location(), "/vault/setup");

    // No password, whatever the hash is: the kit does not sign anyone in.
    let mut other = App::set_up().await;
    other.cookies.clear();
    let res = other
        .post("/login", &[("username", ADMIN), ("password", "!external")])
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(other.cookies.is_empty());
    // The product signs in (as after its own check).
    let res = other.get("/vault/login?next=/settings/security").await;
    assert_eq!(res.location(), "/settings/security");
    assert_eq!(other.get("/settings/security").await.status, StatusCode::OK);
}

#[tokio::test]
async fn the_kits_password_routes_are_gone() {
    let mut app = App::set_up().await;
    app.core
        .with_conn(|c| db::users::insert_user(c, "bob", "!external", Role::Viewer))
        .await
        .unwrap();
    for (path, form) in [
        (
            "/settings/users",
            vec![
                ("username", "eve"),
                ("role", "viewer"),
                ("password", "A-long-password-2026!"),
            ],
        ),
        (
            "/settings/users/2/password",
            vec![("password", "A-long-password-2026!")],
        ),
        (
            "/settings/security/password",
            vec![
                ("current_password", "x"),
                ("new_password", "A-long-password-2026!"),
                ("confirm_password", "A-long-password-2026!"),
            ],
        ),
    ] {
        let res = app.post(path, &form).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}: {}", res.body);
    }
    let users = app
        .core
        .with_conn(|c| db::users::count_users(c))
        .await
        .unwrap();
    assert_eq!(users, 2, "nobody was created");
    let hash = app
        .core
        .with_conn(|c| db::users::get_user(c, 2))
        .await
        .unwrap()
        .unwrap()
        .password_hash;
    assert_eq!(hash, "!external");
    // The pages themselves are still there.
    for path in ["/settings/users", "/settings/security", "/settings/tokens"] {
        assert_eq!(app.get(path).await.status, StatusCode::OK, "{path}");
    }
}

// ---------------------------------------------------------------------------
// The pages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_pages_have_no_password_forms() {
    let mut app = App::set_up().await;
    app.enable_two_factor(1).await;
    app.core
        .with_conn(|c| {
            db::users::insert_user(c, "bob", "!external", Role::Viewer)?;
            db::users::set_pending_totp_secret(c, 2, SECRET)?;
            db::users::enable_totp(c, 2, 0, &[])
        })
        .await
        .unwrap();

    let security = app.get("/settings/security").await;
    assert_eq!(security.status, StatusCode::OK);
    assert!(
        security.body.contains(r#"href="/vault/password""#),
        "{}",
        security.body
    );
    assert!(
        !security
            .body
            .contains(r#"action="/settings/security/password""#)
    );
    assert!(!security.body.contains("new_password"));
    // The 2FA sheets ask for a confirmation, not a password.
    let confirm = r#"href="/vault/reauth?next=%2Fsettings%2Fsecurity""#;
    assert!(security.body.contains(confirm), "{}", security.body);
    assert!(security.body.contains("Confirm it’s you"));

    let users = app.get("/settings/users").await;
    assert_eq!(users.status, StatusCode::OK);
    assert!(!users.body.contains("new-user"), "no new-user sheet");
    assert!(!users.body.contains("reset-pw-"), "no reset-password sheet");
    assert!(!users.body.contains("/password\""), "{}", users.body);
    assert!(
        users
            .body
            .contains(r#"href="/vault/reauth?next=%2Fsettings%2Fusers""#)
    );
    assert!(users.body.contains("reset-tfa-2"), "2FA reset stays");

    let tokens = app.get("/settings/tokens").await;
    assert_eq!(tokens.status, StatusCode::OK);
    assert!(
        tokens
            .body
            .contains(r#"href="/vault/reauth?next=%2Fsettings%2Ftokens""#)
    );

    for (path, body) in [
        ("/settings/security", &security.body),
        ("/settings/users", &users.body),
        ("/settings/tokens", &tokens.body),
    ] {
        assert!(
            !body.contains(r#"type="password""#),
            "{path}: password input"
        );
        assert!(
            !body.contains(r#"name="current_password""#),
            "{path}: current_password"
        );
        assert!(!body.contains(r#"name="password""#), "{path}: password");
    }

    // Once confirmed there is nothing left to ask on any of them.
    app.get("/vault/reauth").await;
    for path in ["/settings/security", "/settings/users", "/settings/tokens"] {
        let body = app.get(path).await.body;
        assert!(!body.contains("/vault/reauth"), "{path} still asks");
        assert!(!body.contains("Confirm it’s you"), "{path} still asks");
    }
}

// ---------------------------------------------------------------------------
// The confirmation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_a_grant_the_change_is_refused() {
    let mut app = App::set_up().await;
    let res = app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(
        (res.status, res.location()),
        (StatusCode::SEE_OTHER, "/settings/tokens")
    );
    assert_eq!(app.token_count().await, 0);
    let page = app.get("/settings/tokens").await;
    assert!(page.body.contains(REFUSED), "{}", page.body);
    // A read-only token that expires needs no confirmation, as before.
    let res = app
        .post(
            "/settings/tokens",
            &[("name", "ro"), ("scope", "read"), ("expires", "30")],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(app.token_count().await, 1);
    // A password sent along counts for nothing.
    let mut with_password = App::NEW_TOKEN.to_vec();
    with_password.push(("current_password", "!external"));
    app.post("/settings/tokens", &with_password).await;
    assert_eq!(app.token_count().await, 1);
}

#[tokio::test]
async fn a_fresh_grant_is_accepted_for_five_minutes() {
    let mut app = App::set_up().await;
    app.enable_two_factor(1).await;

    // The products page sets it.
    let res = app.get("/vault/reauth?next=/settings/tokens").await;
    assert_eq!(res.location(), "/settings/tokens");
    let cookie = res.set_cookies("kd_reauth");
    assert_eq!(cookie.len(), 1, "{cookie:?}");
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=300"] {
        assert!(cookie[0].contains(attr), "{attr} missing in {}", cookie[0]);
    }
    assert!(!cookie[0].contains("Secure"), "over http: {}", cookie[0]);

    let res = app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(app.token_count().await, 1);
    // It is not used up: more changes within the time are fine.
    let res = app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(app.token_count().await, 2);
    // Another sensitive change takes the same grant.
    let res = app.post("/settings/security/2fa/recovery", &[]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains(r#"id="recovery-codes""#), "{}", res.body);
}

#[tokio::test]
async fn an_expired_grant_another_user_and_another_session_are_refused() {
    let mut app = App::set_up().await;
    app.core
        .with_conn(|c| db::users::insert_user(c, "bob", "!external", Role::Viewer))
        .await
        .unwrap();
    let version = app.session_version(1).await;

    // Six minutes old: expired.
    let old = app.forged_grant(1, version, now() - 360);
    app.cookies.insert("kd_reauth".into(), old);
    app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(app.token_count().await, 0, "expired");
    // Just inside the five minutes it still counts.
    let recent = app.forged_grant(1, version, now() - 240);
    app.cookies.insert("kd_reauth".into(), recent);
    app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(app.token_count().await, 1, "four minutes old");

    // A grant for another user.
    let theirs = app.forged_grant(2, version, now());
    app.cookies.insert("kd_reauth".into(), theirs);
    app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(app.token_count().await, 1, "another user's grant");

    // A grant for another session version of this user.
    let other = app.forged_grant(1, version + 1, now());
    app.cookies.insert("kd_reauth".into(), other);
    app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(app.token_count().await, 1, "another session version");

    // A value the kit did not sign.
    app.cookies
        .insert("kd_reauth".into(), format!("r1.1.{version}.{}", now()));
    app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(app.token_count().await, 1, "unsigned");
}

#[tokio::test]
async fn signing_out_other_sessions_ends_the_grant() {
    let mut app = App::set_up().await;
    app.get("/vault/reauth").await;
    assert!(app.cookies.contains_key("kd_reauth"));
    // The session version moves on (this browser stays signed in) ...
    let before = app.session_version(1).await;
    let res = app.post("/settings/security/sessions", &[]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_ne!(app.session_version(1).await, before);
    // ... and the grant is dropped with it.
    assert!(!res.set_cookies("kd_reauth").is_empty(), "grant cleared");
    assert!(!app.cookies.contains_key("kd_reauth"));
    app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(app.token_count().await, 0);
    // Even a grant kept for the old version is no good now.
    let version = app.session_version(1).await;
    let stale = app.forged_grant(1, before, now());
    app.cookies.insert("kd_reauth".into(), stale);
    app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(app.token_count().await, 0);
    let fresh = app.forged_grant(1, version, now());
    app.cookies.insert("kd_reauth".into(), fresh);
    app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(app.token_count().await, 1);
}

#[tokio::test]
async fn logging_out_or_in_clears_the_grant() {
    let mut app = App::set_up().await;
    app.get("/vault/reauth").await;
    let kept = app.cookies.get("kd_reauth").cloned().unwrap();

    let res = app.post("/logout", &[]).await;
    let cleared = res.set_cookies("kd_reauth");
    assert_eq!(cleared.len(), 1, "{:?}", res.headers);
    assert!(cleared[0].starts_with("kd_reauth=;"), "{}", cleared[0]);
    assert!(!app.cookies.contains_key("kd_reauth"));
    assert!(!app.cookies.contains_key("kd_session"));

    // A browser that kept the cookie anyway: the session of the next
    // sign-in (the same version) must still be asked to confirm.
    app.cookies.insert("kd_reauth".into(), kept);
    let res = app.get("/vault/login").await;
    assert!(
        !res.set_cookies("kd_reauth").is_empty(),
        "signing in drops an earlier grant"
    );
    app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(app.token_count().await, 0);
    assert!(
        app.get("/settings/tokens")
            .await
            .body
            .contains("/vault/reauth")
    );
}

// ---------------------------------------------------------------------------
// Passkeys
// ---------------------------------------------------------------------------

#[tokio::test]
async fn passkey_sign_in_stays_and_adding_one_needs_the_grant() {
    use vole_kit::auth::webauthn::{b64, soft::SoftAuthenticator, unb64};
    let mut app = App::set_up().await;
    app.core
        .with_conn(|c| db::settings::set(c, "base_url", "https://demo.test"))
        .await
        .unwrap();

    // The Security page offers them; adding needs the confirmation.
    let page = app.get("/settings/security").await;
    assert!(page.body.contains("data-passkey-add"), "{}", page.body);
    let res = app
        .post("/settings/security/passkeys/options", &[("name", "")])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    app.get("/vault/reauth").await;
    let res = app
        .post("/settings/security/passkeys/options", &[("name", "")])
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let options: Value = serde_json::from_str(&res.body).unwrap();
    let options = options["publicKey"].clone();
    let challenge = unb64(options["challenge"].as_str().unwrap(), "c").unwrap();
    let handle = unb64(options["user"]["id"].as_str().unwrap(), "h").unwrap();

    let mut auth = SoftAuthenticator::es256();
    let (client, att) = auth.register("demo.test", "https://demo.test", &challenge, &handle);
    let id = b64(&auth.credential_id);
    let body = json!({
        "name": "",
        "credential": {
            "id": id, "rawId": id, "type": "public-key",
            "response": {
                "clientDataJSON": b64(&client),
                "attestationObject": b64(&att),
                "transports": ["internal"],
            },
        },
    });
    let res = app.post_json("/settings/security/passkeys", &body).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Signed out, the passkey gives a session on its own.
    app.post("/logout", &[]).await;
    assert!(!app.cookies.contains_key("kd_session"));
    assert!(!app.cookies.contains_key("kd_reauth"));
    let res = app.get("/login").await;
    assert_eq!(res.location(), "/vault/login");
    let res = app.post("/login/passkey/options", &[]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let options: Value = serde_json::from_str(&res.body).unwrap();
    let challenge = unb64(options["publicKey"]["challenge"].as_str().unwrap(), "c").unwrap();
    let (client, data, sig) = auth.assert("demo.test", "https://demo.test", &challenge);
    let body = json!({
        "next": "/settings/tokens",
        "credential": {
            "id": id, "rawId": id, "type": "public-key",
            "response": {
                "clientDataJSON": b64(&client),
                "authenticatorData": b64(&data),
                "signature": b64(&sig),
                "userHandle": b64(&auth.user_handle),
            },
        },
    });
    let res = app.post_json("/login/passkey", &body).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, r#"{"redirect":"/settings/tokens"}"#);
    assert!(app.cookies.contains_key("kd_session"));
    assert_eq!(app.get("/settings/tokens").await.status, StatusCode::OK);
    // A session from a passkey is not a confirmation.
    assert!(!app.cookies.contains_key("kd_reauth"));
    app.post("/settings/tokens", App::NEW_TOKEN).await;
    assert_eq!(app.token_count().await, 0);
}
