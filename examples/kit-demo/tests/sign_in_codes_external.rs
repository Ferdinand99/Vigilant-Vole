//! One-time sign-in codes for a product that signs people in on its own
//! pages (`CredentialMode::External`): the kit's `/sign-in-code` leads to
//! the product's password page with `?code=1`, and the product redeems the
//! code with `auth::sign_in_codes`. This is its own test binary because the
//! kit's configuration is installed once per process.
//!
//! The "product" is a few routes in this file standing in for what a real
//! one does: create the first admin, sign people in, confirm the admin and
//! take a new credential for a code.

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
use tower::ServiceExt as _;
use vole_kit::auth::guard::{self, PeerAddr, login_jar, secure_cookies};
use vole_kit::auth::pages::{check_setup_token, insert_first_admin};
use vole_kit::auth::sign_in_codes::{self, Generic};
use vole_kit::auth::{Role, reauth};
use vole_kit::db;
use vole_kit::state::Core;

static N: AtomicUsize = AtomicUsize::new(0);
const CONFIRM: &str = "Confirm it’s you to make this change.";

// ---------------------------------------------------------------------------
// The product's own routes
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct Q {
    user: Option<String>,
    token: Option<String>,
}

async fn vault_setup(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    Query(q): Query<Q>,
) -> Response {
    check_setup_token(&core, &headers, peer, q.token.as_deref()).expect("setup is open in tests");
    let id = insert_first_admin(&core, "ada", "!external", "Kit Demo", "UTC")
        .await
        .unwrap()
        .expect("the first admin");
    sign_in(&core, jar, &headers, id).await
}

/// Signs `?user=` in without asking for anything (the product's sign-in,
/// after its own check).
async fn vault_as(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Query(q): Query<Q>,
) -> Response {
    let name = q.user.unwrap_or_default();
    let user = core
        .with_conn(move |c| db::users::get_user_by_username(c, &name))
        .await
        .unwrap()
        .expect("a user");
    sign_in(&core, jar, &headers, user.id).await
}

async fn sign_in(core: &Core, jar: SignedCookieJar, headers: &HeaderMap, id: i64) -> Response {
    let user = core
        .with_conn(move |c| db::users::get_user(c, id))
        .await
        .unwrap()
        .unwrap();
    let jar = login_jar(jar, user.id, user.session_version, secure_cookies(headers));
    (jar, Redirect::to("/")).into_response()
}

async fn vault_reauth(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
) -> Response {
    guard::resolve_session(&core, &jar)
        .await
        .unwrap()
        .expect("signed in");
    let jar = reauth::grant(jar, &core.session_key, secure_cookies(&headers));
    (jar, Redirect::to("/settings/users")).into_response()
}

#[derive(Deserialize)]
struct Take {
    username: String,
    code: String,
}

/// Its password page, on submit: redeems the code, stores its marker
/// credential and signs the user in.
async fn vault_password(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    axum::Form(take): axum::Form<Take>,
) -> Response {
    let ip = vole_kit::auth::session::client_ip(&headers, peer, false);
    match sign_in_codes::redeem_from(&core, &take.username, &take.code, ip)
        .await
        .unwrap()
    {
        Ok(id) => {
            assert!(
                sign_in_codes::set_password_hash(&core, id, "!external-new")
                    .await
                    .unwrap()
            );
            sign_in(&core, jar, &headers, id).await
        }
        Err(Generic) => (StatusCode::UNAUTHORIZED, "invalid").into_response(),
    }
}

async fn vault_form() -> Response {
    "the product's password page".into_response()
}

fn vault(core: &Core) -> Router {
    Router::new()
        .route("/vault/setup", get(vault_setup))
        .route("/vault/as", get(vault_as))
        .route("/vault/reauth", get(vault_reauth))
        .route("/vault/password", get(vault_form).post(vault_password))
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
}

struct Browser {
    cookies: BTreeMap<String, String>,
    peer: SocketAddr,
}

impl Browser {
    fn at(peer: &str) -> Self {
        Self {
            cookies: BTreeMap::new(),
            peer: peer.parse().unwrap(),
        }
    }

    fn signed_in(&self) -> bool {
        self.cookies.contains_key("kd_session")
    }
}

