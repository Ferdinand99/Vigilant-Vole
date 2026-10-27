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
    // Stored sealed.
    assert!(crate::crypto::is_sealed(&secret));
    let secret = totp::open_secret(&app.core.session_key, &secret).unwrap();
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
            &[("current_password", PASSWORD)],
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

// ---------------------------------------------------------------------------
// VK-10
// ---------------------------------------------------------------------------

const BOB_PASSWORD: &str = "Bob-Own-Pass-2026-Zq!";
const NEW_PASSWORD: &str = "Second-Pass-2026-Qz!";

impl App {
    async fn tokens_of(&self, user: i64) -> Vec<db::api_tokens::ApiToken> {
        self.core
            .with_conn(move |c| db::api_tokens::list_for_user(c, user))
            .await
            .unwrap()
    }

    async fn give_token(&self, user: i64) {
        let token = tokens::generate();
        let (hash, prefix) = (tokens::hash(&token), tokens::display_prefix(&token));
        self.core
            .with_conn(move |c| {
                db::api_tokens::insert(c, user, "script", &prefix, &hash, TokenScope::Read, None)
            })
            .await
            .unwrap();
    }

    async fn user_row(&self, id: i64) -> users::User {
        self.core
            .with_conn(move |c| users::get_user(c, id))
            .await
            .unwrap()
            .expect("user exists")
    }

    /// Turns two-factor on for `id` the way an older kit stored it: the
    /// secret in plaintext.
    async fn plaintext_two_factor(&self, id: i64, secret: &'static str) {
        self.core
            .with_conn(move |c| {
                users::set_pending_totp_secret(c, id, secret)?;
                users::enable_totp(c, id, 0, &["hash".to_string()])
            })
            .await
            .unwrap();
    }

    /// A refused change: back to the page with this flash.
    fn assert_flash(&self, res: &Res, key: &'static str) {
        self.assert_flash_text(res, crate::i18n::t(key));
    }

    fn assert_flash_text(&self, res: &Res, text: &str) {
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        assert_eq!(self.flash(res).as_deref(), Some(text));
    }
}

async fn new_token(app: &App, b: &mut Browser, scope: &str, expires: &str, password: &str) -> Res {
    let mut form = vec![("name", "ci"), ("scope", scope), ("expires", expires)];
    if !password.is_empty() {
        form.push(("current_password", password));
    }
    app.post(b, "/settings/tokens", &form).await
}

