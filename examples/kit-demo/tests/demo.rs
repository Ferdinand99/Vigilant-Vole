//! Kit Demo driven in-process: setup, sign-in with two factors, the kit's
//! pages under the demo's name, static files, versions and the schema.

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
use vole_kit::auth::{TokenScope, tokens, totp};
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
        let core = kit_demo::start(&dir).await.unwrap();
        App {
            router: kit_demo::router(core.clone()),
            core,
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
}

/// Branding checks for every page a product gets from the kit.
fn assert_demo_branded(path: &str, html: &str) {
    assert!(html.contains("Kit Demo"), "{path}: no product name\n{html}");
    for foreign in ["Vigilant", "vigilant", "Vole", "vv_", "VV_"] {
        assert!(!html.contains(foreign), "{path}: contains {foreign:?}");
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
    let tabs = app.get("/settings/users").await.body;
    for tab in ["/settings/users", "/settings/security", "/settings/tokens"] {
        assert!(tabs.contains(&format!("href=\"{tab}\"")), "{tab}");
    }
    assert!(!tabs.contains("/settings/general"));

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

    // Errors use the demo's icon.
    let missing = app.get("/no-such-page").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert!(missing.body.contains("/static/logo.svg"));
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
            "api_tokens",
            "notes",
            "schema_migrations",
            "settings",
            "users"
        ]
    );
    assert_eq!(applied, ["kit/0001_core.sql", "0001_notes.sql"]);

    // Starting again changes nothing and takes no backup.
    let report = migrate_again(&app).await;
    assert!(report.applied.is_empty() && report.backup.is_none());
}

async fn migrate_again(app: &App) -> vole_kit::db::MigrationReport {
    vole_kit::db::migrate(&app.core.db, &app.dir.join("backups"))
        .await
        .unwrap()
}
