//! A product that turns one-time sign-in codes on
//! (`KitFeatures::sign_in_codes`) and keeps the kit's own sign-in: an admin
//! resets someone's sign-in from the Users page, gets a code once, and the
//! user redeems it at `/sign-in-code` for a new password. This is its own
//! test binary because the kit's configuration is installed once per
//! process.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tower::ServiceExt as _;
use vole_kit::auth::sign_in_codes;
use vole_kit::auth::{Role, TokenScope, totp};
use vole_kit::db;
use vole_kit::state::Core;

static N: AtomicUsize = AtomicUsize::new(0);
const PASSWORD: &str = "Demo-Owner-Pass-2026!";
const TEMP: &str = "Temp-Pass-2026-Kit!";
const NEW_PASSWORD: &str = "Brand-New-Pass-2026!";
const REAUTH_NEEDED: &str = "Enter your password to confirm this change.";
const WRONG_PASSWORD: &str = "Your current password is incorrect.";
const SECRET: &str = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";

// ---------------------------------------------------------------------------
// What the kit logs
// ---------------------------------------------------------------------------

static LOGS: OnceLock<Arc<Mutex<String>>> = OnceLock::new();

/// Everything the kit logs, from every thread, as text.
struct Capture(Arc<Mutex<String>>);

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut Visitor(&mut line));
        let mut logs = self.0.lock().unwrap();
        logs.push_str(event.metadata().target());
        logs.push_str(&line);
        logs.push('\n');
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn logs() -> String {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let shared = Arc::new(Mutex::new(String::new()));
        LOGS.set(shared.clone()).unwrap();
        let _ = tracing::subscriber::set_global_default(Capture(shared));
    });
    LOGS.get().unwrap().lock().unwrap().clone()
}

// ---------------------------------------------------------------------------
// A fake mail server
// ---------------------------------------------------------------------------

/// Minimal SMTP server that keeps the DATA of every message it gets.
struct FakeSmtp {
    port: u16,
    messages: Arc<Mutex<Vec<String>>>,
}

