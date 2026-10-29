//! A product that turns API tokens and the API reference off
//! (`KitFeatures`): the routes are gone, the menus don't link them and a
//! bearer token gets nowhere. This is its own test binary because the kit's
//! configuration is installed once per process.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Request, StatusCode, header};
use tower::ServiceExt as _;
use vole_kit::auth::{TokenScope, tokens};
use vole_kit::db;

static N: AtomicUsize = AtomicUsize::new(0);
const PASSWORD: &str = "Demo-Owner-Pass-2026!";

struct Page {
    status: StatusCode,
    body: String,
}

struct App {
    core: vole_kit::state::Core,
    router: axum::Router,
    cookie: String,
    dir: PathBuf,
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl App {
    async fn send(&self, mut req: Request<Body>, cookie: bool) -> (Page, Option<String>) {
        let h = req.headers_mut();
        h.insert(header::HOST, HeaderValue::from_static("demo.test"));
        if cookie && !self.cookie.is_empty() {
            h.insert(header::COOKIE, HeaderValue::from_str(&self.cookie).unwrap());
        }
        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));
        let res = self.router.clone().oneshot(req).await.unwrap();
        let session = res
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap().to_string())
            .find(|pair| pair.starts_with("kd_session="));
        let status = res.status();
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&body).into_owned();
        (Page { status, body }, session)
    }

    async fn get(&self, path: &str) -> Page {
        self.send(Request::get(path).body(Body::empty()).unwrap(), true)
            .await
            .0
    }
}

async fn set_up() -> App {
    let dir = std::env::temp_dir().join(format!(
        "kit-demo-off-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let demo = kit_demo::start_with(&dir, &kit_demo::KIT_NO_API)
        .await
        .unwrap();
    let mut app = App {
        router: kit_demo::router(&demo),
        core: demo.core.clone(),
        cookie: String::new(),
        dir,
    };
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("username", "ada"),
            ("password", PASSWORD),
            ("confirm", PASSWORD),
            ("site_name", ""),
            ("timezone", "UTC"),
        ])
        .finish();
    let req = Request::post("/setup")
        .header(header::ORIGIN, "http://demo.test")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    let (res, session) = app.send(req, false).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    app.cookie = session.expect("setup signs the admin in");
    app
}

#[tokio::test]
async fn the_token_and_api_reference_routes_are_gone() {
    let app = set_up().await;
    for path in [
        "/settings/tokens",
        "/api-docs",
        "/api-docs.md",
        "/llms.txt",
        kit_demo::KIT_NO_API.docs.openapi_path,
    ] {
        let res = app.get(path).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}");
    }
    // The pages that stay still work.
    for path in ["/", "/settings/security", "/settings/users"] {
        let res = app.get(path).await;
        assert_eq!(res.status, StatusCode::OK, "{path}: {}", res.body);
    }
}

#[tokio::test]
async fn the_menus_do_not_link_them() {
    let app = set_up().await;
    for path in ["/", "/settings/security", "/settings/users"] {
        let body = app.get(path).await.body;
        assert!(
            !body.contains("/settings/tokens"),
            "{path} links the tokens"
        );
        assert!(
            !body.contains("/api-docs"),
            "{path} links the API reference"
        );
    }
}

#[tokio::test]
async fn a_bearer_token_is_not_accepted() {
    let app = set_up().await;
    // A token that would be valid with the features on.
    let token = tokens::generate();
    let (hash, prefix) = (tokens::hash(&token), tokens::display_prefix(&token));
    app.core
        .with_conn(move |c| {
            db::api_tokens::insert(c, 1, "test", &prefix, &hash, TokenScope::Write, None)
        })
        .await
        .unwrap();
    let req = Request::get("/api/v1/notes")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let (res, _) = app.send(req, false).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", res.body);
    // The session still reads the same endpoint.
    let res = app.get("/api/v1/notes").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}
