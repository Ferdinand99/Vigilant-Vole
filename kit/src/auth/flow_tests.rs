//! The auth pages and guards driven in-process, the way a product mounts
//! them: `public_router` and `router` (behind `require_auth`) with an
//! exempt `/api/v1` route, all under `csrf_guard`.
//!
//! Tests that read or set the process-wide "public URL is https" flag, and
//! the global setup token, take turns ([`SERIAL`]).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::request::Builder;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::routing::get;
use axum_extra::extract::cookie::{Key, SignedCookieJar};
use base64::Engine as _;
use tower::ServiceExt as _;

use crate::auth::{Role, TokenScope, guard, pages, session, tokens, totp};
use crate::db::{self, passkeys as db_passkeys, users};
use crate::state::Core;

const PASSWORD: &str = "Kit-Owner-Pass-2026!";
const HOST: &str = "kit.test";

static SERIAL: LazyLock<Arc<tokio::sync::Mutex<()>>> = LazyLock::new(Default::default);
static N: AtomicUsize = AtomicUsize::new(0);

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

    fn set_cookie(&self, name: &str) -> Option<&str> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(&format!("{name}=")))
    }
}

/// One browser: its cookies.
#[derive(Default)]
struct Browser {
    cookies: BTreeMap<String, String>,
}

struct App {
    core: Core,
    router: Router,
    dir: PathBuf,
    _serial: tokio::sync::OwnedMutexGuard<()>,
}