impl FakeSmtp {
    async fn start() -> FakeSmtp {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let kept = messages.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let kept = kept.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    if write.write_all(b"220 fake ESMTP\r\n").await.is_err() {
                        return;
                    }
                    let (mut data, mut in_data) = (String::new(), false);
                    while let Ok(Some(line)) = lines.next_line().await {
                        if in_data {
                            if line == "." {
                                in_data = false;
                                kept.lock().unwrap().push(std::mem::take(&mut data));
                                let _ = write.write_all(b"250 queued\r\n").await;
                            } else {
                                data.push_str(&line);
                                data.push('\n');
                            }
                            continue;
                        }
                        let upper = line.to_ascii_uppercase();
                        let reply: &[u8] = if upper.starts_with("EHLO") {
                            b"250-fake\r\n250 8BITMIME\r\n"
                        } else if upper.starts_with("DATA") {
                            in_data = true;
                            b"354 go ahead\r\n"
                        } else if upper.starts_with("QUIT") {
                            let _ = write.write_all(b"221 bye\r\n").await;
                            break;
                        } else {
                            b"250 ok\r\n"
                        };
                        if write.write_all(reply).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        FakeSmtp { port, messages }
    }

    fn messages(&self) -> Vec<String> {
        self.messages.lock().unwrap().clone()
    }
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

/// One browser: its cookies and the address it comes from.
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
    /// An instance with its admin `ada`, signed in in the returned browser.
    async fn set_up() -> (App, Browser) {
        let _ = logs();
        let dir = std::env::temp_dir().join(format!(
            "kit-demo-codes-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let demo = kit_demo::start_with(&dir, &kit_demo::KIT_CODES)
            .await
            .unwrap();
        let app = App {
            core: demo.core.clone(),
            router: kit_demo::router(&demo),
            dir,
        };
        let mut admin = Browser::at("127.0.0.1:40000");
        let res = app
            .post(
                &mut admin,
                "/setup",
                &[
                    ("username", "ada"),
                    ("password", PASSWORD),
                    ("confirm", PASSWORD),
                    ("site_name", "Ours"),
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

    /// A user, made directly (`password` is hashed for real when given).
    async fn add_user(&self, name: &str, password: Option<&str>) -> i64 {
        let hash = match password {
            Some(p) => vole_kit::auth::hash_password_async(p.to_string())
                .await
                .unwrap(),
            None => "!x".to_string(),
        };
        let name = name.to_string();
        self.core
            .with_conn(move |c| db::users::insert_user_full(c, &name, &hash, Role::Viewer, ""))
            .await
            .unwrap()
    }

    /// A code for `user_id`, made directly; returns it as shown.
    async fn code_for(&self, user_id: i64) -> String {
        let code = sign_in_codes::generate();
        let hash = sign_in_codes::hash(&code).to_vec();
        self.core
            .with_conn(move |c| db::sign_in_codes::insert(c, user_id, &hash, 1))
            .await
            .unwrap();
        code
    }

    async fn redeem(&self, b: &mut Browser, username: &str, code: &str, password: &str) -> Res {
        self.post(
            b,
            "/sign-in-code",
            &[
                ("username", username),
                ("code", code),
                ("password", password),
                ("confirm", password),
            ],
        )
        .await
    }

    /// The admin asks for a code for `id` and, when it works, returns it.
    async fn admin_code(&self, admin: &mut Browser, id: i64) -> (Res, Option<String>) {
        let res = self
            .post(
                admin,
                &format!("/settings/users/{id}/sign-in-code"),
                &[("current_password", PASSWORD)],
            )
            .await;
        let code = shown_code(&res.body);
        (res, code)
    }

    /// Like [`App::admin_code`], and also emails it (the box is ticked).
    async fn admin_code_mailed(&self, admin: &mut Browser, id: i64) -> (Res, Option<String>) {
        let res = self
            .post(
                admin,
                &format!("/settings/users/{id}/sign-in-code"),
                &[("current_password", PASSWORD), ("send_email", "true")],
            )
            .await;
        let code = shown_code(&res.body);
        (res, code)
    }

    /// Sets a user's email address on the edit sheet, with the password.
    async fn set_email(&self, admin: &mut Browser, id: i64, email: &str) -> Res {
        self.post(
            admin,
            &format!("/settings/users/{id}"),
            &[
                ("display_name", "Bob"),
                ("role", "viewer"),
                ("email", email),
                ("current_password", PASSWORD),
            ],
        )
        .await
    }

    async fn stored_email(&self, id: i64) -> Option<String> {
        self.core
            .with_conn(move |c| db::sign_in_codes::email(c, id))
            .await
            .unwrap()
    }

    /// How many pending codes the user has.
    async fn pending(&self, id: i64) -> i64 {
        self.core
            .with_conn(move |c| db::sign_in_codes::count_pending(c, id))
            .await
            .unwrap()
    }

    /// An API token for the user, made directly.
    async fn add_token(&self, id: i64, hash: &'static str) {
        self.core
            .with_conn(move |c| {
                db::api_tokens::insert(c, id, "t", "vv_x", hash, TokenScope::Read, None)
            })
            .await
            .unwrap();
    }

    async fn tokens_of(&self, id: i64) -> i64 {
        self.core
            .with_conn(move |c| {
                c.query_row(
                    "SELECT COUNT(*) FROM api_tokens WHERE user_id = ?1",
                    [id],
                    |r| r.get(0),
                )
            })
            .await
            .unwrap()
    }

    async fn codes_in_db(&self) -> i64 {
        self.core
            .with_conn(|c| c.query_row("SELECT COUNT(*) FROM sign_in_codes", [], |r| r.get(0)))
            .await
            .unwrap()
    }

    async fn user(&self, name: &str) -> db::users::User {
        let name = name.to_string();
        self.core
            .with_conn(move |c| db::users::get_user_by_username(c, &name))
            .await
            .unwrap()
            .unwrap()
    }

    async fn signs_in(&self, username: &str, password: &str) -> bool {
        let mut b = Browser::at("10.9.9.9:1");
        self.post(
            &mut b,
            "/login",
            &[("username", username), ("password", password)],
        )
        .await;
        b.signed_in()
    }

    async fn settings_value(&self, key: &'static str) -> Option<String> {
        self.core
            .with_conn(move |c| db::settings::get(c, key))
            .await
            .unwrap()
    }

    /// Saves the instance's mail settings on the General page.
    async fn set_mail(&self, admin: &mut Browser, extra: &[(&str, &str)]) -> Res {
        let mut form = vec![
            ("site_name", "Ours"),
            ("base_url", ""),
            ("timezone", "UTC"),
            ("language", "en"),
            ("notes_per_page", "20"),
            ("current_password", PASSWORD),
        ];
        form.extend_from_slice(extra);
        self.post(admin, "/settings/general", &form).await
    }

    /// The General page's form with the kit's fields and `extra`; the
    /// password only when given.
    async fn general(
        &self,
        admin: &mut Browser,
        extra: &[(&str, &str)],
        password: Option<&str>,
    ) -> Res {
        let mut form = vec![
            ("site_name", "Ours"),
            ("base_url", ""),
            ("timezone", "UTC"),
            ("language", "en"),
            ("notes_per_page", "20"),
        ];
        // Later pairs win: the first of a name is the one the kit reads.
        for (k, v) in extra {
            form.retain(|(name, _)| name != k);
            form.push((*k, *v));
        }
        if let Some(password) = password {
            form.push(("current_password", password));
        }
        self.post(admin, "/settings/general", &form).await
    }

    /// Mail settings pointing at the fake server `smtp`.
    async fn mail_on(&self, admin: &mut Browser, smtp: &FakeSmtp) {
        let port = smtp.port.to_string();
        let res = self
            .set_mail(
                admin,
                &[
                    ("smtp_host", "127.0.0.1"),
                    ("smtp_port", &port),
                    ("smtp_security", "none"),
                    ("smtp_username", ""),
                    ("smtp_password_clear", "true"),
                    ("smtp_from", "Kit Demo <noreply@example.com>"),
                ],
            )
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    }

    /// The Users page once more (it shows the flash message).
    async fn users_page(&self, b: &mut Browser) -> String {
        self.get(b, "/settings/users").await.body
    }
}

/// The code the Users page shows after creating one.
fn shown_code(body: &str) -> Option<String> {
    let marker = "id=\"created-sign-in-code\">";
    let at = body.find(marker)? + marker.len();
    let end = body[at..].find('<')?;
    Some(body[at..at + end].to_string())
}

/// A response, without the user name the page echoes.
fn anonymous(res: &Res, username: &str) -> (StatusCode, String) {
    (res.status, res.body.replace(username, "USER"))
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_admin_resets_and_the_user_signs_in_with_the_code_once() {
    let (app, mut admin) = App::set_up().await;
    let bob = app.add_user("bob", Some(TEMP)).await;
    // Bob is signed in on another device.
    let mut old = Browser::at("10.0.0.2:1");
    app.post(
        &mut old,
        "/login",
        &[("username", "bob"), ("password", TEMP)],
    )
    .await;
    assert!(old.signed_in());
    assert_eq!(app.get(&mut old, "/").await.status, StatusCode::OK);
    let version = app.user("bob").await.session_version;

    let (res, code) = app.admin_code(&mut admin, bob).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.headers.get(header::CACHE_CONTROL).unwrap(),
        "no-store",
        "the page holds a code that works once"
    );
    let code = code.expect("the page shows the code");
    // Eight groups of four, in Crockford's alphabet.
    let groups: Vec<&str> = code.split('-').collect();
    assert_eq!(groups.len(), 8, "{code}");
    assert!(groups.iter().all(|g| g.len() == 4), "{code}");
    assert!(
        code.bytes()
            .all(|c| c == b'-' || b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&c)),
        "{code}"
    );
    assert!(res.body.contains("http://demo.test/sign-in-code"));
    assert!(res.body.contains("Shown once"));

    // Only the SHA-256 is stored, and it expires in a day.
    let (plain, len, hours): (i64, i64, f64) = app
        .core
        .with_conn({
            let code = code.clone();
            move |c| {
                let plain = c.query_row(
                    "SELECT COUNT(*) FROM sign_in_codes WHERE code_hash = ?1 OR code_hash = ?2",
                    rusqlite::params![code.as_bytes(), code.replace('-', "").as_bytes()],
                    |r| r.get(0),
                )?;
                let (len, hours) = c.query_row(
                    "SELECT length(code_hash), (julianday(expires_at) - julianday(created_at)) * 24
                     FROM sign_in_codes",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                Ok((plain, len, hours))
            }
        })
        .await
        .unwrap();
    assert_eq!((plain, len), (0, 32));
    assert!((hours - 24.0).abs() < 0.01, "{hours}");

    // The form.
    let mut bob_browser = Browser::at("10.0.0.3:1");
    let form = app.get(&mut bob_browser, "/sign-in-code").await;
    assert_eq!(form.status, StatusCode::OK, "{}", form.body);
    assert_eq!(form.headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    for needle in [
        "name=\"username\"",
        "name=\"code\"",
        "name=\"password\"",
        "name=\"confirm\"",
    ] {
        assert!(form.body.contains(needle), "{needle}");
    }
    let login = app.get(&mut Browser::at("10.0.0.4:1"), "/login").await;
    assert!(
        login.body.contains("href=\"/sign-in-code\""),
        "the sign-in page links it"
    );

    // Typed the way people do: lower case, spaces for dashes.
    let typed = code.to_lowercase().replace('-', " ");
    let res = app
        .redeem(&mut bob_browser, "BOB", &typed, NEW_PASSWORD)
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(res.location(), "/");
    assert!(bob_browser.signed_in(), "signed in right away");
    assert_eq!(app.get(&mut bob_browser, "/").await.status, StatusCode::OK);

    // The new password works, the old one does not, and the old session is
    // gone: the session version moved on.
    assert!(app.signs_in("bob", NEW_PASSWORD).await);
    assert!(!app.signs_in("bob", TEMP).await);
    let after = app.user("bob").await;
    assert_ne!(after.session_version, version);
    assert!(after.last_login_at.is_some());
    let res = app.get(&mut old, "/").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(res.location().starts_with("/login"), "{}", res.location());

    // It works once.
    let mut again = Browser::at("10.0.0.5:1");
    let res = app
        .redeem(&mut again, "bob", &code, "Another-Pass-2026!")
        .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", res.body);
    assert!(!again.signed_in());
    assert!(app.signs_in("bob", NEW_PASSWORD).await, "nothing changed");

    // The code never reached the log.
    let log = logs();
    assert!(log.contains("sign-in code created"), "{log}");
    assert!(!log.contains(&code), "the code is in the log");
    assert!(
        !log.contains(&code.replace('-', "")),
        "the code is in the log"
    );
    assert!(!log.contains(NEW_PASSWORD));
}

#[tokio::test]
async fn expired_revoked_used_wrong_user_and_unknown_codes_get_the_same_answer() {
    let (app, _admin) = App::set_up().await;
    let expired = app.add_user("expired-user", None).await;
    let revoked = app.add_user("revoked-user", None).await;
    let used = app.add_user("used-user", None).await;
    let disabled = app.add_user("disabled-user", None).await;
    let thief = app.add_user("thief", None).await;
    let victim = app.add_user("victim", None).await;
    let fine = app.add_user("fine-user", None).await;
    let _ = (thief, fine);

    let c_expired = app.code_for(expired).await;
    app.core
        .with_conn(move |c| {
            c.execute(
                "UPDATE sign_in_codes SET expires_at = datetime('now', '-1 second') WHERE user_id = ?1",
                [expired],
            )
        })
        .await
        .unwrap();
    let c_revoked = app.code_for(revoked).await;
    let _newer = app.code_for(revoked).await; // revokes the one above
    let c_used = app.code_for(used).await;
    let mut ok = Browser::at("10.1.0.1:1");
    assert_eq!(
        app.redeem(&mut ok, "used-user", &c_used, NEW_PASSWORD)
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    let c_disabled = app.code_for(disabled).await;
    app.core
        .with_conn(move |c| c.execute("UPDATE users SET active = 0 WHERE id = ?1", [disabled]))
        .await
        .unwrap();
    let c_victim = app.code_for(victim).await;
    let unknown = sign_in_codes::generate();

    let cases: Vec<(&str, &str, String)> = vec![
        ("expired-user", "expired", c_expired),
        ("revoked-user", "revoked", c_revoked),
        ("used-user", "used", c_used),
        ("disabled-user", "disabled", c_disabled),
        // Someone else's valid code.
        ("thief", "wrong user", c_victim.clone()),
        ("fine-user", "unknown code", unknown),
        ("fine-user", "malformed", "not-a-code".to_string()),
        ("fine-user", "empty", String::new()),
        ("no-such-user", "unknown user", sign_in_codes::generate()),
    ];
    let mut answers = Vec::new();
    for (i, (username, what, code)) in cases.iter().enumerate() {
        let mut b = Browser::at(&format!("10.1.1.{i}:1"));
        let res = app.redeem(&mut b, username, code, NEW_PASSWORD).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{what}: {}", res.body);
        assert!(
            res.body.contains("don’t match, or the code has expired"),
            "{what}"
        );
        assert!(!b.signed_in(), "{what}");
        assert!(b.cookies.is_empty(), "{what}: {:?}", b.cookies);
        answers.push((what, anonymous(&res, username)));
    }
    let first = &answers[0].1;
    for (what, answer) in &answers {
        assert_eq!(answer, first, "{what} differs from the first answer");
    }

    // Nothing was spent by someone else's attempt: the victim's code still
    // works for the victim.
    let mut v = Browser::at("10.1.2.1:1");
    let res = app.redeem(&mut v, "victim", &c_victim, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(v.signed_in());
    // And nothing changed for the others.
    for name in [
        "expired-user",
        "revoked-user",
        "disabled-user",
        "thief",
        "fine-user",
    ] {
        assert!(!app.signs_in(name, NEW_PASSWORD).await, "{name}");
    }
}

#[tokio::test]
async fn repeated_failures_are_throttled_per_account_and_per_address() {
    let (app, _admin) = App::set_up().await;
    let target = app.add_user("target", None).await;
    let good = app.code_for(target).await;

    // Per account: a few free guesses, then the account waits, even for the
    // right code, and even from another address.
    let wrong = sign_in_codes::generate();
    let mut statuses = Vec::new();
    for i in 0..14 {
        let mut b = Browser::at(&format!("10.2.0.{i}:1"));
        let res = app.redeem(&mut b, "target", &wrong, NEW_PASSWORD).await;
        statuses.push(res.status);
        if res.status == StatusCode::TOO_MANY_REQUESTS {
            assert!(res.body.contains("Too many attempts"), "{}", res.body);
        }
    }
    assert!(
        statuses[..10]
            .iter()
            .all(|s| *s == StatusCode::UNAUTHORIZED),
        "{statuses:?}"
    );
    assert_eq!(
        *statuses.last().unwrap(),
        StatusCode::TOO_MANY_REQUESTS,
        "{statuses:?}"
    );
    let mut b = Browser::at("10.2.1.1:1");
    let res = app.redeem(&mut b, "target", &good, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS, "{}", res.body);
    assert!(!b.signed_in());
    assert!(!app.signs_in("target", NEW_PASSWORD).await);

    // Per address: many names from one address.
    let mut statuses = Vec::new();
    let mut spray = Browser::at("10.2.2.2:1");
    for i in 0..26 {
        let res = app
            .redeem(&mut spray, &format!("guess{i}"), &wrong, NEW_PASSWORD)
            .await;
        statuses.push(res.status);
    }
    assert!(
        statuses[..20]
            .iter()
            .all(|s| *s == StatusCode::UNAUTHORIZED),
        "{statuses:?}"
    );
    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "{statuses:?}"
    );

    // Mistakes with the new password are not guesses at the code.
    let other = app.add_user("other", None).await;
    let c_other = app.code_for(other).await;
    let mut slip = Browser::at("10.2.3.3:1");
    for _ in 0..30 {
        let res = app.redeem(&mut slip, "other", &c_other, "short").await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let res = app.redeem(&mut slip, "other", &c_other, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
}

#[tokio::test]
async fn a_bad_new_password_leaves_the_code_unspent() {
    let (app, _admin) = App::set_up().await;
    let id = app.add_user("carol", None).await;
    let code = app.code_for(id).await;
    let mut b = Browser::at("10.3.0.1:1");

    let weak = app.redeem(&mut b, "carol", &code, "short").await;
    assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        weak.body.contains("Use at least 10 characters."),
        "{}",
        weak.body
    );
    let mismatch = app
        .post(
            &mut b,
            "/sign-in-code",
            &[
                ("username", "carol"),
                ("code", &code),
                ("password", NEW_PASSWORD),
                ("confirm", "Different-Pass-2026!"),
            ],
        )
        .await;
    assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(mismatch.body.contains("The passwords don’t match."));
    assert!(!b.signed_in());

    let res = app.redeem(&mut b, "carol", &code, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(b.signed_in());
}

#[tokio::test]
async fn two_factor_is_kept_and_still_asked() {
    let (app, _admin) = App::set_up().await;
    let id = app.add_user("dana", None).await;
    app.core
        .with_conn(move |c| {
            db::users::set_pending_totp_secret(c, id, SECRET)?;
            db::users::enable_totp(c, id, 0, &[])
        })
        .await
        .unwrap();
    let code = app.code_for(id).await;

    let mut b = Browser::at("10.4.0.1:1");
    let res = app.redeem(&mut b, "dana", &code, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(res.location(), "/login/2fa");
    assert!(b.cookies.contains_key("kd_2fa"));
    assert!(
        !b.signed_in(),
        "the code replaces the password, not the second factor"
    );
    assert_eq!(app.get(&mut b, "/").await.status, StatusCode::SEE_OTHER);
    assert!(app.user("dana").await.totp_enabled);

    let t = totp::build(SECRET, "dana", "Kit Demo").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let res = app
        .post(&mut b, "/login/2fa", &[("code", &totp::code_at(&t, now))])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(b.signed_in());
    assert!(app.user("dana").await.totp_enabled, "still on");

    // The password is the new one; the code is gone.
    let hash = app.user("dana").await.password_hash;
    assert!(vole_kit::auth::verify_password(&hash, NEW_PASSWORD));
}

#[tokio::test]
async fn only_another_user_can_be_reset_by_an_admin_who_confirms() {
    let (app, mut admin) = App::set_up().await;
    let bob = app.add_user("bob", Some(TEMP)).await;
    let ada = app.user("ada").await.id;

    // Not yourself.
    let (res, code) = app.admin_code(&mut admin, ada).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(code.is_none());
    assert!(
        app.users_page(&mut admin)
            .await
            .contains("You can’t reset your own sign-in here."),
    );

    // Not without the admin's own password, or with a wrong one.
    for (password, message) in [("", REAUTH_NEEDED), ("not the password", WRONG_PASSWORD)] {
        let res = app
            .post(
                &mut admin,
                &format!("/settings/users/{bob}/sign-in-code"),
                &[("current_password", password)],
            )
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        assert!(shown_code(&res.body).is_none());
        assert!(
            app.users_page(&mut admin).await.contains(message),
            "{message}"
        );
    }
    // A form that sends nothing at all.
    let res = app
        .post(
            &mut admin,
            &format!("/settings/users/{bob}/sign-in-code"),
            &[],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(app.codes_in_db().await, 0);

    // Not for a user who does not exist or is disabled.
    let res = app
        .post(
            &mut admin,
            "/settings/users/999/sign-in-code",
            &[("current_password", PASSWORD)],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    app.core
        .with_conn(move |c| c.execute("UPDATE users SET active = 0 WHERE id = ?1", [bob]))
        .await
        .unwrap();
    let (res, code) = app.admin_code(&mut admin, bob).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert!(code.is_none());
    assert!(
        app.users_page(&mut admin)
            .await
            .contains("That account is disabled.")
    );
    assert_eq!(app.codes_in_db().await, 0);

    // Not by someone who is not an admin.
    app.core
        .with_conn(move |c| c.execute("UPDATE users SET active = 1 WHERE id = ?1", [bob]))
        .await
        .unwrap();
    let mut b = Browser::at("10.5.0.1:1");
    app.post(&mut b, "/login", &[("username", "bob"), ("password", TEMP)])
        .await;
    assert!(b.signed_in());
    let res = app
        .post(
            &mut b,
            &format!("/settings/users/{ada}/sign-in-code"),
            &[("current_password", TEMP)],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert_eq!(app.codes_in_db().await, 0);

    // The Users page offers it for others, not for oneself.
    let page = app.users_page(&mut admin).await;
    assert!(page.contains("reset-code-"), "{page}");
    assert!(page.contains(&format!("/settings/users/{bob}/sign-in-code")));
    assert!(!page.contains(&format!("/settings/users/{ada}/sign-in-code")));
}

#[tokio::test]
async fn a_new_code_revokes_the_earlier_ones() {
    let (app, mut admin) = App::set_up().await;
    let bob = app.add_user("bob", None).await;
    let (_, first) = app.admin_code(&mut admin, bob).await;
    let (_, second) = app.admin_code(&mut admin, bob).await;
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_ne!(first, second);
    assert_eq!(app.codes_in_db().await, 2);

    let mut b = Browser::at("10.6.0.1:1");
    let res = app.redeem(&mut b, "bob", &first, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    let res = app.redeem(&mut b, "bob", &second, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
}

#[tokio::test]
async fn admins_set_and_clear_an_email_address_on_the_edit_sheet() {
    let (app, mut admin) = App::set_up().await;
    let bob = app.add_user("bob", None).await;
    let email = |app: &App| {
        let core = app.core.clone();
        async move {
            core.with_conn(move |c| db::sign_in_codes::email(c, bob))
                .await
                .unwrap()
        }
    };
    let page = app.users_page(&mut admin).await;
    assert!(page.contains("name=\"email\""), "{page}");

    let set = |email: &'static str| {
        [
            ("display_name", "Bob"),
            ("role", "viewer"),
            ("email", email),
            ("current_password", PASSWORD),
        ]
    };
    let res = app
        .post(
            &mut admin,
            &format!("/settings/users/{bob}"),
            &set(" bob@example.com "),
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(email(&app).await.as_deref(), Some("bob@example.com"));
    assert!(
        app.users_page(&mut admin)
            .await
            .contains("value=\"bob@example.com\"")
    );

    // Something that is no address changes nothing, not even the name.
    for bad in ["bob", "Bob <bob@example.com>", "a@b.com, c@d.com"] {
        let res = app
            .post(
                &mut admin,
                &format!("/settings/users/{bob}"),
                &[
                    ("display_name", "Changed"),
                    ("role", "viewer"),
                    ("email", bad),
                    ("current_password", PASSWORD),
                ],
            )
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER);
        assert!(
            app.users_page(&mut admin)
                .await
                .contains("Enter a valid email address"),
            "{bad}"
        );
        assert_eq!(
            email(&app).await.as_deref(),
            Some("bob@example.com"),
            "{bad}"
        );
        assert_ne!(app.user("bob").await.display_name, "Changed");
    }

    // A form without the field leaves it alone; an empty one clears it.
    let res = app
        .post(
            &mut admin,
            &format!("/settings/users/{bob}"),
            &[("display_name", "Bob"), ("role", "viewer")],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(email(&app).await.as_deref(), Some("bob@example.com"));
    app.post(&mut admin, &format!("/settings/users/{bob}"), &set(""))
        .await;
    assert_eq!(email(&app).await, None);
}

#[tokio::test]
async fn no_email_is_sent_without_mail_settings() {
    let (app, mut admin) = App::set_up().await;
    let smtp = FakeSmtp::start().await;
    let bob = app.add_user("bob", None).await;
    app.set_email(&mut admin, bob, "bob@example.com").await;

    // The sheet offers no email box, and even a form that asks for one sends
    // nothing.
    let page = app.users_page(&mut admin).await;
    assert!(!page.contains("name=\"send_email\""), "{page}");
    let (res, code) = app.admin_code_mailed(&mut admin, bob).await;
    assert!(code.is_some(), "{}", res.body);
    assert!(!res.body.contains("emailed"), "{}", res.body);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(smtp.messages().is_empty());
    // The settings page offers the form, empty.
    let general = app.get(&mut admin, "/settings/general").await;
    assert!(
        general.body.contains("name=\"smtp_host\""),
        "{}",
        general.body
    );
    assert!(general.body.contains("name=\"smtp-test\"") || general.body.contains("smtp-test"));
}

#[tokio::test]
async fn with_mail_settings_the_code_is_emailed_and_never_logged() {
    let (app, mut admin) = App::set_up().await;
    let smtp = FakeSmtp::start().await;
    let bob = app.add_user("bob", None).await;
    app.set_email(&mut admin, bob, "bob@example.com").await;
    let port = smtp.port.to_string();
    let res = app
        .set_mail(
            &mut admin,
            &[
                ("smtp_host", "127.0.0.1"),
                ("smtp_port", &port),
                ("smtp_security", "none"),
                ("smtp_from", "Kit Demo <noreply@example.com>"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(
        app.settings_value("smtp_host").await.as_deref(),
        Some("127.0.0.1")
    );

    // The sheet has the box, ticked, with the address next to it.
    let page = app.users_page(&mut admin).await;
    assert!(page.contains("name=\"send_email\""), "{page}");
    assert!(
        page.contains("Also send it by email to bob@example.com"),
        "{page}"
    );

    let (res, code) = app.admin_code_mailed(&mut admin, bob).await;
    let code = code.expect("the page shows the code");
    assert!(
        res.body.contains("was also emailed to bob@example.com"),
        "{}",
        res.body
    );
    let messages = smtp.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    let mail = &messages[0];
    assert!(mail.contains("To: bob@example.com"), "{mail}");
    assert!(
        mail.contains("From: \"Kit Demo\" <noreply@example.com>"),
        "{mail}"
    );
    assert!(mail.contains(&code), "{mail}");
    assert!(mail.contains("bob"), "{mail}");
    assert!(
        mail.contains("Subject: Your sign-in code for Ours"),
        "{mail}"
    );
    assert!(!mail.contains("multipart"), "a plain message: {mail}");
    // The same code redeems.
    let mut b = Browser::at("10.7.0.1:1");
    let res = app
        .redeem(&mut b, "bob", &mail_code(mail), NEW_PASSWORD)
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);

    let log = logs();
    assert!(log.contains("sign-in code created"));
    assert!(!log.contains(&code), "the code is in the log");
}

/// The code line of a received message.
fn mail_code(mail: &str) -> String {
    mail.lines()
        .map(str::trim)
        .find(|l| l.len() == 39 && l.split('-').count() == 8)
        .expect("a line with the code")
        .to_string()
}

#[tokio::test]
async fn a_mail_that_fails_still_shows_the_code_and_logs_neither_it_nor_the_password() {
    let (app, mut admin) = App::set_up().await;
    // This server offers no AUTH, so signing in to it fails.
    let smtp = FakeSmtp::start().await;
    let bob = app.add_user("bob", None).await;
    app.set_email(&mut admin, bob, "bob@example.com").await;
    let port = smtp.port.to_string();
    let secret = "smtp-secret-pw-12345";
    let res = app
        .set_mail(
            &mut admin,
            &[
                ("smtp_host", "127.0.0.1"),
                ("smtp_port", &port),
                ("smtp_security", "none"),
                ("smtp_username", "mailer"),
                ("smtp_password", secret),
                ("smtp_from", "noreply@example.com"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);

    // Sealed at rest, never shown again.
    let stored = app.settings_value("smtp_password").await.unwrap();
    assert!(stored.starts_with("sealed:v1:"), "{stored}");
    assert!(!stored.contains(secret));
    let general = app.get(&mut admin, "/settings/general").await.body;
    assert!(!general.contains(secret));
    assert!(general.contains("A password is saved."), "{general}");
    assert!(general.contains("value=\"mailer\""));

    let (res, code) = app.admin_code_mailed(&mut admin, bob).await;
    let code = code.expect("the code is shown even when the mail fails");
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body.contains("The email couldn’t be sent."),
        "{}",
        res.body
    );
    assert!(!res.body.contains("was also emailed to"));
    assert!(smtp.messages().is_empty());

    let log = logs();
    assert!(log.contains("sign-in code email failed"), "{log}");
    assert!(!log.contains(&code));
    assert!(!log.contains(&code.replace('-', "")));
    assert!(!log.contains(secret));

    // Saving again with the password field empty keeps it; clearing removes it.
    let res = app
        .set_mail(
            &mut admin,
            &[
                ("smtp_host", "127.0.0.1"),
                ("smtp_port", &port),
                ("smtp_security", "none"),
                ("smtp_username", "mailer"),
                ("smtp_from", "noreply@example.com"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(
        app.settings_value("smtp_password").await.as_deref(),
        Some(stored.as_str())
    );
    let res = app
        .set_mail(
            &mut admin,
            &[
                ("smtp_host", "127.0.0.1"),
                ("smtp_port", &port),
                ("smtp_security", "none"),
                ("smtp_username", ""),
                ("smtp_password_clear", "true"),
                ("smtp_from", "noreply@example.com"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.settings_value("smtp_password").await, None);
    assert_eq!(app.settings_value("smtp_username").await, None);
}

#[tokio::test]
async fn the_mail_form_checks_its_fields_and_the_test_button_sends_a_test() {
    let (app, mut admin) = App::set_up().await;
    let smtp = FakeSmtp::start().await;

    let res = app
        .set_mail(
            &mut admin,
            &[
                ("smtp_host", "smtp example"),
                ("smtp_port", "99999"),
                ("smtp_from", "nonsense"),
                ("smtp_username", "u"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", res.body);
    for needle in [
        "Enter the SMTP server",
        "Enter a port between 1 and 65535",
        "Enter an address like alerts@example.com",
        "Enter the password for this username",
        "value=\"smtp example\"",
    ] {
        assert!(res.body.contains(needle), "{needle}");
    }
    assert_eq!(app.settings_value("smtp_host").await, None);

    // Not set up: the button says so.
    let res = app
        .post(
            &mut admin,
            "/settings/general/smtp-test",
            &[("to", "ada@example.com")],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let page = app.get(&mut admin, "/settings/general").await.body;
    assert!(
        page.contains("Save a mail server and a sender first."),
        "{page}"
    );

    let port = smtp.port.to_string();
    app.set_mail(
        &mut admin,
        &[
            ("smtp_host", "127.0.0.1"),
            ("smtp_port", &port),
            ("smtp_security", "none"),
            ("smtp_from", "noreply@example.com"),
        ],
    )
    .await;
    let res = app
        .post(
            &mut admin,
            "/settings/general/smtp-test",
            &[("to", "not an address")],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(smtp.messages().is_empty());
    let res = app
        .post(
            &mut admin,
            "/settings/general/smtp-test",
            &[("to", "ada@example.com")],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let page = app.get(&mut admin, "/settings/general").await.body;
    assert!(
        page.contains("Test email sent to ada@example.com."),
        "{page}"
    );
    let messages = smtp.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("To: ada@example.com"));

    // Turning mail off removes every setting: the fields are there, empty.
    let res = app
        .set_mail(
            &mut admin,
            &[
                ("smtp_host", ""),
                ("smtp_port", ""),
                ("smtp_security", "starttls"),
                ("smtp_username", ""),
                ("smtp_from", ""),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    for key in ["smtp_host", "smtp_port", "smtp_from", "smtp_security"] {
        assert_eq!(app.settings_value(key).await, None, "{key}");
    }

    // Only for admins.
    let bob = app.add_user("bob", Some(TEMP)).await;
    let _ = bob;
    let mut b = Browser::at("10.8.0.1:1");
    app.post(&mut b, "/login", &[("username", "bob"), ("password", TEMP)])
        .await;
    let res = app
        .post(
            &mut b,
            "/settings/general/smtp-test",
            &[("to", "bob@example.com")],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// The lifecycle of a pending code
// ---------------------------------------------------------------------------

/// Ada makes a code for `id` and the code is pending.
async fn pending_code(app: &App, admin: &mut Browser, id: i64) -> String {
    let (res, code) = app.admin_code(admin, id).await;
    let code = code.unwrap_or_else(|| panic!("no code: {}", res.body));
    assert_eq!(app.pending(id).await, 1);
    code
}

/// The code no longer works for `name`.
async fn assert_dead(app: &App, name: &str, code: &str, what: &str) {
    let mut b = Browser::at("10.20.0.1:1");
    let res = app.redeem(&mut b, name, code, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{what}: {}", res.body);
    assert!(!b.signed_in(), "{what}");
}

#[tokio::test]
async fn a_pending_code_ends_with_a_password_change_a_reset_a_disable_and_a_sign_in() {
    let (app, mut admin) = App::set_up().await;

    // The user changes their own password.
    let own = app.add_user("own-change", Some(TEMP)).await;
    let mut session = Browser::at("10.20.1.1:1");
    app.post(
        &mut session,
        "/login",
        &[("username", "own-change"), ("password", TEMP)],
    )
    .await;
    assert!(session.signed_in());
    let code = pending_code(&app, &mut admin, own).await;
    let res = app
        .post(
            &mut session,
            "/settings/security/password",
            &[
                ("current_password", TEMP),
                ("new_password", NEW_PASSWORD),
                ("confirm_password", NEW_PASSWORD),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.pending(own).await, 0, "own password change");
    assert_dead(&app, "own-change", &code, "own password change").await;

    // An admin sets a temporary password.
    let reset = app.add_user("admin-reset", Some(TEMP)).await;
    let code = pending_code(&app, &mut admin, reset).await;
    let res = app
        .post(
            &mut admin,
            &format!("/settings/users/{reset}/password"),
            &[("password", NEW_PASSWORD), ("current_password", PASSWORD)],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.pending(reset).await, 0, "admin password reset");
    assert_dead(&app, "admin-reset", &code, "admin password reset").await;

    // An admin resets their two-factor.
    let tfa = app.add_user("tfa-reset", None).await;
    app.core
        .with_conn(move |c| {
            db::users::set_pending_totp_secret(c, tfa, SECRET)?;
            db::users::enable_totp(c, tfa, 0, &[])
        })
        .await
        .unwrap();
    let code = pending_code(&app, &mut admin, tfa).await;
    let res = app
        .post(
            &mut admin,
            &format!("/settings/users/{tfa}/2fa/reset"),
            &[("current_password", PASSWORD)],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.pending(tfa).await, 0, "two-factor reset");
    assert_dead(&app, "tfa-reset", &code, "two-factor reset").await;

    // Disabling, and enabling again, do not bring a code back.
    let off = app.add_user("off-on", None).await;
    let code = pending_code(&app, &mut admin, off).await;
    app.post(&mut admin, &format!("/settings/users/{off}/disable"), &[])
        .await;
    assert_eq!(app.pending(off).await, 0, "disable");
    app.post(&mut admin, &format!("/settings/users/{off}/enable"), &[])
        .await;
    assert_eq!(app.pending(off).await, 0, "enable");
    assert_dead(&app, "off-on", &code, "disable and enable").await;
    app.post(&mut admin, &format!("/settings/users/{off}/disable"), &[])
        .await;
    let code = app.code_for(off).await;
    assert_eq!(app.pending(off).await, 1, "a code made while disabled");
    app.post(&mut admin, &format!("/settings/users/{off}/enable"), &[])
        .await;
    assert_eq!(app.pending(off).await, 0, "enable alone");
    assert_dead(&app, "off-on", &code, "enable alone").await;

    // The user signs in the usual way.
    let back = app.add_user("remembered", Some(TEMP)).await;
    let code = pending_code(&app, &mut admin, back).await;
    let mut b = Browser::at("10.20.2.1:1");
    app.post(
        &mut b,
        "/login",
        &[("username", "remembered"), ("password", TEMP)],
    )
    .await;
    assert!(b.signed_in());
    assert_eq!(app.pending(back).await, 0, "a sign-in");
    assert_dead(&app, "remembered", &code, "a sign-in").await;
    // A wrong password does not count as one.
    let other = app.add_user("fumbles", Some(TEMP)).await;
    let code = pending_code(&app, &mut admin, other).await;
    let mut b = Browser::at("10.20.2.2:1");
    app.post(
        &mut b,
        "/login",
        &[("username", "fumbles"), ("password", "not the password")],
    )
    .await;
    assert!(!b.signed_in());
    assert_eq!(app.pending(other).await, 1, "a failed sign-in");
    let mut b = Browser::at("10.20.2.3:1");
    let res = app.redeem(&mut b, "fumbles", &code, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
}

#[tokio::test]
async fn the_users_page_shows_a_pending_code_and_an_admin_revokes_it() {
    let (app, mut admin) = App::set_up().await;
    let bob = app.add_user("bob", None).await;
    let carol = app.add_user("carol", Some(TEMP)).await;
    let mut carol_browser = Browser::at("10.21.0.1:1");
    app.post(
        &mut carol_browser,
        "/login",
        &[("username", "carol"), ("password", TEMP)],
    )
    .await;
    assert!(carol_browser.signed_in());

    let revoke = format!("/settings/users/{bob}/sign-in-code/revoke");
    let page = app.users_page(&mut admin).await;
    assert!(!page.contains("Code pending"), "{page}");
    assert!(!page.contains("revoke-code-"), "{page}");

    let code = pending_code(&app, &mut admin, bob).await;
    let page = app.users_page(&mut admin).await;
    assert!(page.contains("Code pending"), "{page}");
    assert!(
        page.contains(&format!("id=\"revoke-code-{bob}\"")),
        "{page}"
    );
    assert!(page.contains(&revoke));
    assert!(page.contains("Revoke code"), "{page}");
    // Carol has no code, so no sheet and no label for her.
    assert!(!page.contains(&format!("revoke-code-{carol}")));

    // Not without the admin's password, not with a wrong one.
    for (password, message) in [("", REAUTH_NEEDED), ("not the password", WRONG_PASSWORD)] {
        let res = app
            .post(&mut admin, &revoke, &[("current_password", password)])
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        assert!(app.users_page(&mut admin).await.contains(message));
        assert_eq!(app.pending(bob).await, 1, "still pending");
    }
    // Not by someone who is not an admin.
    let res = app
        .post(&mut carol_browser, &revoke, &[("current_password", TEMP)])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert_eq!(app.pending(bob).await, 1);

    let res = app
        .post(&mut admin, &revoke, &[("current_password", PASSWORD)])
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    let page = app.users_page(&mut admin).await;
    assert!(page.contains("pending code for bob was revoked"), "{page}");
    assert!(!page.contains("Code pending"), "{page}");
    assert_eq!(app.pending(bob).await, 0);
    assert_dead(&app, "bob", &code, "revoked by the admin").await;
    // Nothing else about the account changed.
    assert!(app.user("bob").await.active);
}

#[tokio::test]
async fn redeeming_deletes_the_users_api_tokens_and_only_theirs() {
    let (app, mut admin) = App::set_up().await;
    let bob = app.add_user("bob", None).await;
    let carol = app.add_user("carol", None).await;
    app.add_token(bob, "hash-bob-1").await;
    app.add_token(bob, "hash-bob-2").await;
    app.add_token(carol, "hash-carol-1").await;
    let (_, code) = app.admin_code(&mut admin, bob).await;
    let code = code.unwrap();
    assert_eq!(app.tokens_of(bob).await, 2, "making a code deletes nothing");

    // A refused attempt deletes nothing.
    let mut b = Browser::at("10.22.0.1:1");
    let wrong = sign_in_codes::generate();
    let res = app.redeem(&mut b, "bob", &wrong, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.tokens_of(bob).await, 2);

    let res = app.redeem(&mut b, "bob", &code, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.tokens_of(bob).await, 0, "the tokens went with the code");
    assert_eq!(app.tokens_of(carol).await, 1, "other users keep theirs");
}

// ---------------------------------------------------------------------------
// Where the code goes needs the admin's password
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_mail_settings_and_the_public_url_need_the_admins_password() {
    let (app, mut admin) = App::set_up().await;
    let smtp = FakeSmtp::start().await;
    let port = smtp.port.to_string();
    let mail = [
        ("smtp_host", "127.0.0.1"),
        ("smtp_port", port.as_str()),
        ("smtp_security", "none"),
        ("smtp_from", "Kit Demo <noreply@example.com>"),
    ];

    // The mail server: not without the password, not with a wrong one.
    for (password, message) in [
        (None, REAUTH_NEEDED),
        (Some(""), REAUTH_NEEDED),
        (Some("not the password"), WRONG_PASSWORD),
    ] {
        let res = app.general(&mut admin, &mail, password).await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", res.body);
        assert!(res.body.contains(message), "{message}: {}", res.body);
        assert_eq!(app.settings_value("smtp_host").await, None);
    }
    let res = app.general(&mut admin, &mail, Some(PASSWORD)).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(
        app.settings_value("smtp_host").await.as_deref(),
        Some("127.0.0.1")
    );

    // Changing any one field of it asks again; the sender here.
    let moved = [
        ("smtp_host", "127.0.0.1"),
        ("smtp_port", port.as_str()),
        ("smtp_security", "none"),
        ("smtp_from", "Evil <evil@example.com>"),
    ];
    let res = app.general(&mut admin, &moved, None).await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", res.body);
    assert_eq!(
        app.settings_value("smtp_from").await.as_deref(),
        Some("Kit Demo <noreply@example.com>")
    );

    // The public URL, where the link in the mail points.
    let url = [("base_url", "https://vole.example.org")];
    let res = app
        .general(&mut admin, &[mail.as_slice(), &url].concat(), None)
        .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", res.body);
    assert_eq!(app.settings_value("base_url").await, None);
    let res = app
        .general(
            &mut admin,
            &[mail.as_slice(), &url].concat(),
            Some(PASSWORD),
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(
        app.settings_value("base_url").await.as_deref(),
        Some("https://vole.example.org")
    );

    // Saving the same values again, or changing something else, asks for
    // nothing.
    let same = [mail.as_slice(), &url, &[("site_name", "Renamed")]].concat();
    let res = app.general(&mut admin, &same, None).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(
        app.settings_value("site_name").await.as_deref(),
        Some("Renamed")
    );

    // A form without the mail fields (a stale page, a script) keeps them.
    let res = app
        .general(
            &mut admin,
            &[
                ("base_url", "https://vole.example.org"),
                ("site_name", "Ours"),
            ],
            None,
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    for (key, value) in [
        ("smtp_host", "127.0.0.1"),
        ("smtp_port", port.as_str()),
        ("smtp_from", "Kit Demo <noreply@example.com>"),
    ] {
        assert_eq!(
            app.settings_value(key).await.as_deref(),
            Some(value),
            "{key}"
        );
    }
    // Turning it off is saying so, and asks for the password.
    let off = [
        ("smtp_host", ""),
        ("smtp_port", ""),
        ("smtp_username", ""),
        ("smtp_from", ""),
        ("base_url", "https://vole.example.org"),
    ];
    let res = app.general(&mut admin, &off, None).await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", res.body);
    assert!(app.settings_value("smtp_host").await.is_some());
    let res = app.general(&mut admin, &off, Some(PASSWORD)).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.settings_value("smtp_host").await, None);
    // The page has the password field.
    let page = app.get(&mut admin, "/settings/general").await.body;
    assert!(page.contains("name=\"current_password\""), "{page}");
}

#[tokio::test]
async fn a_users_email_address_needs_the_admins_password_to_change() {
    let (app, mut admin) = App::set_up().await;
    let bob = app.add_user("bob", None).await;
    let path = format!("/settings/users/{bob}");
    let form = |email: &'static str, password: &'static str| {
        [
            ("display_name", "Robert"),
            ("role", "viewer"),
            ("email", email),
            ("current_password", password),
        ]
    };

    // Setting one: not without the password, not with a wrong one.
    for (password, message) in [("", REAUTH_NEEDED), ("not the password", WRONG_PASSWORD)] {
        let res = app
            .post(&mut admin, &path, &form("bob@example.com", password))
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        assert!(app.users_page(&mut admin).await.contains(message));
        assert_eq!(app.stored_email(bob).await, None);
        assert_ne!(
            app.user("bob").await.display_name,
            "Robert",
            "all or nothing"
        );
    }
    let res = app
        .post(&mut admin, &path, &form("bob@example.com", PASSWORD))
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(
        app.stored_email(bob).await.as_deref(),
        Some("bob@example.com")
    );

    // Changing it, and clearing it, ask again.
    for new in ["evil@example.com", ""] {
        let res = app.post(&mut admin, &path, &form(new, "")).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER);
        assert_eq!(
            app.stored_email(bob).await.as_deref(),
            Some("bob@example.com"),
            "{new:?}"
        );
    }
    // The same address, or a form without the field, asks for nothing.
    let res = app
        .post(&mut admin, &path, &form("bob@example.com", ""))
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(
        app.users_page(&mut admin).await.contains("User saved"),
        "unchanged address"
    );
    app.post(
        &mut admin,
        &path,
        &[("display_name", "Bobby"), ("role", "viewer")],
    )
    .await;
    assert_eq!(app.user("bob").await.display_name, "Bobby");
    assert_eq!(
        app.stored_email(bob).await.as_deref(),
        Some("bob@example.com")
    );
    app.post(&mut admin, &path, &form("", PASSWORD)).await;
    assert_eq!(app.stored_email(bob).await, None);
}

#[tokio::test]
async fn the_code_is_emailed_only_when_the_admin_ticks_the_box() {
    let (app, mut admin) = App::set_up().await;
    let smtp = FakeSmtp::start().await;
    let bob = app.add_user("bob", None).await;
    let carol = app.add_user("carol", None).await;
    app.set_email(&mut admin, bob, "bob@example.com").await;
    app.mail_on(&mut admin, &smtp).await;

    // The box is on the sheet of a user with an address, ticked; not on
    // carol's, who has none.
    let page = app.users_page(&mut admin).await;
    assert_eq!(page.matches("name=\"send_email\"").count(), 1, "{page}");
    assert!(
        page.contains("name=\"send_email\" value=\"true\" checked"),
        "{page}"
    );

    // A form that doesn't tick it sends no mail.
    let (res, code) = app.admin_code(&mut admin, bob).await;
    assert!(code.is_some(), "{}", res.body);
    assert!(!res.body.contains("emailed"), "{}", res.body);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(smtp.messages().is_empty());

    // Ticked: one mail. Ticked for a user without an address: none.
    let (res, _) = app.admin_code_mailed(&mut admin, bob).await;
    assert!(res.body.contains("was also emailed to bob@example.com"));
    let (res, code) = app.admin_code_mailed(&mut admin, carol).await;
    assert!(
        code.is_some() && !res.body.contains("emailed"),
        "{}",
        res.body
    );
    assert_eq!(smtp.messages().len(), 1);
}

#[tokio::test]
async fn names_cannot_pull_the_code_into_the_subject_or_add_headers() {
    let (app, mut admin) = App::set_up().await;
    let smtp = FakeSmtp::start().await;
    let bob = app.add_user("bob", None).await;
    app.set_email(&mut admin, bob, "bob@example.com").await;
    app.mail_on(&mut admin, &smtp).await;
    // The person's display name and the site name are the attacker's.
    let evil = "{code} {url} Bob\r\nBcc: evil@example.com";
    let res = app
        .post(
            &mut admin,
            &format!("/settings/users/{bob}"),
            &[
                ("display_name", evil),
                ("role", "viewer"),
                ("email", "bob@example.com"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(app.user("bob").await.display_name, evil.trim());
    let site = "Ours {code}\r\nBcc: evil@example.com";
    let res = app
        .general(
            &mut admin,
            &[
                ("site_name", site),
                ("smtp_host", "127.0.0.1"),
                ("smtp_security", "none"),
            ],
            None,
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);

    let (res, code) = app.admin_code_mailed(&mut admin, bob).await;
    let code = code.expect("the page shows the code");
    assert!(res.body.contains("was also emailed to"), "{}", res.body);
    let messages = smtp.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    let mail = &messages[0];
    let (head, body) = mail.split_once("\n\n").expect("headers and body");
    let compact = code.replace('-', "");
    assert!(!head.contains(&code) && !head.contains(&compact), "{head}");
    assert!(
        !head.lines().any(|l| l.to_lowercase().starts_with("bcc:")),
        "a header was added: {head}"
    );
    assert_eq!(
        head.lines().filter(|l| l.starts_with("Subject:")).count(),
        1,
        "{head}"
    );
    assert!(!head.contains("{code}"), "{head}");
    assert!(
        !body.contains("{code}") && !body.contains("{url}"),
        "{body}"
    );
    assert_eq!(body.matches(&code).count(), 1, "the code once: {body}");
    // The line break did not start a line of the body either.
    assert!(
        !body.lines().any(|l| l.to_lowercase().starts_with("bcc:")),
        "{body}"
    );
    assert_eq!(mail_code(mail), code);
}

#[tokio::test]
async fn unencrypted_credentials_are_refused_and_the_test_button_is_limited() {
    let (app, mut admin) = App::set_up().await;
    let res = app
        .set_mail(
            &mut admin,
            &[
                ("smtp_host", "mail.example.com"),
                ("smtp_port", "25"),
                ("smtp_security", "none"),
                ("smtp_username", "mailer"),
                ("smtp_password", "pw"),
                ("smtp_from", "noreply@example.com"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", res.body);
    assert!(
        res.body
            .contains("can’t be sent unencrypted to another computer"),
        "{}",
        res.body
    );
    assert_eq!(app.settings_value("smtp_host").await, None);
    assert_eq!(app.settings_value("smtp_password").await, None);
    // Without credentials, or with encryption, the same server is fine.
    for extra in [
        [
            ("smtp_username", ""),
            ("smtp_password", ""),
            ("smtp_security", "none"),
        ],
        [
            ("smtp_username", "mailer"),
            ("smtp_password", "pw"),
            ("smtp_security", "tls"),
        ],
    ] {
        let mut form = vec![
            ("smtp_host", "mail.example.com"),
            ("smtp_port", "25"),
            ("smtp_from", "noreply@example.com"),
        ];
        form.extend_from_slice(&extra);
        let res = app.set_mail(&mut admin, &form).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    }

    // Five test mails, then a wait.
    let smtp = FakeSmtp::start().await;
    app.mail_on(&mut admin, &smtp).await;
    for i in 0..7 {
        let res = app
            .post(
                &mut admin,
                "/settings/general/smtp-test",
                &[("to", "ada@example.com")],
            )
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{i}");
        let page = app.get(&mut admin, "/settings/general").await.body;
        if i < 5 {
            assert!(page.contains("Test email sent to"), "{i}: {page}");
        } else {
            assert!(page.contains("Too many test emails"), "{i}: {page}");
        }
    }
    assert_eq!(smtp.messages().len(), 5);
}

// ---------------------------------------------------------------------------
// The throttle and the log
// ---------------------------------------------------------------------------

#[tokio::test]
async fn codes_and_sign_in_have_their_own_throttles_and_a_huge_name_is_just_wrong() {
    let (app, _admin) = App::set_up().await;
    let wrong = sign_in_codes::generate();

    // Guessing codes for ada does not use up ada's sign-in budget ...
    for i in 0..14 {
        let mut b = Browser::at(&format!("10.23.0.{i}:1"));
        app.redeem(&mut b, "ada", &wrong, NEW_PASSWORD).await;
    }
    let mut b = Browser::at("10.23.1.1:1");
    let res = app.redeem(&mut b, "ada", &wrong, NEW_PASSWORD).await;
    assert_eq!(
        res.status,
        StatusCode::TOO_MANY_REQUESTS,
        "codes are waiting"
    );
    assert!(app.signs_in("ada", PASSWORD).await, "the sign-in is not");

    // ... and failed sign-ins for bob do not stop his code.
    let bob = app.add_user("bob", Some(TEMP)).await;
    let code = app.code_for(bob).await;
    for i in 0..8 {
        let mut b = Browser::at(&format!("10.23.2.{i}:1"));
        app.post(
            &mut b,
            "/login",
            &[("username", "bob"), ("password", "not the password")],
        )
        .await;
    }
    assert!(!app.signs_in("bob", TEMP).await, "the sign-in waits");
    let mut b = Browser::at("10.23.3.1:1");
    let res = app.redeem(&mut b, "bob", &code, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);

    // A name of 300 000 characters is one more wrong name, with the same
    // answer.
    let huge = "A".repeat(300_000);
    let mut b = Browser::at("10.23.4.1:1");
    for _ in 0..2 {
        let res = app.redeem(&mut b, &huge, &wrong, NEW_PASSWORD).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED);
        assert!(res.body.contains("don’t match, or the code has expired"));
    }
}

#[tokio::test]
async fn the_log_never_holds_what_was_typed_as_a_user_name() {
    let (app, _admin) = App::set_up().await;
    let target = app.add_user("target", None).await;
    let code = app.code_for(target).await;
    let mut b = Browser::at("10.24.0.1:1");
    // The code pasted into the user name field, and a name that tries to
    // start a log line of its own.
    let res = app.redeem(&mut b, &code, &code, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    let forged = "mallory\nFORGED-LINE sign-in code redeemed user=ada";
    let res = app.redeem(&mut b, forged, &code, NEW_PASSWORD).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    for _ in 0..12 {
        app.redeem(&mut b, "throttled-name", &code, NEW_PASSWORD)
            .await;
    }
    let log = logs();
    assert!(log.contains("failed sign-in code"), "{log}");
    assert!(log.contains("sign-in code attempts throttled"), "{log}");
    assert!(!log.contains(&code), "the code is in the log");
    assert!(
        !log.contains(&code.replace('-', "")),
        "the code is in the log"
    );
    assert!(!log.contains("FORGED-LINE"), "{log}");
    assert!(!log.contains("mallory"), "{log}");
    assert!(!log.contains("throttled-name"), "{log}");
    assert!(log.contains("user_ref="), "{log}");
}
