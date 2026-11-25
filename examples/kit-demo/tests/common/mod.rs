//! A signed-in demo for the tests of the side-menu shell: the kit's
//! configuration is installed once per process, so each test binary picks one.
//! Not every test binary uses every helper.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use tower::ServiceExt as _;
use vole_kit::auth::Role;
use vole_kit::config::KitConfig;
use vole_kit::db;

static N: AtomicUsize = AtomicUsize::new(0);
const PASSWORD: &str = "Demo-Owner-Pass-2026!";

pub struct Page {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

pub struct App {
    pub core: vole_kit::state::Core,
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
    async fn send(&mut self, mut req: Request<Body>) -> Page {
        let h = req.headers_mut();
        h.insert(header::HOST, HeaderValue::from_static("demo.test"));
        if !self.cookie.is_empty() {
            h.insert(header::COOKIE, HeaderValue::from_str(&self.cookie).unwrap());
        }
        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));
        let res = self.router.clone().oneshot(req).await.unwrap();
        if let Some(pair) = res
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap().to_string())
            .find(|pair| pair.starts_with("kd_session="))
        {
            self.cookie = if pair.ends_with('=') {
                String::new()
            } else {
                pair
            };
        }
        let (status, headers) = (res.status(), res.headers().clone());
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        Page {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    pub async fn get(&mut self, path: &str) -> Page {
        self.send(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    pub async fn post(&mut self, path: &str, form: &[(&str, &str)]) -> Page {
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

    /// Adds a user and signs in as that user.
    pub async fn sign_in_as(&mut self, name: &'static str, role: Role) {
        let hash = vole_kit::auth::hash_password(PASSWORD);
        self.core
            .with_conn(move |c| db::users::insert_user(c, name, &hash, role))
            .await
            .unwrap();
        self.post("/logout", &[]).await;
        let res = self
            .post("/login", &[("username", name), ("password", PASSWORD)])
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    }
}

/// The demo with `config`, set up with its first admin ("ada") signed in.
pub async fn set_up(config: &'static KitConfig) -> App {
    let dir = std::env::temp_dir().join(format!(
        "kit-demo-shell-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let demo = kit_demo::start_with(&dir, config).await.unwrap();
    let mut app = App {
        router: kit_demo::router(&demo),
        core: demo.core.clone(),
        cookie: String::new(),
        dir,
    };
    let res = app
        .post(
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
    assert!(!app.cookie.is_empty(), "setup signs the admin in");
    app
}

/// The start tags of the menu's entries (`<a class="side-link" …>`).
pub fn side_links(html: &str) -> Vec<String> {
    html.split("<a class=\"side-link\"")
        .skip(1)
        .map(|rest| format!("<a class=\"side-link\"{}", rest.split('>').next().unwrap()))
        .collect()
}

/// The address of each entry of the menu, in order.
pub fn side_hrefs(html: &str) -> Vec<String> {
    side_links(html)
        .iter()
        .map(|l| {
            l.split("href=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// The addresses of the entries marked as the current page.
pub fn side_current(html: &str) -> Vec<String> {
    side_links(html)
        .iter()
        .filter(|l| l.contains("aria-current=\"page\""))
        .map(|l| {
            l.split("href=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .to_string()
        })
        .collect()
}
