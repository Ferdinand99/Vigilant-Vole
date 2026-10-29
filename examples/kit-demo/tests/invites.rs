//! A product that turns invitation links on (`KitFeatures::invites`) and
//! keeps the kit's own sign-in: an admin creates a link, the invitee opens
//! it, picks a user name and a password and is signed in. This is its own
//! test binary because the kit's configuration is installed once per
//! process.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use tower::ServiceExt as _;
use vole_kit::auth::Role;
use vole_kit::auth::invites;
use vole_kit::db;
use vole_kit::state::Core;

static N: AtomicUsize = AtomicUsize::new(0);
const PASSWORD: &str = "Demo-Owner-Pass-2026!";
const INVITEE_PASSWORD: &str = "Invitee-Pass-2026-Kit!";
const REAUTH_NEEDED: &str = "Enter your password to confirm this change.";
const WRONG_PASSWORD: &str = "Your current password is incorrect.";
const INVALID: &str = "This invitation is no longer valid.";

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

/// One browser: its cookies and the address it comes from.
struct Browser {
    cookies: BTreeMap<String, String>,
    peer: SocketAddr,
}

impl Browser {
    fn new() -> Self {
        Self::at("127.0.0.1:40000")
    }

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
    /// An instance with its admin `ada`, signed in in the returned browser.
    async fn set_up() -> (App, Browser) {
        let dir = std::env::temp_dir().join(format!(
            "kit-demo-invites-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let demo = kit_demo::start_with(&dir, &kit_demo::KIT_INVITES)
            .await
            .unwrap();
        let app = App {
            core: demo.core.clone(),
            router: kit_demo::router(&demo),
            dir,
        };
        let mut admin = Browser::new();
        let res = app
            .post(
                &mut admin,
                "/setup",
                &[
                    ("username", "ada"),
                    ("password", PASSWORD),
                    ("confirm", PASSWORD),
                    ("site_name", ""),
                    ("timezone", "UTC"),
                ],
            )
            .await;
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

    /// The admin creates an invitation; returns its token.
    async fn invite(&self, admin: &mut Browser, role: &str, display_name: &str) -> String {
        let res = self
            .post(
                admin,
                "/settings/invites",
                &[
                    ("role", role),
                    ("display_name", display_name),
                    ("current_password", PASSWORD),
                ],
            )
            .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        link_token(&res.body)
    }

    async fn accept(&self, b: &mut Browser, token: &str, username: &str) -> Res {
        self.post(
            b,
            &format!("/invite/{token}"),
            &[
                ("username", username),
                ("display_name", ""),
                ("password", INVITEE_PASSWORD),
                ("confirm", INVITEE_PASSWORD),
            ],
        )
        .await
    }

    async fn invites_in_db(&self) -> i64 {
        self.core
            .with_conn(|c| c.query_row("SELECT COUNT(*) FROM invites", [], |r| r.get(0)))
            .await
            .unwrap()
    }

    async fn role_of(&self, username: &str) -> Option<Role> {
        let name = username.to_string();
        self.core
            .with_conn(move |c| db::users::get_user_by_username(c, &name))
            .await
            .unwrap()
            .map(|u| u.role)
    }

    async fn user_count(&self) -> usize {
        self.core
            .with_conn(|c| db::users::list_users(c))
            .await
            .unwrap()
            .len()
    }
}

/// The token in the link a Users page shows after creating an invitation.
fn link_token(body: &str) -> String {
    let at = body
        .find("http://demo.test/invite/")
        .expect("the page shows the link");
    let start = at + "http://demo.test/invite/".len();
    let token = &body[start..start + invites::TOKEN_LEN];
    assert!(invites::is_well_formed(token), "{token}");
    token.to_string()
}

fn random_token() -> String {
    invites::generate()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_admin_invites_and_the_invitee_signs_up_with_the_role() {
    let (app, mut admin) = App::set_up().await;
    let res = app
        .post(
            &mut admin,
            "/settings/invites",
            &[
                ("role", "editor"),
                ("display_name", "Bea Beaver"),
                ("current_password", PASSWORD),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.headers.get(header::CACHE_CONTROL).unwrap(),
        "no-store",
        "the page holds a link that works once"
    );
    assert!(res.body.contains("created-invite"), "copy field");
    let token = link_token(&res.body);

    // Only the hash is stored, never the token.
    let (stored, len): (i64, i64) = app
        .core
        .with_conn({
            let token = token.clone();
            move |c| {
                let plain: i64 = c.query_row(
                    "SELECT COUNT(*) FROM invites WHERE token_hash = ?1",
                    [token.as_bytes()],
                    |r| r.get(0),
                )?;
                let len: i64 =
                    c.query_row("SELECT length(token_hash) FROM invites", [], |r| r.get(0))?;
                Ok((plain, len))
            }
        })
        .await
        .unwrap();
    assert_eq!((stored, len), (0, 32));

    let mut bea = Browser::new();
    let form = app.get(&mut bea, &format!("/invite/{token}")).await;
    assert_eq!(form.status, StatusCode::OK, "{}", form.body);
    assert!(form.body.contains("Bea Beaver"), "the suggested name");
    assert!(form.body.contains("Editor"), "{}", form.body);
    assert_eq!(form.headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert!(!bea.signed_in());

    let res = app.accept(&mut bea, &token, "bea").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(res.location(), "/");
    assert!(bea.signed_in(), "signed in right away");
    assert_eq!(app.role_of("bea").await, Some(Role::Editor));
    let bea_user = app
        .core
        .with_conn(|c| db::users::get_user_by_username(c, "bea"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bea_user.display_name, "Bea Beaver");
    assert!(bea_user.last_login_at.is_some());

    // The session works: an editor may see the app but not the user list.
    assert_eq!(app.get(&mut bea, "/").await.status, StatusCode::OK);
    assert_eq!(
        app.get(&mut bea, "/settings/users").await.status,
        StatusCode::FORBIDDEN
    );

    // She can sign in later with the password she chose.
    let mut again = Browser::new();
    let res = app
        .post(
            &mut again,
            "/login",
            &[("username", "bea"), ("password", INVITEE_PASSWORD)],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(again.signed_in());

    // The link is used up.
    let mut late = Browser::new();
    let res = app.get(&mut late, &format!("/invite/{token}")).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(res.body.contains(INVALID));
}

#[tokio::test]
async fn an_invited_admin_is_an_admin() {
    let (app, mut admin) = App::set_up().await;
    let token = app.invite(&mut admin, "admin", "").await;
    let mut cy = Browser::new();
    let res = app.accept(&mut cy, &token, "cy").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.role_of("cy").await, Some(Role::Admin));
    assert_eq!(
        app.get(&mut cy, "/settings/users").await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn expired_used_revoked_and_unknown_links_all_get_the_same_answer() {
    let (app, mut admin) = App::set_up().await;
    let expired = app.invite(&mut admin, "viewer", "").await;
    let used = app.invite(&mut admin, "viewer", "").await;
    let revoked = app.invite(&mut admin, "viewer", "").await;
    let pending = app.invite(&mut admin, "viewer", "").await;

    // Expired: the clock passes the seven days.
    let hash = invites::hash(&expired).to_vec();
    app.core
        .with_conn(move |c| {
            c.execute(
                "UPDATE invites SET expires_at = datetime('now', '-1 second') WHERE token_hash = ?1",
                [hash],
            )
        })
        .await
        .unwrap();
    // Used.
    let mut u = Browser::new();
    assert_eq!(
        app.accept(&mut u, &used, "used-by").await.status,
        StatusCode::SEE_OTHER
    );
    // Revoked, from the Users page.
    let id: i64 = app
        .core
        .with_conn({
            let hash = invites::hash(&revoked).to_vec();
            move |c| {
                c.query_row(
                    "SELECT id FROM invites WHERE token_hash = ?1",
                    [hash],
                    |r| r.get(0),
                )
            }
        })
        .await
        .unwrap();
    let res = app
        .post(&mut admin, &format!("/settings/invites/{id}/revoke"), &[])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(res.location(), "/settings/users");

    let unknown = random_token();
    let malformed = "not-a-token".to_string();
    let mut answers: Vec<(String, Res)> = Vec::new();
    for (name, token) in [
        ("expired", &expired),
        ("used", &used),
        ("revoked", &revoked),
        ("unknown", &unknown),
        ("malformed", &malformed),
    ] {
        let mut b = Browser::new();
        let res = app.get(&mut b, &format!("/invite/{token}")).await;
        answers.push((format!("GET {name}"), res));
        let res = app.accept(&mut b, token, "intruder").await;
        answers.push((format!("POST {name}"), res));
        assert!(!b.signed_in(), "{name}");
    }
    // The page may not give a hint which of the four it was. (The GETs and
    // POSTs are the same answer too: no form, no field.)
    let (first_name, first) = &answers[0];
    assert_eq!(first.status, StatusCode::NOT_FOUND, "{first_name}");
    assert!(first.body.contains(INVALID), "{}", first.body);
    for (name, res) in &answers[1..] {
        assert_eq!(res.status, first.status, "{name}");
        assert_eq!(res.body, first.body, "{name} differs from {first_name}");
        assert_eq!(
            res.headers.get(header::CACHE_CONTROL),
            first.headers.get(header::CACHE_CONTROL),
            "{name}"
        );
    }
    assert_eq!(app.role_of("intruder").await, None);

    // The one that is still pending works.
    let mut ok = Browser::new();
    assert_eq!(
        app.get(&mut ok, &format!("/invite/{pending}")).await.status,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_two_people_racing_for_one_link_only_one_gets_in() {
    let (app, mut admin) = App::set_up().await;
    let token = app.invite(&mut admin, "viewer", "").await;
    let (mut a, mut b) = (
        Browser::at("127.0.0.1:40001"),
        Browser::at("127.0.0.1:40002"),
    );
    let (first, second) = tokio::join!(
        app.accept(&mut a, &token, "first"),
        app.accept(&mut b, &token, "second")
    );
    let mut statuses = [first.status, second.status];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::SEE_OTHER, StatusCode::NOT_FOUND],
        "{} / {}",
        first.body,
        second.body
    );
    // Exactly one account came out of it, and exactly one is signed in.
    assert_eq!(app.user_count().await, 2);
    assert_ne!(a.signed_in(), b.signed_in());
    let (winner, loser) = if first.status == StatusCode::SEE_OTHER {
        ("first", &second)
    } else {
        ("second", &first)
    };
    assert!(app.role_of(winner).await.is_some());
    assert!(loser.body.contains(INVALID));
}

#[tokio::test]
async fn a_taken_name_or_a_weak_password_keeps_the_link_alive() {
    let (app, mut admin) = App::set_up().await;
    let token = app.invite(&mut admin, "viewer", "").await;
    let mut b = Browser::new();
    let path = format!("/invite/{token}");

    // A name that exists (any case).
    let res = app.accept(&mut b, &token, "ADA").await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(res.body.contains("is taken"), "{}", res.body);
    // A weak password, and one that does not match its confirmation.
    for (password, confirm) in [("short", "short"), (INVITEE_PASSWORD, "something else")] {
        let res = app
            .post(
                &mut b,
                &path,
                &[
                    ("username", "dee"),
                    ("password", password),
                    ("confirm", confirm),
                ],
            )
            .await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{password}");
        assert!(res.body.contains("field-error"), "{}", res.body);
    }
    // A bad user name.
    let res = app.accept(&mut b, &token, "no spaces allowed").await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!b.signed_in());
    assert_eq!(app.user_count().await, 1);

    // The same link still works.
    let res = app.accept(&mut b, &token, "dee").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.role_of("dee").await, Some(Role::Viewer));
}

#[tokio::test]
async fn only_admins_create_invitations() {
    let (app, mut admin) = App::set_up().await;
    let token = app.invite(&mut admin, "editor", "").await;
    let mut ed = Browser::new();
    assert_eq!(
        app.accept(&mut ed, &token, "ed").await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(app.invites_in_db().await, 1);

    let res = app
        .post(
            &mut ed,
            "/settings/invites",
            &[
                ("role", "admin"),
                ("display_name", ""),
                ("current_password", INVITEE_PASSWORD),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let id = 1;
    let res = app
        .post(&mut ed, &format!("/settings/invites/{id}/revoke"), &[])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert_eq!(app.invites_in_db().await, 1, "nothing was created");

    // Nor without being signed in at all.
    let mut nobody = Browser::new();
    let res = app
        .post(
            &mut nobody,
            "/settings/invites",
            &[("role", "viewer"), ("current_password", PASSWORD)],
        )
        .await;
    assert_ne!(res.status, StatusCode::OK);
    assert_eq!(app.invites_in_db().await, 1);
}

#[tokio::test]
async fn creating_an_invitation_asks_for_the_admins_password() {
    let (app, mut admin) = App::set_up().await;
    for (password, message) in [("", REAUTH_NEEDED), ("not the password", WRONG_PASSWORD)] {
        for role in ["viewer", "admin"] {
            let res = app
                .post(
                    &mut admin,
                    "/settings/invites",
                    &[("role", role), ("current_password", password)],
                )
                .await;
            assert_eq!(res.status, StatusCode::SEE_OTHER, "{role} {password:?}");
            assert_eq!(res.location(), "/settings/users");
            let page = app.get(&mut admin, "/settings/users").await;
            assert!(page.body.contains(message), "{message}");
            assert!(!page.body.contains("/invite/"));
        }
    }
    assert_eq!(app.invites_in_db().await, 0);
    // The right password does it.
    app.invite(&mut admin, "viewer", "").await;
    assert_eq!(app.invites_in_db().await, 1);
}

#[tokio::test]
async fn the_users_page_lists_pending_invitations_and_revokes_them() {
    let (app, mut admin) = App::set_up().await;
    let page = app.get(&mut admin, "/settings/users").await;
    assert!(page.body.contains("Invite"), "the button");
    assert!(page.body.contains("invite-user"), "the sheet");
    assert!(!page.body.contains("Pending invitations"));

    let keep = app.invite(&mut admin, "viewer", "Keep me").await;
    let drop_it = app.invite(&mut admin, "admin", "").await;
    let page = app.get(&mut admin, "/settings/users").await;
    assert!(page.body.contains("Pending invitations"));
    assert!(page.body.contains("Keep me"));
    assert!(!page.body.contains(&keep), "the token is never shown again");

    let id: i64 = app
        .core
        .with_conn({
            let hash = invites::hash(&drop_it).to_vec();
            move |c| {
                c.query_row(
                    "SELECT id FROM invites WHERE token_hash = ?1",
                    [hash],
                    |r| r.get(0),
                )
            }
        })
        .await
        .unwrap();
    let path = format!("/settings/invites/{id}/revoke");
    let res = app.post(&mut admin, &path, &[]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let page = app.get(&mut admin, "/settings/users").await;
    assert!(page.body.contains("Invitation revoked"));
    assert!(page.body.contains("Keep me"));
    // Revoked links stay dead; revoking twice says so.
    let mut b = Browser::new();
    assert_eq!(
        app.get(&mut b, &format!("/invite/{drop_it}")).await.status,
        StatusCode::NOT_FOUND
    );
    app.post(&mut admin, &path, &[]).await;
    let page = app.get(&mut admin, "/settings/users").await;
    assert!(page.body.contains("no longer pending"));
}

#[tokio::test]
async fn there_is_a_limit_to_pending_invitations() {
    let (app, mut admin) = App::set_up().await;
    app.core
        .with_conn(|c| {
            for n in 0..vole_kit::db::invites::MAX_PENDING {
                vole_kit::db::invites::insert(c, &[n as u8; 32], Role::Viewer, "", 1)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let res = app
        .post(
            &mut admin,
            "/settings/invites",
            &[("role", "viewer"), ("current_password", PASSWORD)],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(
        app.invites_in_db().await,
        vole_kit::db::invites::MAX_PENDING
    );
}

#[tokio::test]
async fn the_link_uses_the_public_url_when_one_is_set() {
    let (app, mut admin) = App::set_up().await;
    app.core
        .with_conn(|c| db::settings::set(c, db::settings::BASE_URL, "https://notes.example.com/"))
        .await
        .unwrap();
    let res = app
        .post(
            &mut admin,
            "/settings/invites",
            &[("role", "viewer"), ("current_password", PASSWORD)],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(
        res.body.contains("https://notes.example.com/invite/"),
        "{}",
        res.body
    );
    assert!(!res.body.contains("notes.example.com//invite"));
}

#[tokio::test]
async fn guessing_links_is_throttled_per_address() {
    let (app, _admin) = App::set_up().await;
    let mut attacker = Browser::at("203.0.113.9:5000");
    let mut statuses = Vec::new();
    for _ in 0..30 {
        let token = random_token();
        statuses.push(app.accept(&mut attacker, &token, "x").await.status);
    }
    assert!(statuses[..5].iter().all(|s| *s == StatusCode::NOT_FOUND));
    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "{statuses:?}"
    );
    // Another address is not affected.
    let mut other = Browser::at("203.0.113.77:5000");
    let res = app.accept(&mut other, &random_token(), "x").await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_library_functions_follow_the_same_rules() {
    let (app, mut admin) = App::set_up().await;
    let token = app.invite(&mut admin, "editor", "Eve").await;
    let invite = invites::lookup(&app.core, &token).await.unwrap().unwrap();
    assert_eq!(invite.role, Role::Editor);
    assert_eq!(invite.display_name, "Eve");
    assert_eq!(invite.created_by, Some(1));
    assert!(
        invites::lookup(&app.core, &random_token())
            .await
            .unwrap()
            .is_none()
    );

    // create_user + consume (a product that registers users itself).
    let id = invites::create_user(&app.core, "eve", "Eve", invite.role, "!external")
        .await
        .unwrap()
        .unwrap();
    assert!(
        invites::create_user(&app.core, "EVE", "", Role::Viewer, "!external")
            .await
            .unwrap()
            .is_none(),
        "taken, whatever the case"
    );
    assert!(
        invites::consume(&app.core, &token, id)
            .await
            .unwrap()
            .is_ok()
    );
    assert!(
        invites::consume(&app.core, &token, id)
            .await
            .unwrap()
            .is_err(),
        "once"
    );
    assert!(invites::lookup(&app.core, &token).await.unwrap().is_none());
}

#[tokio::test]
async fn the_table_has_the_planned_columns() {
    // The kit's migration creates the table, not the product.
    let (app, mut admin) = App::set_up().await;
    let token = app.invite(&mut admin, "viewer", "").await;
    let found = invites::lookup(&app.core, &token).await.unwrap();
    assert!(found.is_some());
    let columns: Vec<String> = app
        .core
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT name FROM pragma_table_info('invites')")?;
            stmt.query_map([], |r| r.get(0))?.collect()
        })
        .await
        .unwrap();
    for column in [
        "id",
        "token_hash",
        "role",
        "display_name",
        "created_by",
        "created_at",
        "expires_at",
        "used_at",
        "used_by",
    ] {
        assert!(columns.iter().any(|c| c == column), "{column}: {columns:?}");
    }
}