#[tokio::test]
async fn write_tokens_and_tokens_without_expiry_need_the_password() {
    let (app, mut ada) = App::set_up().await;

    // The sheet offers 90 days unless you choose otherwise.
    let page = app.get(&mut ada, "/settings/tokens").await;
    assert!(
        page.body.contains(r#"<option value="90" selected>"#),
        "{}",
        page.body
    );

    // A read-only token that expires needs nothing.
    let res = new_token(&app, &mut ada, "read", "30", "").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(app.tokens_of(1).await.len(), 1);

    // Write access, or no expiry: the password is asked for first.
    for (scope, expires) in [("write", "30"), ("read", "never"), ("write", "never")] {
        let res = new_token(&app, &mut ada, scope, expires, "").await;
        app.assert_flash(&res, "sec.reauth_needed");
        let res = new_token(&app, &mut ada, scope, expires, "not the password").await;
        app.assert_flash(&res, "sec.wrong_password");
    }
    assert_eq!(app.tokens_of(1).await.len(), 1, "nothing was created");

    // With the password they are created, and nothing else changes.
    for (scope, expires) in [("write", "never"), ("read", "never"), ("write", "90")] {
        let res = new_token(&app, &mut ada, scope, expires, PASSWORD).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    let all = app.tokens_of(1).await;
    assert_eq!(all.len(), 4);
    assert_eq!(
        all.iter().filter(|t| t.expires_at.is_none()).count(),
        2,
        "the two that never expire"
    );
    // The right password forgot the failures.
    assert!(
        app.core
            .limits
            .account
            .retry_after("reauth:1", std::time::Instant::now())
            .is_none()
    );
}

#[tokio::test]
async fn token_reauthentication_counts_parallel_guesses_before_checking_them() {
    let (app, ada) = App::set_up().await;
    let cookie = ada
        .cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    let requests = (0..30).map(|i| {
        let guess = format!("wrong guess {i}");
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("name", "ci"),
                ("scope", "write"),
                ("expires", "never"),
                ("current_password", guess.as_str()),
            ])
            .finish();
        let mut req = Request::post("/settings/tokens")
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
    assert!(
        (1..=6).contains(&checked),
        "{checked} of 30 parallel guesses were checked"
    );
    assert!(app.tokens_of(1).await.is_empty());
}

#[tokio::test]
async fn adding_or_promoting_an_administrator_needs_the_password() {
    let (app, mut ada) = App::set_up().await;
    let add = |name: &'static str, role: &'static str, password: &'static str| {
        let mut form = vec![
            ("username", name),
            ("display_name", ""),
            ("role", role),
            ("password", "Temp-Pass-2026-Zq!"),
        ];
        if !password.is_empty() {
            form.push(("current_password", password));
        }
        form
    };

    // An administrator: refused without the password, and with a wrong one.
    let res = app
        .post(&mut ada, "/settings/users", &add("bob", "admin", ""))
        .await;
    app.assert_flash(&res, "sec.reauth_needed");
    let res = app
        .post(&mut ada, "/settings/users", &add("bob", "admin", "nope"))
        .await;
    app.assert_flash(&res, "sec.wrong_password");
    let taken = app
        .core
        .with_conn(|c| users::username_taken(c, "bob"))
        .await
        .unwrap();
    assert!(!taken, "no user was created");

    // A viewer needs no confirmation.
    let res = app
        .post(&mut ada, "/settings/users", &add("bob", "viewer", ""))
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(app.user_row(2).await.role, Role::Viewer);

    // Making that user an administrator is the same step.
    let promote = |password: &'static str| {
        let mut form = vec![("display_name", ""), ("role", "admin")];
        if !password.is_empty() {
            form.push(("current_password", password));
        }
        form
    };
    let res = app.post(&mut ada, "/settings/users/2", &promote("")).await;
    app.assert_flash(&res, "sec.reauth_needed");
    assert_eq!(app.user_row(2).await.role, Role::Viewer);
    let res = app
        .post(&mut ada, "/settings/users/2", &promote(PASSWORD))
        .await;
    app.assert_flash(&res, "users.saved");
    assert_eq!(app.user_row(2).await.role, Role::Admin);

    // Editing an administrator, or demoting one, asks for nothing.
    for role in ["admin", "editor"] {
        let res = app
            .post(
                &mut ada,
                "/settings/users/2",
                &[("display_name", "Bob"), ("role", role)],
            )
            .await;
        app.assert_flash(&res, "users.saved");
    }

    // With the password, an administrator is created.
    let res = app
        .post(&mut ada, "/settings/users", &add("cy", "admin", PASSWORD))
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(app.user_row(3).await.role, Role::Admin);
}

#[tokio::test]
async fn admin_resets_need_the_admin_password_and_can_revoke_tokens() {
    let (app, mut ada) = App::set_up().await;
    let bob = app.user("bob", BOB_PASSWORD).await;
    app.give_token(bob).await;
    app.plaintext_two_factor(bob, "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP")
        .await;
    let reset = |password: &'static str, revoke: bool| {
        let mut form = vec![("password", NEW_PASSWORD)];
        if !password.is_empty() {
            form.push(("current_password", password));
        }
        if revoke {
            form.push(("revoke_tokens", "true"));
        }
        form
    };
    let path = format!("/settings/users/{bob}/password");

    // No password, or a wrong one: nothing changes.
    let res = app.post(&mut ada, &path, &reset("", true)).await;
    app.assert_flash(&res, "sec.reauth_needed");
    let res = app.post(&mut ada, &path, &reset("nope", true)).await;
    app.assert_flash(&res, "sec.wrong_password");
    let hash = app.user_row(bob).await.password_hash;
    assert!(crate::auth::verify_password(&hash, BOB_PASSWORD));
    assert_eq!(app.tokens_of(bob).await.len(), 1);

    // Confirmed, without the box: the tokens stay.
    let res = app.post(&mut ada, &path, &reset(PASSWORD, false)).await;
    app.assert_flash_text(
        &res,
        &crate::i18n::tf("users.password_reset", &[("name", "bob")]),
    );
    let hash = app.user_row(bob).await.password_hash;
    assert!(crate::auth::verify_password(&hash, NEW_PASSWORD));
    assert_eq!(app.tokens_of(bob).await.len(), 1);
    // Confirmed, with it: they are revoked, and only bob's.
    app.give_token(1).await;
    let res = app.post(&mut ada, &path, &reset(PASSWORD, true)).await;
    app.assert_flash_text(
        &res,
        &crate::i18n::tf("users.password_reset", &[("name", "bob")]),
    );
    assert!(app.tokens_of(bob).await.is_empty());
    assert_eq!(app.tokens_of(1).await.len(), 1);

    // Two-factor: the same confirmation.
    let tfa = format!("/settings/users/{bob}/2fa/reset");
    let res = app.post(&mut ada, &tfa, &[]).await;
    app.assert_flash(&res, "sec.reauth_needed");
    let res = app
        .post(&mut ada, &tfa, &[("current_password", "nope")])
        .await;
    app.assert_flash(&res, "sec.wrong_password");
    assert!(app.user_row(bob).await.totp_enabled);
    let res = app
        .post(&mut ada, &tfa, &[("current_password", PASSWORD)])
        .await;
    app.assert_flash_text(
        &res,
        &crate::i18n::tf("users.tfa_reset", &[("name", "bob")]),
    );
    assert!(!app.user_row(bob).await.totp_enabled);
}

