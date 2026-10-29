//! Invitation links for a product that signs people in on its own pages
//! (`CredentialMode::External`): the kit's `/invite/{token}` leads to the
//! product's setup page with the token, and the product accepts the
//! invitation with `auth::invites`. This is its own test binary because the
//! kit's configuration is installed once per process.
//!
//! The "product" is a few routes in this file standing in for what a real
//! one does: create the first admin, register an invited user, confirm the
//! admin.

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
use vole_kit::auth::invites::{self, AcceptError};
use vole_kit::auth::pages::{check_setup_token, insert_first_admin};
use vole_kit::auth::{Role, reauth};
use vole_kit::db;
use vole_kit::state::Core;

static N: AtomicUsize = AtomicUsize::new(0);
const INVALID: &str = "This invitation is no longer valid.";

// ---------------------------------------------------------------------------
// The product's own routes
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct Q {
    user: Option<String>,
    invite: Option<String>,
    token: Option<String>,
    /// "lookup": only look the invitation up; "create+consume": make the
    /// account with `create_user` and then `consume`.
    how: Option<String>,
}

/// Its setup page: the first admin, or, with `?invite=`, the invited user.
async fn vault_setup(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    Query(q): Query<Q>,
) -> Response {
    let name = q.user.unwrap_or_else(|| "ada".to_string());
    if let Some(token) = q.invite {
        let Some(invite) = invites::lookup(&core, &token).await.unwrap() else {
            return (StatusCode::GONE, "invalid").into_response();
        };
        if q.how.as_deref() == Some("lookup") {
            return format!("{}:{}", invite.role, invite.display_name).into_response();
        }
        let id = if q.how.as_deref() == Some("create+consume") {
            let id = invites::create_user(&core, &name, "", invite.role, "!external")
                .await
                .unwrap()
                .expect("a free name");
            if invites::consume(&core, &token, id).await.unwrap().is_err() {
                return (StatusCode::GONE, "lost the race").into_response();
            }
            id
        } else {
            match invites::accept(&core, &token, &name, "", "!external")
                .await
                .unwrap()
            {
                Ok(id) => id,
                Err(AcceptError::Invalid) => return (StatusCode::GONE, "invalid").into_response(),
                Err(AcceptError::UsernameTaken) => {
                    return (StatusCode::CONFLICT, "taken").into_response();
                }
            }
        };
        let user = core
            .with_conn(move |c| db::users::get_user(c, id))
            .await
            .unwrap()
            .unwrap();
        let jar = login_jar(jar, user.id, user.session_version, secure_cookies(&headers));
        return (jar, Redirect::to("/")).into_response();
    }
    check_setup_token(&core, &headers, peer, q.token.as_deref()).expect("setup is open in tests");
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

/// Its confirmation page: a fresh grant for the signed-in session.
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

fn vault(core: &Core) -> Router {
    Router::new()
        .route("/vault/setup", get(vault_setup))
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
}

#[derive(Default)]
struct Browser {
    cookies: BTreeMap<String, String>,
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
    /// An instance whose admin the product created, signed in in the
    /// returned browser.
    async fn set_up() -> (App, Browser) {
        let dir = std::env::temp_dir().join(format!(
            "kit-demo-invites-ext-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let demo = kit_demo::start_with(&dir, &kit_demo::KIT_INVITES_EXTERNAL)
            .await
            .unwrap();
        let router = kit_demo::router(&demo).merge(vault(&demo.core));
        let app = App {
            core: demo.core.clone(),
            router,
            dir,
        };
        let mut admin = Browser::default();
        let res = app.get(&mut admin, "/vault/setup").await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        assert!(admin.cookies.contains_key("kd_session"));
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

    /// The admin (with a fresh confirmation) creates an invitation.
    async fn invite(&self, admin: &mut Browser, role: &str) -> String {
        self.get(admin, "/vault/reauth").await;
        let res = self
            .post(
                admin,
                "/settings/invites",
                &[("role", role), ("display_name", "Gus")],
            )
            .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        let at = res.body.find("http://demo.test/invite/").expect("the link");
        let start = at + "http://demo.test/invite/".len();
        res.body[start..start + invites::TOKEN_LEN].to_string()
    }

    async fn invites_in_db(&self) -> i64 {
        self.core
            .with_conn(|c| c.query_row("SELECT COUNT(*) FROM invites", [], |r| r.get(0)))
            .await
            .unwrap()
    }

    async fn expire(&self, token: &str) {
        let hash = invites::hash(token).to_vec();
        self.core
            .with_conn(move |c| {
                c.execute(
                    "UPDATE invites SET expires_at = datetime('now', '-1 second')
                     WHERE token_hash = ?1",
                    [hash],
                )
            })
            .await
            .unwrap();
    }
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_link_leads_to_the_products_setup_page_with_the_token() {
    let (app, mut admin) = App::set_up().await;
    let token = app.invite(&mut admin, "editor").await;

    let mut guest = Browser::default();
    let res = app.get(&mut guest, &format!("/invite/{token}")).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(res.location(), format!("/vault/setup?invite={token}"));
    assert_eq!(res.headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert!(guest.cookies.is_empty());

    // The kit does not take the password itself.
    let res = app
        .post(
            &mut guest,
            &format!("/invite/{token}"),
            &[("username", "gus"), ("password", "x")],
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert_eq!(app.invites_in_db().await, 1);
    assert!(
        invites::lookup(&app.core, &token).await.unwrap().is_some(),
        "still pending"
    );
}

#[tokio::test]
async fn a_dead_link_gets_the_generic_page_and_no_redirect() {
    let (app, mut admin) = App::set_up().await;
    let expired = app.invite(&mut admin, "viewer").await;
    app.expire(&expired).await;
    let unknown = invites::generate();
    let mut pages = Vec::new();
    for token in [&expired, &unknown] {
        let mut b = Browser::default();
        let res = app.get(&mut b, &format!("/invite/{token}")).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
        assert!(res.body.contains(INVALID));
        assert_eq!(res.location(), "");
        pages.push(res.body);
    }
    assert_eq!(pages[0], pages[1]);
}

#[tokio::test]
async fn the_product_accepts_the_invitation_in_one_step() {
    let (app, mut admin) = App::set_up().await;
    let token = app.invite(&mut admin, "editor").await;

    let mut guest = Browser::default();
    let res = app
        .get(
            &mut guest,
            &format!("/vault/setup?invite={token}&how=lookup"),
        )
        .await;
    assert_eq!(res.body, "editor:Gus", "lookup sees role and name");

    let res = app
        .get(&mut guest, &format!("/vault/setup?invite={token}&user=gus"))
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(guest.cookies.contains_key("kd_session"));
    let gus = app
        .core
        .with_conn(|c| db::users::get_user_by_username(c, "gus"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gus.role, Role::Editor);
    assert_eq!(gus.display_name, "Gus", "the invitation's suggestion");
    assert_eq!(gus.password_hash, "!external");

    // Used: lookup, accept and consume all say no.
    assert!(invites::lookup(&app.core, &token).await.unwrap().is_none());
    let mut late = Browser::default();
    let res = app
        .get(&mut late, &format!("/vault/setup?invite={token}&user=hal"))
        .await;
    assert_eq!(res.status, StatusCode::GONE);
    assert!(late.cookies.is_empty());
    assert!(
        invites::consume(&app.core, &token, gus.id)
            .await
            .unwrap()
            .is_err()
    );
    // And the user list has just the two.
    assert_eq!(
        app.core
            .with_conn(|c| db::users::count_users(c))
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn consume_fails_when_the_invitation_went_away_in_the_meantime() {
    let (app, mut admin) = App::set_up().await;
    // The product created the account first, then tries to use the link.
    for (how, spoil) in [("expired", 0), ("revoked", 1), ("used", 2)] {
        let token = app.invite(&mut admin, "viewer").await;
        let invite = invites::lookup(&app.core, &token).await.unwrap().unwrap();
        let name = format!("user-{how}");
        let id = invites::create_user(&app.core, &name, "", invite.role, "!external")
            .await
            .unwrap()
            .unwrap();
        match spoil {
            0 => app.expire(&token).await,
            1 => {
                let res = app
                    .post(
                        &mut admin,
                        &format!("/settings/invites/{}/revoke", invite.id),
                        &[],
                    )
                    .await;
                assert_eq!(res.status, StatusCode::SEE_OTHER);
            }
            _ => assert!(
                invites::consume(&app.core, &token, id)
                    .await
                    .unwrap()
                    .is_ok()
            ),
        }
        let result = invites::consume(&app.core, &token, id).await.unwrap();
        assert!(result.is_err(), "{how}");
        // `accept` refuses too, and leaves no new account behind.
        let before = app
            .core
            .with_conn(|c| db::users::count_users(c))
            .await
            .unwrap();
        let accepted = invites::accept(&app.core, &token, &format!("late-{how}"), "", "!external")
            .await
            .unwrap();
        assert_eq!(accepted, Err(AcceptError::Invalid), "{how}");
        let after = app
            .core
            .with_conn(|c| db::users::count_users(c))
            .await
            .unwrap();
        assert_eq!(before, after, "{how}");
    }
}

#[tokio::test]
async fn consume_after_create_user_works_through_the_product_page() {
    let (app, mut admin) = App::set_up().await;
    let token = app.invite(&mut admin, "viewer").await;
    let mut guest = Browser::default();
    let res = app
        .get(
            &mut guest,
            &format!("/vault/setup?invite={token}&user=ivy&how=create%2Bconsume"),
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let ivy = app
        .core
        .with_conn(|c| db::users::get_user_by_username(c, "ivy"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ivy.role, Role::Viewer);
    let used_by: Option<i64> = app
        .core
        .with_conn(|c| c.query_row("SELECT used_by FROM invites", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(used_by, Some(ivy.id));
}

#[tokio::test]
async fn an_admin_confirms_through_the_product_before_inviting() {
    let (app, mut admin) = App::set_up().await;
    // No fresh confirmation: refused, nothing created.
    let res = app
        .post(&mut admin, "/settings/invites", &[("role", "viewer")])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(res.location(), "/settings/users");
    assert_eq!(app.invites_in_db().await, 0);

    // The sheet says where to confirm, and has no password field.
    let page = app.get(&mut admin, "/settings/users").await;
    assert!(page.body.contains("invite-user"));
    assert!(
        page.body
            .contains(r#"href="/vault/reauth?next=%2Fsettings%2Fusers""#)
    );
    assert!(!page.body.contains(r#"id="iu-current-password""#));

    // After confirming it works, and the link is the kit's.
    let token = app.invite(&mut admin, "viewer").await;
    assert!(invites::is_well_formed(&token));
    assert_eq!(app.invites_in_db().await, 1);
}
