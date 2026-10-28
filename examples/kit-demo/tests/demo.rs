//! Kit Demo driven in-process: setup, sign-in with two factors, the kit's
//! pages and API reference under the demo's name and in its words, the
//! demo's token scopes, the General page, the sign-in throttle per
//! instance, static files, versions, the schema and the translations;
//! and the building blocks a second product needs (a second database,
//! CSRF patterns, token buckets, header policies, live updates, provider
//! forms and test messages, the design system's data components).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use vole_kit::auth::{Role, TokenScope, tokens, totp};
use vole_kit::db;
use vole_kit::state::Core;

const ADMIN: &str = "ada";
const PASSWORD: &str = "Demo-Owner-Pass-2026!";
const SECRET: &str = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP";

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

struct App {
    core: Core,
    demo: kit_demo::Demo,
    router: Router,
    cookies: BTreeMap<String, String>,
    dir: PathBuf,
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

static N: AtomicUsize = AtomicUsize::new(0);

impl App {
    async fn fresh() -> App {
        let dir = std::env::temp_dir().join(format!(
            "kit-demo-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let demo = kit_demo::start(&dir).await.unwrap();
        App {
            router: kit_demo::router(&demo),
            core: demo.core.clone(),
            demo,
            cookies: BTreeMap::new(),
            dir,
        }
    }

    /// After setup: the admin signed in.
    async fn set_up() -> App {
        let mut app = App::fresh().await;
        let res = app
            .post(
                "/setup",
                &[
                    ("username", ADMIN),
                    ("password", PASSWORD),
                    ("confirm", PASSWORD),
                    ("site_name", ""),
                    ("timezone", "UTC"),
                ],
            )
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
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
        for v in res.headers().get_all(header::SET_COOKIE) {
            let v = v.to_str().unwrap();
            let pair = v.split(';').next().unwrap();
            if let Some((k, val)) = pair.split_once('=') {
                if val.is_empty() || v.contains("Max-Age=0") {
                    self.cookies.remove(k);
                } else {
                    self.cookies.insert(k.into(), val.into());
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

    async fn bearer(&mut self, method: &str, path: &str, token: &str, json: Option<Value>) -> Res {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::CONTENT_TYPE, "application/json");
        let body = json.map_or_else(Body::empty, |j| Body::from(j.to_string()));
        let saved = std::mem::take(&mut self.cookies);
        let res = self.send(req.body(body).unwrap()).await;
        self.cookies = saved;
        res
    }

    /// A write token of the admin, made the way the Tokens page makes one.
    async fn token(&self) -> String {
        self.token_of(1, TokenScope::Write, &[]).await
    }

    /// A token of user `user_id` with `access` and product `scopes`.
    async fn token_of(&self, user_id: i64, access: TokenScope, scopes: &[&'static str]) -> String {
        let token = tokens::generate();
        let (hash, prefix) = (tokens::hash(&token), tokens::display_prefix(&token));
        let scopes = scopes.to_vec();
        self.core
            .with_conn(move |c| {
                let id = db::api_tokens::insert(c, user_id, "test", &prefix, &hash, access, None)?;
                for scope in scopes {
                    db::api_tokens::add_scope(c, id, scope)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        token
    }

    /// A user with `role` (no password anyone knows).
    async fn user(&self, name: &'static str, role: Role) -> i64 {
        self.core
            .with_conn(move |c| db::users::insert_user(c, name, "x", role))
            .await
            .unwrap()
    }
}

/// The token a page shows once.
fn shown_token(html: &str) -> String {
    html.match_indices("kd_")
        .map(|(at, _)| {
            html[at + 3..]
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect::<String>()
        })
        .find(|body| body.len() == 43)
        .map(|body| format!("kd_{body}"))
        .expect("a token on the page")
}

/// Branding checks for every page a product gets from the kit.
fn assert_demo_branded(path: &str, html: &str) {
    assert!(html.contains("Kit Demo"), "{path}: no product name\n{html}");
    assert_no_foreign_words(path, html);
}

/// No other product's names or words. The demo itself never says
/// "monitor" or "dashboard", so any that shows up came from the kit.
fn assert_no_foreign_words(path: &str, text: &str) {
    for foreign in ["Vigilant", "Vole", "vv_", "VV_", "vva_"] {
        assert!(!text.contains(foreign), "{path}: contains {foreign:?}");
    }
    let lower = text.to_lowercase();
    for word in [
        "vigilant",
        "vole",
        "monitor",
        "dashboard",
        "status page",
        "heartbeat",
        "agent",
        "settings → general",
        "localhost:3001",
    ] {
        if let Some(at) = lower.find(word) {
            let start = lower.floor_char_boundary(at.saturating_sub(80));
            let end = lower.ceil_char_boundary((at + 80).min(lower.len()));
            panic!("{path}: contains {word:?}: …{}…", &lower[start..end]);
        }
    }
}

#[tokio::test]
async fn setup_creates_the_first_admin() {
    let mut app = App::fresh().await;
    // No users yet: everything leads to setup.
    assert_eq!(app.get("/").await.location(), "/setup");
    assert_eq!(app.get("/login").await.location(), "/setup");
    let page = app.get("/setup").await;
    assert_eq!(page.status, StatusCode::OK);
    assert_demo_branded("/setup", &page.body);
    // The texts the kit's setup page takes from the product.
    assert!(page.body.contains("placeholder=\"Team notes…\""));
    assert!(!page.body.contains("You can change this in"));

    let res = app
        .post(
            "/setup",
            &[
                ("username", ADMIN),
                ("password", PASSWORD),
                ("confirm", PASSWORD),
                ("site_name", ""),
                ("timezone", "Europe/Oslo"),
            ],
        )
        .await;
    assert_eq!((res.status, res.location()), (StatusCode::SEE_OTHER, "/"));
    assert!(app.cookies.contains_key("kd_session"), "{:?}", app.cookies);
    assert!(app.cookies.keys().all(|k| k.starts_with("kd_")));

    let home = app.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(home.body.contains("Welcome to Kit Demo. You have 0 notes."));
    assert!(
        home.body.contains("Write your first note."),
        "{}",
        home.body
    );
    assert_demo_branded("/", &home.body);

    let (users, admins, site, tz) = app
        .core
        .with_conn(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))?,
                c.query_row("SELECT COUNT(*) FROM users WHERE role = 'admin'", [], |r| {
                    r.get::<_, i64>(0)
                })?,
                db::settings::get(c, db::settings::SITE_NAME)?,
                db::settings::get(c, db::settings::TIMEZONE)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((users, admins), (1, 1));
    assert_eq!(site.as_deref(), Some("Kit Demo"));
    assert_eq!(tz.as_deref(), Some("Europe/Oslo"));
    // Setup is done: it now leads to sign-in.
    app.cookies.clear();
    assert_eq!(app.get("/setup").await.location(), "/login");
}

#[tokio::test]
async fn login_and_the_second_factor() {
    let mut app = App::set_up().await;
    app.core
        .with_conn(|c| {
            db::users::set_pending_totp_secret(c, 1, SECRET)?;
            db::users::enable_totp(c, 1, 0, &[])
        })
        .await
        .unwrap();
    let res = app.post("/logout", &[]).await;
    assert_eq!(res.location(), "/login");
    assert!(!app.cookies.contains_key("kd_session"));

    let login = app.get("/login").await;
    assert_eq!(login.status, StatusCode::OK);
    assert_demo_branded("/login", &login.body);
    assert!(login.body.contains("Log in to see your notes."));
    let wrong = app
        .post("/login", &[("username", ADMIN), ("password", "nope")])
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);

    let res = app
        .post("/login", &[("username", ADMIN), ("password", PASSWORD)])
        .await;
    assert_eq!(res.location(), "/login/2fa");
    assert!(app.cookies.contains_key("kd_2fa"));
    assert!(!app.cookies.contains_key("kd_session"));
    let step = app.get("/login/2fa").await;
    assert_eq!(step.status, StatusCode::OK);
    assert_demo_branded("/login/2fa", &step.body);

    let bad = app.post("/login/2fa", &[("code", "000000")]).await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    let t = totp::build(SECRET, ADMIN, "Kit Demo").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let res = app
        .post("/login/2fa", &[("code", &totp::code_at(&t, now))])
        .await;
    assert_eq!((res.status, res.location()), (StatusCode::SEE_OTHER, "/"));
    assert!(app.cookies.contains_key("kd_session"));
    assert_eq!(app.get("/").await.status, StatusCode::OK);
}

#[tokio::test]
async fn the_kits_pages_carry_the_demos_name() {
    let mut app = App::set_up().await;
    for path in [
        "/",
        "/settings/general",
        "/settings/security",
        "/settings/users",
        "/settings/tokens",
        "/api-docs",
    ] {
        let res = app.get(path).await;
        assert_eq!(res.status, StatusCode::OK, "{path}: {}", res.body);
        assert_demo_branded(path, &res.body);
    }
    let docs = app.get("/api-docs").await.body;
    assert!(docs.contains(r#"download="kit-demo-openapi.json""#));
    assert!(docs.contains("Bearer kd_…"));
    assert!(docs.contains("/api/v1/notes"));
    // The kit's generic error codes on the page; the demo's only addition,
    // `rate_limited`, is in the full reference.
    assert!(docs.contains("<code>conflict</code> 409, <code>validation_failed</code> 422."));
    assert!(!docs.contains("delivery_failed"));
    let tokens = app.get("/settings/tokens").await.body;
    assert!(tokens.contains("Let scripts read or change your notes."));
    let tabs = app.get("/settings/users").await.body;
    assert!(tabs.contains("Editors can also write and delete them."));
    for tab in [
        "/settings/general",
        "/settings/users",
        "/settings/security",
        "/settings/tokens",
    ] {
        assert!(tabs.contains(&format!("href=\"{tab}\"")), "{tab}");
    }

    // A token is made with the demo's prefix and shown once.
    let res = app
        .post(
            "/settings/tokens",
            &[("name", "CI"), ("scope", "read"), ("expires", "30")],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Bearer kd_"), "{}", res.body);
    assert!(!res.body.contains("/metrics"));

    // Errors use the demo's icon and words.
    let missing = app.get("/no-such-page").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert!(missing.body.contains("/static/logo.svg"));
    assert!(missing.body.contains("Back to your notes"));
    assert_no_foreign_words("/no-such-page", &missing.body);
}

/// The API reference in every form: the demo's words and the kit's generic
/// parts, nothing of Vigilant Vole's.
#[tokio::test]
async fn the_api_reference_is_the_demos_own() {
    let mut app = App::set_up().await;
    let token = app.token().await;
    for path in ["/api-docs.md", "/llms.txt"] {
        let res = app.get(path).await;
        assert_eq!(res.status, StatusCode::OK, "{path}");
        assert_no_foreign_words(path, &res.body);
        assert!(res.body.starts_with("# Kit Demo API"), "{path}");
        assert!(res.body.contains("http://demo.test"), "{path}");
        assert!(
            res.body.contains(
                "| `conflict` | 409 | clashes with existing data or the object's state |"
            )
        );
        for code in ["too_large", "invalid_document", "delivery_failed"] {
            assert!(!res.body.contains(code), "{path}: {code}");
        }
        assert!(res.body.find("`validation_failed`") < res.body.find("`internal`"));
    }

    let res = app
        .bearer("GET", "/api/v1/openapi.json", &token, None)
        .await;
    assert_no_foreign_words("/api/v1/openapi.json", &res.body);
    let doc: Value = serde_json::from_str(&res.body).unwrap();
    let schemes: Vec<&String> = doc["components"]["securitySchemes"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(schemes, ["bearerAuth", "ingestKey", "sessionCookie"]);

    // Without a Host header or a public URL: the demo's own address.
    assert_eq!(
        vole_kit::docs::instance_url(None, &HeaderMap::new()),
        "http://127.0.0.1:3101"
    );
}

/// The MCP endpoint refuses a foreign browser origin without pointing to a
/// settings page the demo doesn't have.
#[tokio::test]
async fn mcp_refuses_a_foreign_origin_in_neutral_words() {
    let mut app = App::set_up().await;
    let token = app.token().await;
    let req = Request::post("/mcp")
        .header(header::ORIGIN, "http://evil.example")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
        .unwrap();
    let saved = std::mem::take(&mut app.cookies);
    let res = app.send(req).await;
    app.cookies = saved;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert!(
        res.body
            .contains("Browser clients must use the instance's public URL."),
        "{}",
        res.body
    );
    assert_no_foreign_words("/mcp", &res.body);
}

/// Every language the demo ships translates all of its strings, checked
/// with the kit's helper.
#[test]
fn every_language_the_demo_ships_is_complete() {
    use vole_kit::i18n::{Lang, catalog};
    kit_demo::install();
    catalog::assert_full_coverage();
    assert!(catalog::coverage(Lang::NB).is_complete());
    // A language it doesn't ship lacks its strings (they fall back to English).
    let gaps = catalog::coverage(Lang::from_code("de").unwrap()).problems;
    for key in ["demo.notes", "error.back"] {
        assert!(gaps.contains(&format!("missing {key}")), "{gaps:?}");
    }
}

#[tokio::test]
async fn the_design_system_and_own_files_are_served() {
    let mut app = App::fresh().await;
    for (path, kind) in [
        ("/static/style.css", "text/css"),
        ("/static/app.js", "javascript"),
        ("/static/icons.svg", "image/svg+xml"),
        ("/static/logo.svg", "image/svg+xml"),
    ] {
        let res = app.get(path).await;
        assert_eq!(res.status, StatusCode::OK, "{path}");
        let content_type = res.headers[header::CONTENT_TYPE].to_str().unwrap();
        assert!(content_type.contains(kind), "{path}: {content_type}");
    }
    assert_eq!(
        app.get("/static/favicon.svg").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn api_documents_and_mcp_report_the_demos_version() {
    let mut app = App::set_up().await;
    let token = app.token().await;
    assert!(token.starts_with("kd_"));

    let res = app
        .bearer("GET", "/api/v1/openapi.json", &token, None)
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let doc: Value = serde_json::from_str(&res.body).unwrap();
    // 0.3.7: neither Vigilant Vole's version nor the kit's.
    assert_eq!(kit_demo::VERSION, "0.3.7");
    assert_eq!(doc["info"]["version"], kit_demo::VERSION);
    assert_eq!(doc["info"]["title"], "Kit Demo API");

    let res = app
        .bearer(
            "POST",
            "/mcp",
            &token,
            Some(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": "2025-06-18"},
            })),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let reply: Value = serde_json::from_str(&res.body).unwrap();
    let info = &reply["result"]["serverInfo"];
    assert_eq!(info["version"], kit_demo::VERSION);
    assert_eq!(info["name"], "kit-demo");
    assert_eq!(info["title"], "Kit Demo");

    let notes = app.bearer("GET", "/api/v1/notes", &token, None).await;
    assert_eq!(notes.body, r#"{"notes":[]}"#);
    // The Markdown reference, in the signed-in browser like VV's.
    let md = app.get("/api-docs.md").await;
    assert!(
        md.body.starts_with("# Kit Demo API"),
        "{}: {}",
        md.status,
        md.body
    );
}

#[tokio::test]
async fn a_fresh_database_has_the_core_tables_and_the_demos() {
    let app = App::fresh().await;
    let (tables, applied) = app
        .core
        .with_conn(|c| {
            let tables: Vec<String> = c
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table'
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let applied: Vec<String> = c
                .prepare("SELECT name FROM schema_migrations ORDER BY rowid")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            Ok((tables, applied))
        })
        .await
        .unwrap();
    assert_eq!(
        tables,
        [
            "api_token_scopes",
            "api_tokens",
            "notes",
            "passkey_challenges",
            "passkeys",
            "push_devices",
            "push_events",
            "schema_migrations",
            "settings",
            "users"
        ]
    );
    assert_eq!(
        applied,
        [
            "kit/0001_core.sql",
            "kit/0002_token_scopes.sql",
            "kit/0003_passkeys.sql",
            "kit/0004_sealed_secrets.sql",
            "kit/0005_sealed_totp.sql",
            "kit/0006_push.sql",
            "0001_notes.sql"
        ]
    );

    // Starting again changes nothing and takes no backup.
    let report = migrate_again(&app).await;
    assert!(report.applied.is_empty() && report.backup.is_none());
}

async fn migrate_again(app: &App) -> vole_kit::db::MigrationReport {
    vole_kit::db::migrate(&app.core.db, &app.dir.join("backups"))
        .await
        .unwrap()
}

/// Provider forms name the demo, never Vigilant Vole.
#[test]
fn notification_forms_name_the_demo() {
    kit_demo::install();
    let mut shown = Vec::new();
    for p in vole_kit::notify::PROVIDERS {
        for f in p.fields {
            let text = format!("{} {}", f.placeholder_text(), f.default_value());
            assert!(
                !text.contains("Vigilant") && !text.contains("Vole"),
                "{}.{}: {text}",
                p.kind,
                f.key
            );
            if text.contains("Demo") {
                shown.push(format!("{}.{}", p.kind, f.key));
            }
        }
    }
    assert!(shown.contains(&"slack.username".to_string()), "{shown:?}");
    assert!(shown.contains(&"zulip.topic".to_string()), "{shown:?}");
    assert!(shown.contains(&"clicksend.from".to_string()), "{shown:?}");
}

// ---------------------------------------------------------------------------
// The building blocks a second product needs
// ---------------------------------------------------------------------------

impl App {
    /// A request as a program sends it: no cookie, no `Origin`.
    async fn machine_post(&mut self, path: &str, headers: &[(&str, &str)], body: &str) -> Res {
        let mut req = Request::post(path);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let saved = std::mem::take(&mut self.cookies);
        let res = self
            .send(req.body(Body::from(body.to_string())).unwrap())
            .await;
        self.cookies = saved;
        res
    }

    async fn envelope(&mut self, project: &str) -> Res {
        self.machine_post(
            &format!("/api/{project}/envelope/"),
            &[("x-demo-key", kit_demo::INGEST_KEY)],
            "{\"event\": \"signup\"}\n",
        )
        .await
    }

    /// An htmx request from the demo's own pages.
    async fn hx_get(&mut self, path: &str) -> Res {
        let req = Request::get(path)
            .header("hx-request", "true")
            .body(Body::empty())
            .unwrap();
        self.send(req).await
    }
}

/// The next event of a server-sent event stream (up to its blank line).
async fn next_event(body: &mut axum::body::BodyDataStream) -> String {
    use futures_util::StreamExt as _;
    let mut text = String::new();
    while !text.contains("\n\n") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .expect("an event within 5 s")
            .expect("the stream is open")
            .unwrap();
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    text
}

#[tokio::test]
async fn the_store_is_a_second_database_with_its_own_migrations() {
    let app = App::fresh().await;
    let (tables, applied, vacuum) = vole_kit::db::interact(&app.demo.store, |c| {
        let tables: Vec<String> = c
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table'
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let applied: Vec<String> = c
            .prepare("SELECT name FROM schema_migrations ORDER BY rowid")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let vacuum: i64 = c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
        Ok((tables, applied, vacuum))
    })
    .await
    .unwrap();
    assert_eq!(tables, ["envelopes", "schema_migrations"], "no core tables");
    assert_eq!(applied, ["0001_envelopes.sql"]);
    assert_eq!(vacuum, 2, "incremental auto-vacuum from the first table on");

    // Opening it again applies nothing and never takes a copy.
    let report = vole_kit::db::migrate_with(
        &app.demo.store,
        &kit_demo::STORE_MIGRATIONS,
        vole_kit::db::MigrateOptions::default(),
    )
    .await
    .unwrap();
    assert!(report.applied.is_empty() && report.backup.is_none());

    // A store from a newer release is refused without pointing at backups.
    vole_kit::db::interact(&app.demo.store, |c| {
        c.execute(
            "INSERT INTO schema_migrations (name) VALUES ('0002_future.sql')",
            [],
        )
    })
    .await
    .unwrap();
    let err = kit_demo::open_store(&app.dir)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("newer Kit Demo (schema 0002_future)"), "{err}");
    assert!(!err.contains("backup"), "{err}");
    assert!(!app.dir.join("backups").exists());
}

#[tokio::test]
async fn ingest_is_csrf_exempt_rate_limited_and_answered_as_a_machine() {
    let mut app = App::set_up().await;
    // No cookie, no Origin: exempt by the `/api/*/envelope` pattern.
    let res = app.envelope("notes").await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    assert_eq!(
        res.headers[header::CONTENT_SECURITY_POLICY],
        vole_kit::http::headers::MACHINE_CSP
    );
    assert!(res.headers.get(header::X_FRAME_OPTIONS).is_none());
    assert_eq!(res.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

    // The pattern is one segment wide: other paths still need the proof.
    let other = app.machine_post("/api/notes/envelopes/", &[], "{}").await;
    assert_eq!(other.status, StatusCode::FORBIDDEN, "{}", other.body);

    // The key is the endpoint's own credential.
    let keyless = app.machine_post("/api/notes/envelope/", &[], "{}").await;
    assert_eq!(keyless.status, StatusCode::UNAUTHORIZED);

    // Five a minute per address; the sixth waits.
    for _ in 1..kit_demo::INGEST_RATE.burst {
        assert_eq!(app.envelope("notes").await.status, StatusCode::ACCEPTED);
    }
    let limited = app.envelope("notes").await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers[header::RETRY_AFTER], "12");
    let body: Value = serde_json::from_str(&limited.body).unwrap();
    assert_eq!(body["error"]["code"], "rate_limited");

    // The snippet other sites load: no CSP or framing rules, CORP open.
    let js = app.get("/js/d.js").await;
    assert_eq!(js.status, StatusCode::OK);
    assert!(js.headers.get(header::CONTENT_SECURITY_POLICY).is_none());
    assert!(js.headers.get(header::X_FRAME_OPTIONS).is_none());
    assert_eq!(js.headers["cross-origin-resource-policy"], "cross-origin");
    // The demo's pages keep the app's headers.
    let home = app.get("/").await;
    assert_eq!(
        home.headers[header::CONTENT_SECURITY_POLICY],
        vole_kit::http::headers::APP_CSP
    );
    assert_eq!(home.headers[header::X_FRAME_OPTIONS], "DENY");
}

#[tokio::test]
async fn the_live_view_merges_a_burst_into_one_update() {
    let mut app = App::set_up().await;
    let cookie = app
        .cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut req = Request::get("/projects/notes/live")
        .header(header::HOST, "demo.test")
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
    ));
    let res = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
    let mut body = res.into_body().into_data_stream();

    // Another project's envelope doesn't wake it; three of its own do, in
    // as few updates as quiet periods separate them.
    assert_eq!(app.envelope("other").await.status, StatusCode::ACCEPTED);
    for _ in 0..3 {
        assert_eq!(app.envelope("notes").await.status, StatusCode::ACCEPTED);
    }
    let mut events = Vec::new();
    loop {
        let event = next_event(&mut body).await;
        let done = event.contains("3 envelopes");
        events.push(event);
        if done {
            break;
        }
        assert!(events.len() < 3, "{events:?}");
    }
    let last = events.last().unwrap();
    assert!(last.starts_with("event: envelopes-updated\n"), "{last}");
    assert!(
        last.contains("<p id=\"envelopes\">3 envelopes</p>"),
        "{last}"
    );
}

/// A server that answers one request and returns its body.
async fn capture_one() -> (u16, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if body.len() >= length {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                        .await
                        .unwrap();
                    return body.to_string();
                }
            }
            assert!(n > 0, "the client hung up: {text}");
        }
    });
    (port, server)
}

#[tokio::test]
async fn provider_forms_and_test_messages_are_the_kits_in_the_demos_words() {
    let mut app = App::set_up().await;
    let picker = app.hx_get("/settings/notifications/providers").await;
    assert_eq!(picker.status, StatusCode::OK, "{}", picker.body);
    assert!(picker.body.contains("id=\"provider-grid\""));
    assert!(picker.body.contains("id=\"cat-chat\""));
    let grid = app
        .hx_get("/settings/notifications/providers?grid=true&q=disc")
        .await;
    assert!(grid.body.contains("value=\"discord\""));
    assert!(!grid.body.contains("value=\"slack\""));

    let form = app
        .hx_get("/settings/notifications/form?kind=webhook")
        .await;
    assert_eq!(form.status, StatusCode::OK, "{}", form.body);
    assert!(
        form.body.contains("placeholder=\"Webhook alerts…\""),
        "the kit's name field"
    );
    assert!(
        form.body
            .contains("placeholder=\"https://example.com/hooks/notes…\"")
    );
    assert!(
        form.body
            .contains("{{note_name}}, {{note_id}}, {{note_type}}")
    );
    assert!(form.body.contains("{{duration}}, {{duration_seconds}}"));
    assert!(form.body.contains("class=\"kv-editor\""));
    assert_no_foreign_words("/settings/notifications/form", &form.body);

    // "Send test" to a webhook, in the language the form asks for.
    let (port, server) = capture_one().await;
    let url = format!("http://127.0.0.1:{port}/hook");
    let req = Request::post("/settings/notifications/test")
        .header("hx-request", "true")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([
                    ("kind", "webhook"),
                    ("c_url", url.as_str()),
                    ("language", "nb"),
                ])
                .finish(),
        ))
        .unwrap();
    let res = app.send(req).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let toast = res.headers["hx-trigger"].to_str().unwrap();
    assert!(toast.contains("\"success\""), "{toast}");
    let sent: Value = serde_json::from_str(&server.await.unwrap()).unwrap();
    assert_eq!(sent["note"], "Testmelding fra Kit Demo");
    assert_eq!(sent["message"], "Kan du lese dette, virker kanalen.");
    assert_eq!(sent["status"], "test");
    assert!(sent.get("note_id").is_some() && sent.get("duration_seconds").is_some());
    assert!(sent.get("monitor").is_none());
}

#[tokio::test]
async fn the_api_reference_types_parameters_bodies_and_header_keys() {
    let mut app = App::set_up().await;
    assert!(vole_kit::docs::undeclared_access(kit_demo::KIT.docs).is_empty());
    let token = app.token().await;
    let res = app
        .bearer("GET", "/api/v1/openapi.json", &token, None)
        .await;
    let doc: Value = serde_json::from_str(&res.body).unwrap();
    let op = &doc["paths"]["/api/{project}/envelope/"]["post"];
    assert_eq!(op["parameters"][0]["name"], "project");
    assert_eq!(op["parameters"][0]["schema"]["type"], "string");
    assert_eq!(
        op["requestBody"]["content"]["application/x-ndjson"]["example"],
        "{\"event\": \"signup\"}\n{\"event\": \"login\"}"
    );
    assert_eq!(op["security"], json!([{"ingestKey": []}]));
    assert_eq!(
        doc["components"]["securitySchemes"]["ingestKey"],
        json!({"type": "apiKey", "in": "header", "name": "X-Demo-Key", "description": "Ingest key"})
    );

    let md = app.get("/api-docs.md").await.body;
    assert!(md.contains("- Access: the ingest key in `X-Demo-Key` (no API token)"));
    assert!(md.contains("- Path parameters: `project` (string)"));
    assert!(md.contains("- Body: `application/x-ndjson` (see the example)"));
    let curl = concat!(
        "curl -X POST -H \"X-Demo-Key: $KD_INGEST_KEY\" ",
        "-H \"Content-Type: application/x-ndjson\" \"$KD_URL/api/{project}/envelope/\" ",
        "--data-binary '{\"event\": \"signup\"}\n{\"event\": \"login\"}'"
    );
    assert!(md.contains(curl), "{md}");
    assert!(md.contains("| `rate_limited` | 429 |"));
    assert!(md.contains(
        "### GET /js/d.js\n\nThe script a page embeds; any site may load it.\n\n- Access: public (no token)"
    ));
}

#[tokio::test]
async fn the_home_page_draws_bars_lines_and_code_from_the_design_system() {
    let mut app = App::set_up().await;
    for project in ["notes", "notes", "shop"] {
        assert_eq!(app.envelope(project).await.status, StatusCode::ACCEPTED);
    }
    let home = app.get("/").await;
    assert_eq!(home.status, StatusCode::OK, "{}", home.body);
    assert_demo_branded("/", &home.body);
    let html = &home.body;
    // The bar list: the busiest project first, as wide as its share.
    assert!(html.contains("<ul class=\"bars\" role=\"list\" aria-label=\"Projects\">"));
    let notes = html.find("href=\"/projects/notes\">notes</a>").unwrap();
    let shop = html.find("href=\"/projects/shop\">shop</a>").unwrap();
    assert!(notes < shop);
    assert!(html.contains("style=\"--share: 66.67%\""), "{html}");
    assert!(html.contains("<span class=\"bar-num\">67%</span>"));
    // Two lines on one scale, the first with its area.
    assert!(html.contains("class=\"chart-area series-1\""));
    assert!(html.contains("class=\"chart-line series-2\""));
    assert!(html.contains("<span class=\"legend-swatch series-2\"></span><span>Envelopes</span>"));
    assert!(html.contains("aria-label=\"Last 7 days\""));
    // Code with the second line highlighted.
    assert!(html.contains(
        "<span class=\"code-line is-current\" aria-current=\"true\"><span class=\"code-no\" aria-hidden=\"true\">2</span>"
    ));
    // Sizes and shares in the reader's words.
    assert!(html.contains(" KB; "), "{html}");
}

/// The demo's scopes on the kit's Tokens page: offered with their texts,
/// stored with the token and shown as its access.
#[tokio::test]
async fn product_scopes_are_offered_and_shown_on_the_tokens_page() {
    let mut app = App::set_up().await;
    let page = app.get("/settings/tokens").await.body;
    for (value, label, help) in [
        (
            "notes:add",
            "Read and add notes",
            "Reads everything and adds notes",
        ),
        ("inbox", "Inbox only", "can’t read anything"),
    ] {
        assert!(
            page.contains(&format!(
                r#"<input type="radio" name="scope" value="{value}">"#
            )),
            "{value}"
        );
        assert!(page.contains(label) && page.contains(help), "{value}");
    }
    let res = app
        .post(
            "/settings/tokens",
            &[
                ("name", "Importer"),
                ("scope", "notes:add"),
                ("expires", "30"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let token = shown_token(&res.body);
    assert_eq!(token.len(), 3 + 43, "{token}");
    let list = app.get("/settings/tokens").await.body;
    assert!(
        list.contains(r#"<span class="badge">Read and add notes</span>"#),
        "{list}"
    );
    let (access, scopes) = app
        .core
        .with_conn(|c| {
            let t = db::api_tokens::list_all(c)?.remove(0);
            Ok((t.scope, db::api_tokens::scopes_of(c, t.id)?))
        })
        .await
        .unwrap();
    assert_eq!(
        (access, scopes),
        (TokenScope::Read, vec!["notes:add".into()])
    );

    // The token reads and adds notes, and changes nothing else.
    let notes = app.bearer("GET", "/api/v1/notes", &token, None).await;
    assert_eq!(notes.status, StatusCode::OK);
    let add = app
        .bearer(
            "POST",
            "/api/v1/notes",
            &token,
            Some(json!({"body": "Milk"})),
        )
        .await;
    assert_eq!(add.status, StatusCode::CREATED, "{}", add.body);
    let inbox = app
        .bearer(
            "POST",
            "/api/v1/inbox",
            &token,
            Some(json!({"body": "Milk"})),
        )
        .await;
    assert_eq!(inbox.status, StatusCode::FORBIDDEN);

    // An unknown scope is refused.
    let res = app
        .post(
            "/settings/tokens",
            &[("name", "X"), ("scope", "notes:delete"), ("expires", "30")],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let page = app.get("/settings/tokens").await.body;
    assert_eq!(page.matches("<tr id=\"token-").count(), 1, "{page}");
}

/// What a request may do with each kind of token: `has_scope` and the
/// owner's own role (`may_act_as`) decide, as in Keen Kestrel's upload
/// check.
#[tokio::test]
async fn scopes_and_the_owners_role_decide_together() {
    let mut app = App::set_up().await;
    let editor = app.user("eve", Role::Editor).await;
    let viewer = app.user("vic", Role::Viewer).await;
    let add = || Some(json!({"body": "Milk"}));
    let cases: [(i64, TokenScope, &[&'static str], StatusCode); 5] = [
        // Read and write: every scope, the role decides.
        (editor, TokenScope::Write, &[], StatusCode::CREATED),
        (viewer, TokenScope::Write, &[], StatusCode::FORBIDDEN),
        // Read only: exactly as before, no scope.
        (editor, TokenScope::Read, &[], StatusCode::FORBIDDEN),
        // Read only with the scope: the owner's role counts, not the cap.
        (
            editor,
            TokenScope::Read,
            &["notes:add"],
            StatusCode::CREATED,
        ),
        (
            viewer,
            TokenScope::Read,
            &["notes:add"],
            StatusCode::FORBIDDEN,
        ),
    ];
    for (who, access, scopes, expected) in cases {
        let token = app.token_of(who, access, scopes).await;
        let res = app.bearer("POST", "/api/v1/notes", &token, add()).await;
        assert_eq!(res.status, expected, "{who} {access:?} {scopes:?}");
        let read = app.bearer("GET", "/api/v1/notes", &token, None).await;
        assert_eq!(read.status, StatusCode::OK, "{who} {access:?} {scopes:?}");
    }
}

/// A scope that doesn't imply read: the token works under the scope's path
/// and is refused everywhere else, MCP included.
#[tokio::test]
async fn a_scope_without_read_works_only_under_its_paths() {
    let mut app = App::set_up().await;
    let editor = app.user("eve", Role::Editor).await;
    let token = app.token_of(editor, TokenScope::Read, &["inbox"]).await;
    let res = app
        .bearer(
            "POST",
            "/api/v1/inbox",
            &token,
            Some(json!({"body": "Milk"})),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    for (method, path) in [
        ("GET", "/api/v1/notes"),
        ("POST", "/api/v1/notes"),
        ("GET", "/api/v1/openapi.json"),
    ] {
        let res = app
            .bearer(method, path, &token, Some(json!({"body": "Milk"})))
            .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    let mcp = app
        .bearer(
            "POST",
            "/mcp",
            &token,
            Some(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})),
        )
        .await;
    assert_eq!(mcp.status, StatusCode::FORBIDDEN, "{}", mcp.body);
    assert!(mcp.body.contains("can't read"), "{}", mcp.body);
    // Not for reading by token outside require_auth either.
    assert!(
        vole_kit::auth::authenticate_token(&app.core, &token)
            .await
            .unwrap()
            .is_none()
    );

    // A token whose only scope the product no longer declares stops
    // working instead of becoming a read-only token.
    let gone = app
        .token_of(editor, TokenScope::Read, &["gone:scope"])
        .await;
    let res = app.bearer("GET", "/api/v1/notes", &gone, None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

/// The kit's General page: the core settings and the demo's own section,
/// validated, stored, and followed by the demo's hook.
#[tokio::test]
async fn the_general_page_saves_core_and_product_settings() {
    let mut app = App::set_up().await;
    let page = app.get("/settings/general").await;
    assert_eq!(page.status, StatusCode::OK);
    assert_demo_branded("/settings/general", &page.body);
    for text in [
        "How this instance names itself and tells time, and how it lists notes.",
        "Used for links to notes.",
        r#"name="site_name""#,
        r#"name="base_url""#,
        r#"id="f-timezone" name="timezone""#,
        r#"name="language""#,
        "Notes per page",
        r#"name="notes_per_page" type="number" inputmode="numeric" min="5" max="100" step="1" autocomplete="off" value="20""#,
        r#"name="notes_public" value="true">"#,
        "Team notes…",
    ] {
        assert!(page.body.contains(text), "{text}\n{}", page.body);
    }

    let bad = app
        .post(
            "/settings/general",
            &[
                ("site_name", "Ours"),
                ("base_url", "notes.example.com"),
                ("timezone", "Mars/Olympus"),
                ("language", "nb"),
                ("notes_per_page", "500"),
            ],
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    for text in [
        "Enter a full URL",
        "Choose a time zone from the list.",
        "Enter a number between 5 and 100.",
        r#"value="500""#,
    ] {
        assert!(bad.body.contains(text), "{text}");
    }
    let before = kit_demo::GENERAL_SAVES.load(Ordering::SeqCst);
    let ok = app
        .post(
            "/settings/general",
            &[
                ("site_name", "Ours"),
                ("base_url", "https://notes.example.com/"),
                ("timezone", "Europe/Oslo"),
                ("language", "nb"),
                ("notes_per_page", "50"),
                ("notes_public", "true"),
            ],
        )
        .await;
    assert_eq!(
        (ok.status, ok.location()),
        (StatusCode::SEE_OTHER, "/settings/general"),
        "{}",
        ok.body
    );
    assert!(kit_demo::GENERAL_SAVES.load(Ordering::SeqCst) > before);
    let (site, per_page, public) = app
        .core
        .with_conn(|c| {
            Ok((
                vole_kit::settings::SiteSettings::load(c)?,
                db::settings::get(c, "notes_per_page")?,
                db::settings::get(c, "notes_public")?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(site.site_name.as_deref(), Some("Ours"));
    assert_eq!(site.base_url.as_deref(), Some("https://notes.example.com"));
    assert_eq!(site.timezone.as_deref(), Some("Europe/Oslo"));
    assert_eq!(site.language.map(|l| l.code()), Some("nb"));
    assert_eq!(
        (per_page.as_deref(), public.as_deref()),
        (Some("50"), Some("1"))
    );
    assert_eq!(
        app.core.base_url().await.as_deref(),
        Some("https://notes.example.com")
    );

    // The next page is in the new site language, with the saved values.
    let page = app.get("/settings/general").await.body;
    assert!(page.contains("Innstillingene er lagret"), "{page}");
    assert!(page.contains(r#"lang="nb""#));
    assert!(page.contains(r#"value="50""#), "{page}");
    assert!(page.contains(r#"name="notes_public" value="true" checked>"#));

    // Admins only.
    let hash = vole_kit::auth::hash_password("Editor-Pass-2026!");
    app.core
        .with_conn(move |c| db::users::insert_user(c, "eve", &hash, Role::Editor))
        .await
        .unwrap();
    app.post("/logout", &[]).await;
    let res = app
        .post(
            "/login",
            &[("username", "eve"), ("password", "Editor-Pass-2026!")],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(
        app.get("/settings/general").await.status,
        StatusCode::FORBIDDEN
    );
    let res = app
        .post("/settings/general", &[("site_name", "Theirs")])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    vole_kit::settings::set_site_language(&app.core, None)
        .await
        .unwrap();
}

/// A product without the page sets the site language itself; the kit's
/// pages and messages outside a request follow it.
#[tokio::test]
async fn the_site_language_can_be_set_without_the_page() {
    let app = App::set_up().await;
    let nb = vole_kit::i18n::Lang::from_code("nb").unwrap();
    vole_kit::settings::set_site_language(&app.core, Some(nb))
        .await
        .unwrap();
    assert_eq!(vole_kit::i18n::site_language(&app.core).await, Some(nb));
    vole_kit::settings::set_site_language(&app.core, None)
        .await
        .unwrap();
    assert_eq!(vole_kit::i18n::site_language(&app.core).await, None);
}

/// Each running instance throttles sign-ins on its own: a test server that
/// is told to wait doesn't hold up another one in the same process.
#[tokio::test]
async fn sign_in_throttling_is_per_instance() {
    let mut first = App::set_up().await;
    let mut second = App::set_up().await;
    first.cookies.clear();
    second.cookies.clear();
    let mut throttled = false;
    for _ in 0..20 {
        let res = first
            .post("/login", &[("username", ADMIN), ("password", "wrong")])
            .await;
        if res.status == StatusCode::TOO_MANY_REQUESTS {
            throttled = true;
            break;
        }
    }
    assert!(throttled, "the first instance never throttled");
    let res = second
        .post("/login", &[("username", ADMIN), ("password", PASSWORD)])
        .await;
    assert_eq!((res.status, res.location()), (StatusCode::SEE_OTHER, "/"));
}

impl App {
    async fn post_json(&mut self, path: &str, body: &Value) -> Res {
        let req = Request::post(path)
            .header(header::ORIGIN, "http://demo.test")
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(req).await
    }

    /// `publicKey` of an options endpoint and its challenge.
    async fn passkey_options(&mut self, path: &str, form: &[(&str, &str)]) -> (Value, Vec<u8>) {
        let res = self.post(path, form).await;
        assert_eq!(res.status, StatusCode::OK, "{path}: {}", res.body);
        let options: Value = serde_json::from_str(&res.body).unwrap();
        let options = options["publicKey"].clone();
        let challenge =
            vole_kit::auth::webauthn::unb64(options["challenge"].as_str().unwrap(), "c").unwrap();
        assert_eq!(challenge.len(), 32);
        (options, challenge)
    }
}

/// A sign-in request body: the software authenticator's assertion for
/// `challenge` as the browser at `origin` would send it.
fn passkey_sign_in(
    auth: &mut vole_kit::auth::webauthn::soft::SoftAuthenticator,
    challenge: &[u8],
    origin: &str,
) -> Value {
    use vole_kit::auth::webauthn::b64;
    let (client, data, sig) = auth.assert("demo.test", origin, challenge);
    json!({
        "next": "/settings/tokens",
        "credential": {
            "id": b64(&auth.credential_id), "rawId": b64(&auth.credential_id), "type": "public-key",
            "response": {
                "clientDataJSON": b64(&client),
                "authenticatorData": b64(&data),
                "signature": b64(&sig),
                "userHandle": b64(&auth.user_handle),
            },
        },
    })
}

/// A passkey made with the software authenticator: registered under
/// Security after the password, then signing in on its own (no password,
/// no TOTP step, though the account has two-factor on). Challenges are
/// single use and bound to the public URL's origin.
#[tokio::test]
async fn passkeys_register_and_sign_in_without_password_or_code() {
    use vole_kit::auth::webauthn::{b64, soft::SoftAuthenticator, unb64};
    let mut app = App::set_up().await;
    app.core
        .with_conn(|c| {
            db::users::set_pending_totp_secret(c, 1, SECRET)?;
            db::users::enable_totp(c, 1, 0, &[])
        })
        .await
        .unwrap();

    // Without a public URL there are no passkeys to add, only the note.
    let page = app.get("/settings/security").await;
    assert!(
        page.body
            .contains("Passkeys need this site’s public URL over HTTPS.")
    );
    assert!(!page.body.contains("data-passkey-add"));
    let res = app
        .post(
            "/settings/security/passkeys/options",
            &[("password", PASSWORD)],
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    app.core
        .with_conn(|c| db::settings::set(c, "base_url", "https://demo.test"))
        .await
        .unwrap();
    let page = app.get("/settings/security").await;
    assert_demo_branded("/settings/security", &page.body);
    assert!(page.body.contains("data-passkey-add"));
    assert!(
        page.body
            .contains(r#"Passkeys work at <a href="https://demo.test/settings/security#passkeys">https://demo.test</a> over HTTPS. Open that address to add one."#)
    );
    assert!(
        page.body
            .contains(r#"<script src="/static/passkeys.js" defer></script>"#)
    );
    assert!(page.body.contains("No passkeys yet"));

    // Adding one needs the password.
    let res = app
        .post(
            "/settings/security/passkeys/options",
            &[("password", "wrong")],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let (options, challenge) = app
        .passkey_options(
            "/settings/security/passkeys/options",
            &[("password", PASSWORD)],
        )
        .await;
    assert_eq!(options["rp"]["id"], "demo.test");
    assert_eq!(options["rp"]["name"], "Kit Demo");
    assert_eq!(options["user"]["name"], ADMIN);
    assert_eq!(options["attestation"], "none");
    assert_eq!(options["authenticatorSelection"]["residentKey"], "required");
    assert_eq!(
        options["authenticatorSelection"]["userVerification"],
        "required"
    );
    assert_eq!(options["excludeCredentials"], json!([]));
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
                "transports": ["internal", "hybrid", "carrier-pigeon"],
            },
        },
    });
    // Another device that is signed in as the admin right now.
    let other_device = app.cookies.clone();
    let res = app.post_json("/settings/security/passkeys", &body).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, r##"{"redirect":"/settings/security#passkeys"}"##);
    // Adding a passkey signs the other devices out; this one stays signed in.
    let here = std::mem::replace(&mut app.cookies, other_device);
    let res = app.get("/settings/security").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(res.location().starts_with("/login"), "{}", res.location());
    app.cookies = here;
    // The same response again: its challenge is used up.
    let res = app.post_json("/settings/security/passkeys", &body).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert!(res.body.contains("The passkey couldn’t be saved."));

    let page = app.get("/settings/security").await;
    assert!(page.body.contains("Passkey added"));
    assert!(
        page.body
            .contains(r#"<span class="list-title">Passkey 1</span>"#)
    );
    assert!(page.body.contains("Never used"));
    let stored = app
        .core
        .with_conn(|c| db::passkeys::list_for_user(c, 1))
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].transports, "hybrid,internal");
    assert_eq!(stored[0].user_handle, handle);
    // A second one is offered the first to exclude, and the same handle.
    let (options, _) = app
        .passkey_options(
            "/settings/security/passkeys/options",
            &[("password", PASSWORD)],
        )
        .await;
    assert_eq!(options["excludeCredentials"][0]["id"], id.as_str());
    assert_eq!(options["user"]["id"], b64(&handle).as_str());

    // Signed out, the login page offers passkeys (autofill too).
    app.post("/logout", &[]).await;
    let login = app.get("/login").await;
    assert!(login.body.contains(r#"autocomplete="username webauthn""#));
    assert!(
        login
            .body
            .contains(r#"data-passkey-signin data-origin="https://demo.test""#)
    );

    // Another origin is refused, and so is the challenge it used.
    let (options, challenge) = app.passkey_options("/login/passkey/options", &[]).await;
    assert_eq!(options["rpId"], "demo.test");
    assert_eq!(options["userVerification"], "required");
    assert_eq!(options["allowCredentials"], json!([]));
    let wrong = passkey_sign_in(&mut auth, &challenge, "https://evil.test");
    let res = app.post_json("/login/passkey", &wrong).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(res.body.contains("That passkey didn’t work."));
    let right = passkey_sign_in(&mut auth, &challenge, "https://demo.test");
    let res = app.post_json("/login/passkey", &right).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(!app.cookies.contains_key("kd_session"));

    // The real thing: no password, no code.
    let (_, challenge) = app.passkey_options("/login/passkey/options", &[]).await;
    let body = passkey_sign_in(&mut auth, &challenge, "https://demo.test");
    let res = app.post_json("/login/passkey", &body).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, r#"{"redirect":"/settings/tokens"}"#);
    assert!(app.cookies.contains_key("kd_session"));
    assert!(!app.cookies.contains_key("kd_2fa"));
    assert_eq!(app.get("/settings/tokens").await.status, StatusCode::OK);
    // Replayed: the challenge is gone.
    app.post("/logout", &[]).await;
    let res = app.post_json("/login/passkey", &body).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(!app.cookies.contains_key("kd_session"));

    // Signed in again: it shows when it was used; rename, delete.
    let (_, challenge) = app.passkey_options("/login/passkey/options", &[]).await;
    let body = passkey_sign_in(&mut auth, &challenge, "https://demo.test");
    let res = app.post_json("/login/passkey", &body).await;
    assert_eq!(res.status, StatusCode::OK);
    let page = app.get("/settings/security").await;
    assert!(page.body.contains("Last used"));
    let pk = stored[0].id;
    let res = app
        .post(
            &format!("/settings/security/passkeys/{pk}/rename"),
            &[("name", "Laptop")],
        )
        .await;
    assert_eq!(res.location(), "/settings/security#passkeys");
    assert!(app.get("/settings/security").await.body.contains("Laptop"));
    let res = app
        .post(
            &format!("/settings/security/passkeys/{pk}/delete"),
            &[("current_password", PASSWORD)],
        )
        .await;
    assert_eq!(res.location(), "/settings/security#passkeys");
    assert!(
        app.get("/settings/security")
            .await
            .body
            .contains("No passkeys yet")
    );
    // A deleted passkey signs nobody in.
    app.post("/logout", &[]).await;
    let (_, challenge) = app.passkey_options("/login/passkey/options", &[]).await;
    let body = passkey_sign_in(&mut auth, &challenge, "https://demo.test");
    let res = app.post_json("/login/passkey", &body).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

/// Admins remove all of a user's passkeys from the Users page; the user is
/// signed out, and the passkeys sign nobody in any more.
#[tokio::test]
async fn admins_remove_a_users_passkeys() {
    let mut app = App::set_up().await;
    let bob = app.user("bob", Role::Editor).await;
    let before: i64 = app
        .core
        .with_conn(move |c| Ok(db::users::get_user(c, bob)?.unwrap().session_version))
        .await
        .unwrap();
    app.core
        .with_conn(move |c| {
            db::passkeys::insert(
                c,
                &db::passkeys::NewPasskey {
                    user_id: bob,
                    credential_id: b"bob-key",
                    public_key: b"cose",
                    alg: -7,
                    user_handle: b"bob",
                    name: "Phone",
                    sign_count: 0,
                    transports: "",
                    backup_eligible: true,
                    backed_up: true,
                },
            )
        })
        .await
        .unwrap();
    let users = app.get("/settings/users").await;
    let reset = format!("/settings/users/{bob}/passkeys/reset");
    assert!(users.body.contains(&reset));
    assert_eq!(users.body.matches("/passkeys/reset").count(), 1);
    let res = app.post(&reset, &[]).await;
    assert_eq!(res.location(), "/settings/users");
    assert!(
        app.get("/settings/users")
            .await
            .body
            .contains("Passkeys removed for bob.")
    );
    let left = app
        .core
        .with_conn(move |c| db::passkeys::list_for_user(c, bob))
        .await
        .unwrap();
    assert!(left.is_empty());
    let version: i64 = app
        .core
        .with_conn(move |c| Ok(db::users::get_user(c, bob)?.unwrap().session_version))
        .await
        .unwrap();
    assert_eq!(version, before + 1);
}