#[tokio::test]
async fn deleting_a_passkey_needs_the_password() {
    let (app, mut here) = App::set_up().await;
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
    let path = format!("/settings/security/passkeys/{id}/delete");
    let count = || async {
        app.core
            .with_conn(|c| db_passkeys::list_for_user(c, 1))
            .await
            .unwrap()
            .len()
    };

    let res = app.post(&mut here, &path, &[]).await;
    app.assert_flash(&res, "sec.reauth_needed");
    let res = app
        .post(&mut here, &path, &[("current_password", "nope")])
        .await;
    app.assert_flash(&res, "sec.wrong_password");
    assert_eq!(count().await, 1);
    assert!(app.signed_in(&mut laptop).await, "nobody was signed out");

    let res = app
        .post(&mut here, &path, &[("current_password", PASSWORD)])
        .await;
    app.assert_flash(&res, "sec.pk_deleted");
    assert_eq!(count().await, 0);
    assert!(!app.signed_in(&mut laptop).await);
}

#[tokio::test]
async fn changing_the_password_can_revoke_the_api_tokens() {
    let (app, mut ada) = App::set_up().await;
    let bob = app.user("bob", BOB_PASSWORD).await;
    app.give_token(1).await;
    app.give_token(bob).await;

    // The box is on in the form.
    let page = app.get(&mut ada, "/settings/security").await;
    assert!(
        page.body
            .contains(r#"name="revoke_tokens" value="true" checked"#),
        "{}",
        page.body
    );

    let change = |from: &'static str, to: &'static str, revoke: bool| {
        let mut form = vec![
            ("current_password", from),
            ("new_password", to),
            ("confirm_password", to),
        ];
        if revoke {
            form.push(("revoke_tokens", "true"));
        }
        form
    };
    // Unticked: the tokens stay.
    let res = app
        .post(
            &mut ada,
            "/settings/security/password",
            &change(PASSWORD, NEW_PASSWORD, false),
        )
        .await;
    app.assert_flash(&res, "sec.password_changed");
    assert_eq!(app.tokens_of(1).await.len(), 1);
    // Ticked: this user's are revoked, nobody else's.
    let res = app
        .post(
            &mut ada,
            "/settings/security/password",
            &change(NEW_PASSWORD, PASSWORD, true),
        )
        .await;
    app.assert_flash(&res, "sec.password_changed");
    assert!(app.tokens_of(1).await.is_empty());
    assert_eq!(app.tokens_of(bob).await.len(), 1);
    assert!(
        app.signed_in(&mut ada).await,
        "this browser stays signed in"
    );
}

// ---------------------------------------------------------------------------
// VK-12
// ---------------------------------------------------------------------------

const TOTP_SECRET: &str = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";

async fn raw_secret(core: &Core) -> Option<String> {
    core.with_conn(|c| {
        c.query_row("SELECT totp_secret FROM users WHERE id = 1", [], |r| {
            r.get(0)
        })
    })
    .await
    .unwrap()
}

/// A TOTP login on a new device: the password, then the current code.
async fn totp_login(app: &App, secret: &str) -> StatusCode {
    app.core
        .with_conn(|c| c.execute("UPDATE users SET totp_last_step = NULL", []))
        .await
        .unwrap();
    let mut b = Browser::default();
    let res = app
        .post(
            &mut b,
            "/login",
            &[("username", "ada"), ("password", PASSWORD)],
        )
        .await;
    assert_eq!(res.location(), "/login/2fa", "{}", res.body);
    let t = totp::build(secret, "ada", "Kit").unwrap();
    let code = totp::code_at(&t, now());
    let res = app.post(&mut b, "/login/2fa", &[("code", &code)]).await;
    if res.status == StatusCode::SEE_OTHER {
        assert!(app.signed_in(&mut b).await);
    }
    res.status
}

#[tokio::test]
async fn new_two_factor_secrets_are_stored_sealed_and_still_work() {
    let (app, mut ada) = App::set_up().await;
    let res = app
        .post(&mut ada, "/settings/security/2fa/setup", &[])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let stored = raw_secret(&app.core).await.unwrap();
    assert!(crate::crypto::is_sealed(&stored), "{stored}");
    let secret = totp::open_secret(&app.core.session_key, &stored).unwrap();
    assert!(!stored.contains(&secret));

    // The enrolment shows the secret (opened) to type into an app, and the
    // right code turns two-factor on.
    let page = app.get(&mut ada, "/settings/security").await;
    assert!(
        page.body.contains(&totp::format_secret(&secret)),
        "{}",
        page.body
    );
    let t = totp::build(&secret, "ada", "Kit").unwrap();
    let res = app
        .post(
            &mut ada,
            "/settings/security/2fa/enable",
            &[("code", &totp::code_at(&t, now()))],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        raw_secret(&app.core).await.as_deref(),
        Some(stored.as_str())
    );
    assert_eq!(totp_login(&app, &secret).await, StatusCode::SEE_OTHER);

    // A sealed secret that can't be opened (a replaced session.key) never
    // matches: the login is refused, not a server error.
    let foreign = crate::crypto::seal(&Key::generate(), totp::PURPOSE, TOTP_SECRET);
    app.core
        .with_conn(move |c| c.execute("UPDATE users SET totp_secret = ?1", [foreign]))
        .await
        .unwrap();
    assert_eq!(totp_login(&app, &secret).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn plaintext_two_factor_secrets_are_sealed_without_breaking_logins() {
    let (app, _ada) = App::set_up().await;
    app.plaintext_two_factor(1, TOTP_SECRET).await;
    assert_eq!(raw_secret(&app.core).await.as_deref(), Some(TOTP_SECRET));

    // Before the migration has run, the plaintext secret logs in.
    assert_eq!(totp_login(&app, TOTP_SECRET).await, StatusCode::SEE_OTHER);

    // The migration seals it ...
    let key = app.core.session_key.clone();
    let sealed = app
        .core
        .with_conn(move |c| totp::seal_stored(c, &key))
        .await
        .unwrap();
    assert_eq!(sealed, 1);
    let stored = raw_secret(&app.core).await.unwrap();
    assert!(crate::crypto::is_sealed(&stored) && !stored.contains(TOTP_SECRET));
    // ... once: what is sealed already is left as it is ...
    let key = app.core.session_key.clone();
    let again = app
        .core
        .with_conn(move |c| totp::seal_stored(c, &key))
        .await
        .unwrap();
    assert_eq!(again, 0);
    assert_eq!(
        raw_secret(&app.core).await.as_deref(),
        Some(stored.as_str())
    );
    // ... and the same authenticator app still gets in.
    assert_eq!(totp_login(&app, TOTP_SECRET).await, StatusCode::SEE_OTHER);
    let tf = app
        .core
        .with_conn(|c| users::get_totp(c, 1))
        .await
        .unwrap()
        .unwrap();
    assert!(tf.enabled && tf.last_step.is_some());
    assert_eq!(tf.recovery_codes, ["hash"]);
}

#[tokio::test]
async fn a_new_core_seals_the_two_factor_secrets_of_an_older_database() {
    let dir = std::env::temp_dir().join(format!(
        "vole-kit-totp-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pool = db::build_pool(&dir.join("kit.db"));
    db::migrate::run_migrations(&pool, &dir.join("backups")).await;
    db::interact(&pool, |c| {
        let id = users::insert_user(c, "ada", "h", Role::Admin)?;
        users::set_pending_totp_secret(c, id, TOTP_SECRET)?;
        users::enable_totp(c, id, 0, &[])
    })
    .await
    .unwrap();

    // Starting the product builds its Core, which seals them in the background.
    let key = Key::generate();
    let core = Core::new(
        pool.clone(),
        key.clone(),
        reqwest::Client::new(),
        &crate::testing::CONFIG,
    );
    let mut stored = None;
    for _ in 0..100 {
        stored = raw_secret(&core).await;
        if stored.as_deref().is_some_and(crate::crypto::is_sealed) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let stored = stored.unwrap();
    assert!(crate::crypto::is_sealed(&stored), "{stored}");
    assert_eq!(
        totp::open_secret(&key, &stored).as_deref(),
        Some(TOTP_SECRET)
    );
    db::close(&pool).await;
    let _ = std::fs::remove_dir_all(&dir);
}