struct App {
    core: Core,
    router: Router,
    dir: PathBuf,
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl App {
    async fn set_up() -> (App, Browser) {
        let dir = std::env::temp_dir().join(format!(
            "kit-demo-codes-ext-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let demo = kit_demo::start_with(&dir, &kit_demo::KIT_CODES_EXTERNAL)
            .await
            .unwrap();
        let router = kit_demo::router(&demo).merge(vault(&demo.core));
        let app = App {
            core: demo.core.clone(),
            router,
            dir,
        };
        let mut admin = Browser::at("127.0.0.1:40000");
        let res = app.get(&mut admin, "/vault/setup").await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        assert!(admin.signed_in());
        (app, admin)
    }

    async fn send(&self, b: &mut Browser, mut req: Request<Body>) -> Res {
        let h = req.headers_mut();
        h.insert(header::HOST, HeaderValue::from_static("demo.test"));
        if !b.cookies.is_empty() {
            let cookie = b
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            h.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        }
        req.extensions_mut().insert(ConnectInfo(b.peer));
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
                b.cookies.remove(name);
            } else {
                b.cookies.insert(name.to_string(), value.to_string());
            }
        }
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        Res {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    async fn get(&self, b: &mut Browser, path: &str) -> Res {
        self.send(b, Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    async fn post(&self, b: &mut Browser, path: &str, form: &[(&str, &str)]) -> Res {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let req = Request::post(path)
            .header(header::ORIGIN, "http://demo.test")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        self.send(b, req).await
    }

    async fn add_user(&self, name: &str) -> i64 {
        let name = name.to_string();
        self.core
            .with_conn(move |c| {
                db::users::insert_user_full(c, &name, "!external", Role::Viewer, "")
            })
            .await
            .unwrap()
    }

    async fn code_for(&self, user_id: i64) -> String {
        let code = sign_in_codes::generate();
        let hash = sign_in_codes::hash(&code).to_vec();
        self.core
            .with_conn(move |c| db::sign_in_codes::insert(c, user_id, &hash, 1))
            .await
            .unwrap();
        code
    }

    async fn user(&self, name: &str) -> db::users::User {
        let name = name.to_string();
        self.core
            .with_conn(move |c| db::users::get_user_by_username(c, &name))
            .await
            .unwrap()
            .unwrap()
    }

    async fn codes_in_db(&self) -> i64 {
        self.core
            .with_conn(|c| c.query_row("SELECT COUNT(*) FROM sign_in_codes", [], |r| r.get(0)))
            .await
            .unwrap()
    }
}

fn shown_code(body: &str) -> Option<String> {
    let marker = "id=\"created-sign-in-code\">";
    let at = body.find(marker)? + marker.len();
    let end = body[at..].find('<')?;
    Some(body[at..at + end].to_string())
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_page_leads_to_the_products_password_page() {
    let (app, _admin) = App::set_up().await;
    let mut guest = Browser::at("10.0.0.1:1");
    let res = app.get(&mut guest, "/sign-in-code").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(res.location(), "/vault/password?code=1");
    assert!(guest.cookies.is_empty());

    // The kit takes no password itself.
    let res = app
        .post(
            &mut guest,
            "/sign-in-code",
            &[
                ("username", "ada"),
                ("code", "0000"),
                ("password", "Whatever-Pass-2026!"),
                ("confirm", "Whatever-Pass-2026!"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    assert!(!guest.signed_in());
    // And the sign-in page, which leads to the product's, links nothing.
    let res = app.get(&mut guest, "/login").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.location(), "/vault/login");
}

#[tokio::test]
async fn an_admin_needs_a_fresh_confirmation_and_it_is_used_up() {
    let (app, mut admin) = App::set_up().await;
    let bob = app.add_user("bob").await;
    let path = format!("/settings/users/{bob}/sign-in-code");

    // The sheet asks for the confirmation.
    let page = app.get(&mut admin, "/settings/users").await.body;
    assert!(page.contains("reset-code-"), "{page}");
    assert!(page.contains("Confirm it’s you"), "{page}");
    assert!(!page.contains("name=\"current_password\" type=\"password\" required"));

    let res = app.post(&mut admin, &path, &[]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(shown_code(&res.body).is_none());
    assert!(
        app.get(&mut admin, "/settings/users")
            .await
            .body
            .contains(CONFIRM)
    );
    assert_eq!(app.codes_in_db().await, 0);

    // A password in the form is no confirmation.
    let res = app
        .post(&mut admin, &path, &[("current_password", "anything")])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(app.codes_in_db().await, 0);

    app.get(&mut admin, "/vault/reauth").await;
    let res = app.post(&mut admin, &path, &[]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let code = shown_code(&res.body).expect("the code");
    assert_eq!(app.codes_in_db().await, 1);

    // The confirmation is used up: a second code needs another.
    let res = app.post(&mut admin, &path, &[]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.codes_in_db().await, 1);

    // The code is valid for the product.
    assert!(
        sign_in_codes::redeem(&app.core, "bob", &code)
            .await
            .unwrap()
            .is_ok()
    );

    // Not for yourself, not for someone else's account without being admin.
    app.get(&mut admin, "/vault/reauth").await;
    let ada = app.user("ada").await.id;
    let res = app
        .post(
            &mut admin,
            &format!("/settings/users/{ada}/sign-in-code"),
            &[],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(app.codes_in_db().await, 1, "only bob's, already used");
    let mut b = Browser::at("10.0.0.2:1");
    app.get(&mut b, "/vault/as?user=bob").await;
    app.get(&mut b, "/vault/reauth").await;
    let res = app
        .post(&mut b, &format!("/settings/users/{ada}/sign-in-code"), &[])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn redeeming_uses_the_code_up_and_signs_the_user_out() {
    let (app, _admin) = App::set_up().await;
    let bob = app.add_user("bob").await;
    let mut old = Browser::at("10.0.1.1:1");
    app.get(&mut old, "/vault/as?user=bob").await;
    assert_eq!(app.get(&mut old, "/").await.status, StatusCode::OK);
    app.core
        .with_conn(move |c| {
            db::users::set_pending_totp_secret(c, bob, "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP")?;
            db::users::enable_totp(c, bob, 0, &[])
        })
        .await
        .unwrap();
    let before = app.user("bob").await;
    let code = app.code_for(bob).await;

    let id = sign_in_codes::redeem(&app.core, "Bob", &code)
        .await
        .unwrap()
        .expect("the code works");
    assert_eq!(id, bob);
    let after = app.user("bob").await;
    assert_ne!(after.session_version, before.session_version, "signed out");
    assert!(after.totp_enabled, "two-factor stays");
    assert_eq!(
        after.password_hash, "!external",
        "the kit sets no credential"
    );
    let res = app.get(&mut old, "/").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(res.location().starts_with("/login"), "{}", res.location());

    // Once.
    assert_eq!(
        sign_in_codes::redeem(&app.core, "bob", &code)
            .await
            .unwrap(),
        Err(Generic)
    );
    assert_eq!(
        app.user("bob").await.session_version,
        after.session_version,
        "a refused code changes nothing"
    );

    // The product stores its credential.
    assert!(
        sign_in_codes::set_password_hash(&app.core, bob, "!marker")
            .await
            .unwrap()
    );
    assert_eq!(app.user("bob").await.password_hash, "!marker");
    assert!(
        !sign_in_codes::set_password_hash(&app.core, 999, "!marker")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn the_product_page_redeems_and_signs_in() {
    let (app, _admin) = App::set_up().await;
    let bob = app.add_user("bob").await;
    let code = app.code_for(bob).await;
    let mut b = Browser::at("10.0.2.1:1");
    let res = app
        .post(
            &mut b,
            "/vault/password",
            &[("username", "bob"), ("code", &code.to_lowercase())],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(b.signed_in());
    assert_eq!(app.get(&mut b, "/").await.status, StatusCode::OK);
    assert_eq!(app.user("bob").await.password_hash, "!external-new");
    let mut again = Browser::at("10.0.2.2:1");
    let res = app
        .post(
            &mut again,
            "/vault/password",
            &[("username", "bob"), ("code", &code)],
        )
        .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(!again.signed_in());
}

#[tokio::test]
async fn every_refusal_is_the_same_generic_error() {
    let (app, _admin) = App::set_up().await;
    let expired = app.add_user("expired").await;
    let revoked = app.add_user("revoked").await;
    let disabled = app.add_user("disabled").await;
    let thief = app.add_user("thief").await;
    let victim = app.add_user("victim").await;

    let c_expired = app.code_for(expired).await;
    app.core
        .with_conn(move |c| {
            c.execute(
                "UPDATE sign_in_codes SET expires_at = datetime('now', '-1 second')",
                [],
            )
        })
        .await
        .unwrap();
    let c_revoked = app.code_for(revoked).await;
    let _newer = app.code_for(revoked).await;
    let c_disabled = app.code_for(disabled).await;
    app.core
        .with_conn(move |c| c.execute("UPDATE users SET active = 0 WHERE id = ?1", [disabled]))
        .await
        .unwrap();
    let c_victim = app.code_for(victim).await;
    let _ = thief;

    for (user, code) in [
        ("expired", c_expired.as_str()),
        ("revoked", c_revoked.as_str()),
        ("disabled", c_disabled.as_str()),
        ("thief", c_victim.as_str()),
        ("no-such-user", c_victim.as_str()),
        ("victim", "garbage"),
        ("victim", ""),
    ] {
        assert_eq!(
            sign_in_codes::redeem(&app.core, user, code).await.unwrap(),
            Err(Generic),
            "{user}"
        );
    }
    // Nothing was spent by the others' attempts.
    assert_eq!(
        sign_in_codes::redeem(&app.core, "victim", &c_victim)
            .await
            .unwrap(),
        Ok(victim)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_many_racing_redeems_exactly_one_wins() {
    let (app, _admin) = App::set_up().await;
    let bob = app.add_user("bob").await;
    let code = app.code_for(bob).await;
    let before = app.user("bob").await.session_version;

    let tries = (0..5).map(|_| {
        let (core, code) = (app.core.clone(), code.clone());
        tokio::spawn(async move { sign_in_codes::redeem(&core, "bob", &code).await.unwrap() })
    });
    let results: Vec<_> = futures_util::future::join_all(tries)
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "{results:?}");
    assert!(
        results
            .iter()
            .filter(|r| r.is_err())
            .all(|r| *r == Err(Generic))
    );
    assert_eq!(
        app.user("bob").await.session_version,
        before + 1,
        "bumped once, not once per try"
    );
}

#[tokio::test]
async fn redeem_is_throttled_per_account_and_per_address() {
    let (app, _admin) = App::set_up().await;
    let bob = app.add_user("bob").await;
    let good = app.code_for(bob).await;
    let wrong = sign_in_codes::generate();

    for _ in 0..8 {
        assert_eq!(
            sign_in_codes::redeem(&app.core, "bob", &wrong)
                .await
                .unwrap(),
            Err(Generic)
        );
    }
    // The account waits: even the right code gets the same refusal.
    assert_eq!(
        sign_in_codes::redeem(&app.core, "bob", &good)
            .await
            .unwrap(),
        Err(Generic)
    );
    assert_eq!(app.codes_in_db().await, 1);
    let still: Option<String> = app
        .core
        .with_conn(|c| {
            use rusqlite::OptionalExtension as _;
            c.query_row("SELECT used_at FROM sign_in_codes", [], |r| r.get(0))
                .optional()
                .map(Option::flatten)
        })
        .await
        .unwrap();
    assert!(still.is_none(), "the throttled try did not use the code");

    // Per address.
    let ip: std::net::IpAddr = "203.0.113.9".parse().unwrap();
    for i in 0..30 {
        let r = sign_in_codes::redeem_from(&app.core, &format!("guess{i}"), &wrong, Some(ip))
            .await
            .unwrap();
        assert_eq!(r, Err(Generic));
    }
    // A fresh account from the throttled address is refused even with a good code.
    let carol = app.add_user("carol").await;
    let c = app.code_for(carol).await;
    assert_eq!(
        sign_in_codes::redeem_from(&app.core, "carol", &c, Some(ip))
            .await
            .unwrap(),
        Err(Generic)
    );
    // From another address it works.
    let other: std::net::IpAddr = "203.0.113.10".parse().unwrap();
    assert_eq!(
        sign_in_codes::redeem_from(&app.core, "carol", &c, Some(other))
            .await
            .unwrap(),
        Ok(carol)
    );
}