impl Drop for App {
    fn drop(&mut self) {
        session::note_base_url(None);
        session::set_setup_open_for_tests(None);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn router(core: &Core) -> Router {
    let protected = pages::router::<Core>()
        .route(
            "/api/v1/ping",
            get(|| async { "pong" })
                .post(|| async { "pong" })
                .delete(|| async { "pong" }),
        )
        .route_layer(from_fn_with_state(core.clone(), guard::require_auth));
    Router::new()
        .merge(pages::public_router::<Core>())
        .merge(protected)
        .layer(from_fn_with_state(core.clone(), guard::csrf_guard))
        .layer(axum::middleware::from_fn(crate::http::error::negotiate))
        .with_state(core.clone())
}

impl App {
    /// A new instance with no users; `/setup` is open unless a test says otherwise.
    async fn fresh() -> App {
        let serial = SERIAL.clone().lock_owned().await;
        session::set_setup_open_for_tests(Some(true));
        session::note_base_url(None);
        let dir = std::env::temp_dir().join(format!(
            "vole-kit-flow-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = db::build_pool(&dir.join("kit.db"));
        db::migrate::run_migrations(&pool, &dir.join("backups")).await;
        let core = Core::new(
            pool,
            Key::generate(),
            reqwest::Client::new(),
            &crate::testing::CONFIG,
        );
        App {
            router: router(&core),
            core,
            dir,
            _serial: serial,
        }
    }

    /// After setup: the admin `ada` (user 1), signed in.
    async fn set_up() -> (App, Browser) {
        let app = App::fresh().await;
        let mut b = Browser::default();
        let res = app.setup(&mut b, &[]).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        (app, b)
    }

    async fn setup(&self, b: &mut Browser, extra: &[(&str, &str)]) -> Res {
        let mut form = vec![
            ("username", "ada"),
            ("password", PASSWORD),
            ("confirm", PASSWORD),
            ("site_name", ""),
            ("timezone", "UTC"),
        ];
        form.extend_from_slice(extra);
        self.post(b, "/setup", &form).await
    }

    async fn send(&self, b: &mut Browser, mut req: Request<Body>) -> Res {
        let h = req.headers_mut();
        h.insert(header::HOST, HeaderValue::from_static(HOST));
        if !b.cookies.is_empty() {
            let cookie = b
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
        for v in res.headers().get_all(header::SET_COOKIE) {
            let v = v.to_str().unwrap();
            if let Some((k, val)) = v.split(';').next().unwrap().split_once('=') {
                if val.is_empty() || v.contains("Max-Age=0") {
                    b.cookies.remove(k);
                } else {
                    b.cookies.insert(k.into(), val.into());
                }
            }
        }
        let (status, headers) = (res.status(), res.headers().clone());
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

    fn form_request(path: &str) -> Builder {
        Request::post(path)
            .header(header::ORIGIN, format!("http://{HOST}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
    }

    fn encode(form: &[(&str, &str)]) -> Body {
        Body::from(
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(form)
                .finish(),
        )
    }

    async fn post(&self, b: &mut Browser, path: &str, form: &[(&str, &str)]) -> Res {
        let req = Self::form_request(path).body(Self::encode(form)).unwrap();
        self.send(b, req).await
    }

    /// A browser signed in as `name` with `password`.
    async fn sign_in(&self, name: &str, password: &str) -> Browser {
        let mut b = Browser::default();
        let res = self
            .post(
                &mut b,
                "/login",
                &[("username", name), ("password", password)],
            )
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        b
    }

    async fn signed_in(&self, b: &mut Browser) -> bool {
        let res = self.get(b, "/settings/security").await;
        match res.status {
            StatusCode::OK => true,
            StatusCode::SEE_OTHER => {
                assert!(res.location().starts_with("/login"), "{}", res.location());
                false
            }
            other => panic!("unexpected {other}: {}", res.body),
        }
    }

    /// A write token of user 1.
    async fn token(&self) -> String {
        let token = tokens::generate();
        let (hash, prefix) = (tokens::hash(&token), tokens::display_prefix(&token));
        self.core
            .with_conn(move |c| {
                db::api_tokens::insert(c, 1, "test", &prefix, &hash, TokenScope::Write, None)
            })
            .await
            .unwrap();
        token
    }

    async fn user(&self, name: &'static str, password: &str) -> i64 {
        let hash = crate::auth::hash_password(password);
        self.core
            .with_conn(move |c| users::insert_user(c, name, &hash, Role::Viewer))
            .await
            .unwrap()
    }

    /// The message of the flash a response sets.
    fn flash(&self, res: &Res) -> Option<String> {
        let pair = res.set_cookie("vv_flash")?.split(';').next()?.to_string();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(&pair).ok()?);
        let jar = SignedCookieJar::from_headers(&headers, self.core.session_key.clone());
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(jar.get("vv_flash")?.value())
            .ok()?;
        let json: serde_json::Value = serde_json::from_slice(&raw).ok()?;
        json["m"].as_str().map(str::to_string)
    }
}

fn now() -> u64 {
    u64::try_from(session::now_unix()).unwrap()
}

// ---------------------------------------------------------------------------
// VK-01
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cookie_writes_to_exempt_paths_need_same_origin_proof() {
    let (app, mut ada) = App::set_up().await;
    let token = app.token().await;
    let ping = |method: &str| {
        Request::builder()
            .method(method)
            .uri("/api/v1/ping")
            .header(header::ORIGIN, "http://evil.test")
    };

    // The browser's cookie, posted from another origin: refused.
    let res = app
        .send(&mut ada, ping("POST").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert!(res.body.contains("Cross-site"), "{}", res.body);
    // Nothing to prove where it comes from: refused too.
    let bare = Request::post("/api/v1/ping").body(Body::empty()).unwrap();
    assert_eq!(app.send(&mut ada, bare).await.status, StatusCode::FORBIDDEN);

    // Our own origin passes, and so does htmx.
    let own = Request::post("/api/v1/ping")
        .header(header::ORIGIN, format!("http://{HOST}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(&mut ada, own).await.status, StatusCode::OK);
    let htmx = Request::post("/api/v1/ping")
        .header("hx-request", "true")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(&mut ada, htmx).await.status, StatusCode::OK);
    // Reads need no proof; a DELETE can't be sent cross-site without a preflight.
    assert_eq!(
        app.get(&mut ada, "/api/v1/ping").await.status,
        StatusCode::OK
    );
    let delete = ping("DELETE").body(Body::empty()).unwrap();
    assert_eq!(app.send(&mut ada, delete).await.status, StatusCode::OK);

    // A token is not a cookie: no origin proof needed, and a cookie sent
    // along is ignored.
    let by_token = ping("POST")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let mut nobody = Browser::default();
    assert_eq!(app.send(&mut nobody, by_token).await.status, StatusCode::OK);
    let both = ping("POST")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(&mut ada, both).await.status, StatusCode::OK);
    // And without either, it is still 401.
    let anon = ping("POST")
        .header(header::ACCEPT, "application/json")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.send(&mut nobody, anon).await.status,
        StatusCode::UNAUTHORIZED
    );
}

// ---------------------------------------------------------------------------
// VK-04
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_reused_user_id_does_not_inherit_the_old_cookie() {
    let (app, _ada) = App::set_up().await;
    let bob_id = app.user("bob", PASSWORD).await;
    let mut bob = app.sign_in("bob", PASSWORD).await;
    assert!(app.signed_in(&mut bob).await);

    app.core
        .with_conn(move |c| users::delete_user(c, bob_id))
        .await
        .unwrap()
        .unwrap();
    // SQLite hands the freed id to the next user.
    let carol_id = app.user("carol", PASSWORD).await;
    assert_eq!(carol_id, bob_id);
    assert!(
        !app.signed_in(&mut bob).await,
        "bob's cookie is carol's now"
    );
    // Carol herself is fine.
    let mut carol = app.sign_in("carol", PASSWORD).await;
    assert!(app.signed_in(&mut carol).await);
}

// ---------------------------------------------------------------------------
// VK-05
// ---------------------------------------------------------------------------

#[tokio::test]
async fn setup_needs_the_made_up_token_unless_opened() {
    let app = App::fresh().await;
    session::set_setup_open_for_tests(Some(false));
    let token = session::required_setup_token().expect("a token is required");
    assert_eq!(token.len(), 32);
    let mut b = Browser::default();

    // No token: a locked page with a field for it, and nothing is created.
    let res = app.get(&mut b, "/setup").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert!(res.body.contains(r#"name="token""#) && !res.body.contains(r#"name="password""#));
    let res = app.setup(&mut b, &[]).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = app.setup(&mut b, &[("token", "not-the-token")]).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let count = |app: &App| {
        let core = app.core.clone();
        async move { core.with_conn(|c| users::count_users(c)).await.unwrap() }
    };
    assert_eq!(count(&app).await, 0);
    // Every other page sends a visitor there.
    let res = app.get(&mut b, "/settings/security").await;
    assert_eq!(
        (res.status, res.location()),
        (StatusCode::SEE_OTHER, "/setup")
    );

    // The token in the query opens the form, which carries it along.
    let res = app.get(&mut b, &format!("/setup?token={token}")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body
            .contains(&format!(r#"name="token" value="{token}""#))
    );
    // The token typed into the locked page's field (the form field, not the query).
    let res = app.post(&mut b, "/setup", &[("token", &token)]).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains(r#"name="password""#));
    // And with the whole form.
    let res = app.setup(&mut b, &[("token", &token)]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(count(&app).await, 1);

    // The opt-out keeps the old behaviour.
    drop(app);
    let app = App::fresh().await;
    let mut b = Browser::default();
    assert_eq!(app.setup(&mut b, &[]).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn wrong_setup_tokens_are_throttled_per_address() {
    let app = App::fresh().await;
    session::set_setup_open_for_tests(Some(false));
    let token = session::required_setup_token().unwrap();
    let mut b = Browser::default();
    let mut statuses = Vec::new();
    for i in 0..30 {
        let guess = format!("guess-{i}");
        statuses.push(
            app.post(&mut b, "/setup", &[("token", &guess)])
                .await
                .status,
        );
    }
    assert_eq!(statuses[0], StatusCode::FORBIDDEN);
    assert_eq!(statuses[29], StatusCode::TOO_MANY_REQUESTS, "{statuses:?}");
    // While backing off, even the right token waits.
    let res = app.post(&mut b, "/setup", &[("token", &token)]).await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);
    // The password sign-in throttle (`ip:`) is a different key.
    assert!(
        app.core
            .limits
            .ip
            .retry_after("ip:127.0.0.1", std::time::Instant::now())
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// VK-06
// ---------------------------------------------------------------------------

#[tokio::test]
async fn changing_two_factor_or_passkeys_signs_other_devices_out() {
    let (app, mut here) = App::set_up().await;
    let mut phone = app.sign_in("ada", PASSWORD).await;
    assert!(app.signed_in(&mut here).await && app.signed_in(&mut phone).await);

    // Turn two-factor on from here.
    let res = app
        .post(&mut here, "/settings/security/2fa/setup", &[])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let secret = app
        .core
        .with_conn(|c| users::get_totp(c, 1))
        .await
        .unwrap()
        .unwrap()
        .secret
        .unwrap();
    let t = totp::build(&secret, "ada", "Kit").unwrap();
    let code = totp::code_at(&t, now());
    let res = app
        .post(
            &mut here,
            "/settings/security/2fa/enable",
            &[("code", &code)],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(
        res.set_cookie("vv_session").is_some(),
        "this device is re-issued"
    );
    assert!(
        app.signed_in(&mut here).await,
        "this device stays signed in"
    );
    assert!(!app.signed_in(&mut phone).await, "the other one does not");

    // Sign in on the tablet (password and code), then turn it off from here.
    let mut tablet = Browser::default();
    let res = app
        .post(
            &mut tablet,
            "/login",
            &[("username", "ada"), ("password", PASSWORD)],
        )
        .await;
    assert_eq!(res.location(), "/login/2fa");
    app.core
        .with_conn(|c| c.execute("UPDATE users SET totp_last_step = NULL", []))
        .await
        .unwrap();
    let code = totp::code_at(&t, now());
    let res = app
        .post(&mut tablet, "/login/2fa", &[("code", &code)])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(app.signed_in(&mut tablet).await);
    app.core
        .with_conn(|c| c.execute("UPDATE users SET totp_last_step = NULL", []))
        .await
        .unwrap();
    let res = app
        .post(
            &mut here,
            "/settings/security/2fa/disable",
            &[("password", PASSWORD), ("code", &totp::code_at(&t, now()))],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(app.signed_in(&mut here).await);
    assert!(!app.signed_in(&mut tablet).await);

    // Deleting a passkey does the same.
    let mut laptop = app.sign_in("ada", PASSWORD).await;
    let id = app
        .core
        .with_conn(|c| {
            db_passkeys::insert(
                c,
                &db_passkeys::NewPasskey {
                    user_id: 1,
                    credential_id: b"credential",
                    public_key: b"key",
                    alg: -7,
                    user_handle: b"handle",
                    name: "Old key",
                    sign_count: 0,
                    transports: "",
                    backup_eligible: false,
                    backed_up: false,
                },
            )
        })
        .await
        .unwrap()
        .expect("inserted");
    let res = app
        .post(
            &mut here,
            &format!("/settings/security/passkeys/{id}/delete"),
            &[],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(app.signed_in(&mut here).await);
    assert!(!app.signed_in(&mut laptop).await);
}

// ---------------------------------------------------------------------------
// VK-07
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cookies_are_secure_when_the_public_url_is_https() {
    let (app, _ada) = App::set_up().await;
    let https = |url: &'static str| {
        let core = app.core.clone();
        async move {
            core.with_conn(move |c| db::settings::set(c, db::settings::BASE_URL, url))
                .await
                .unwrap();
            core.base_url().await;
        }
    };

    // Plain http reaches us here (no proxy header, trust off): not Secure.
    https("http://kit.test").await;
    let mut b = Browser::default();
    let res = app
        .post(
            &mut b,
            "/login",
            &[("username", "ada"), ("password", PASSWORD)],
        )
        .await;
    assert!(!res.set_cookie("vv_session").unwrap().contains("Secure"));

    https("https://kit.test").await;
    assert!(session::base_url_is_https());
    let mut b = Browser::default();
    let res = app
        .post(
            &mut b,
            "/login",
            &[("username", "ada"), ("password", PASSWORD)],
        )
        .await;
    let cookie = res.set_cookie("vv_session").unwrap();
    assert!(
        cookie.contains("Secure") && cookie.contains("HttpOnly"),
        "{cookie}"
    );
    // The flash message too.
    let res = app
        .post(
            &mut b,
            "/settings/security/profile",
            &[("display_name", "Ada")],
        )
        .await;
    assert!(res.set_cookie("vv_flash").unwrap().contains("Secure"));

    https("http://kit.test").await;
    assert!(!session::base_url_is_https());
}

// ---------------------------------------------------------------------------
// VK-09
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_password_guesses_are_counted_before_they_are_checked() {
    let (app, ada) = App::set_up().await;
    let cookie = ada
        .cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    let requests = (0..30).map(|i| {
        let form = [
            ("current_password", format!("wrong guess {i}")),
            ("new_password", "Another-Pass-2026!".to_string()),
            ("confirm_password", "Another-Pass-2026!".to_string()),
        ];
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form.iter().map(|(k, v)| (*k, v.as_str())))
            .finish();
        let mut req = Request::post("/settings/security/password")
            .header(header::HOST, HOST)
            .header(header::ORIGIN, format!("http://{HOST}"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
        ));
        app.router.clone().oneshot(req)
    });
    let results = futures_util::future::join_all(requests).await;
    let wrong = crate::i18n::t("sec.wrong_password");
    let mut checked = 0;
    for res in results {
        let res = res.unwrap();
        let res = Res {
            status: res.status(),
            headers: res.headers().clone(),
            body: String::new(),
        };
        assert_eq!(res.status, StatusCode::SEE_OTHER);
        if app.flash(&res).as_deref() == Some(wrong) {
            checked += 1;
        }
    }
    // The free guesses plus the one that starts the backoff; the other 24 were turned away.
    assert!(
        (1..=6).contains(&checked),
        "{checked} of 30 parallel guesses were checked"
    );
    assert!(
        app.core
            .limits
            .account
            .retry_after("reauth:1", std::time::Instant::now())
            .is_some()
    );
}

#[tokio::test]
async fn the_right_password_clears_the_reauthentication_count() {
    let (app, mut ada) = App::set_up().await;
    for _ in 0..4 {
        let res = app
            .post(
                &mut ada,
                "/settings/security/2fa/recovery",
                &[("password", "nope")],
            )
            .await;
        assert_eq!(
            app.flash(&res).as_deref(),
            Some(crate::i18n::t("sec.wrong_password"))
        );
    }
    // A right one (the page says 2FA is off) is not refused, and forgets the failures.
    let res = app
        .post(
            &mut ada,
            "/settings/security/2fa/recovery",
            &[("password", PASSWORD)],
        )
        .await;
    assert_eq!(
        app.flash(&res).as_deref(),
        Some(crate::i18n::t("sec.tfa_turn_on_first"))
    );
    assert!(
        app.core
            .limits
            .account
            .retry_after("reauth:1", std::time::Instant::now())
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// VK-03
// ---------------------------------------------------------------------------

#[tokio::test]
async fn passkey_sign_in_and_password_sign_in_throttle_separately() {
    let (app, _ada) = App::set_up().await;
    app.core
        .with_conn(|c| db::settings::set(c, db::settings::BASE_URL, "http://localhost:3001"))
        .await
        .unwrap();
    let mut b = Browser::default();
    let wrong = [("username", "ada"), ("password", "nope")];
    let mut last = StatusCode::OK;
    for _ in 0..30 {
        last = app.post(&mut b, "/login", &wrong).await.status;
    }
    assert_eq!(
        last,
        StatusCode::TOO_MANY_REQUESTS,
        "the password bucket is full"
    );

    // Passkeys still start, though this address can't try passwords any more.
    let res = app.post(&mut b, "/login/passkey/options", &[]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    // A passkey sign-in that is not one.
    let garbage = |app: &App| {
        let mut req = Request::post("/login/passkey")
            .header(header::HOST, HOST)
            .header(header::ORIGIN, format!("http://{HOST}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
        ));
        let router = app.router.clone();
        async move { router.oneshot(req).await.unwrap().status() }
    };
    assert_eq!(garbage(&app).await, StatusCode::UNAUTHORIZED);
    drop(app);

    // The other way round: many failed passkey sign-ins, passwords still work.
    let (app, _ada) = App::set_up().await;
    app.core
        .with_conn(|c| db::settings::set(c, db::settings::BASE_URL, "http://localhost:3001"))
        .await
        .unwrap();
    for _ in 0..40 {
        assert_eq!(garbage(&app).await, StatusCode::UNAUTHORIZED);
    }
    let mut b = Browser::default();
    let res = app
        .post(
            &mut b,
            "/login",
            &[("username", "ada"), ("password", PASSWORD)],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
}
