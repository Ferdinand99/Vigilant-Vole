//! The real app (startup path, router, every middleware) driven in-process
//! with `tower::ServiceExt::oneshot`: no TCP port, so nothing in a response
//! depends on a random address. Requests carry `Host: vv.test` and the
//! harness keeps cookies like a browser.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use vigilant_vole::api::{self, AppState};
use vigilant_vole::auth::{TokenScope, tokens};
use vigilant_vole::db;

use crate::common::DataDir;

pub const HOST: &str = "vv.test";
pub const ORIGIN: &str = "http://vv.test";
pub const ADMIN: &str = "admin";
pub const ADMIN_PASSWORD: &str = "Golden-Owner-Pass-2026!";
pub const SITE_NAME: &str = "Golden Vole";
/// Slug and custom domain of the seeded status page.
pub const PAGE_SLUG: &str = "golden";
pub const PAGE_DOMAIN: &str = "status.golden.test";

/// A well-formed, fixed API token (`vv_` + 43 base62), so pages and requests
/// that show or use it never differ between runs.
fn fixed_token(seed: &str) -> String {
    format!(
        "{}{seed:0<width$}",
        tokens::TOKEN_PREFIX,
        width = tokens::TOKEN_BODY_LEN
    )
}

/// The admin's write token (seeded by [`App::seeded`]).
pub fn admin_token() -> String {
    fixed_token("GoldenAdminWriteToken")
}

/// The admin's read-only token (seeded by [`App::seeded`]).
pub fn read_token() -> String {
    fixed_token("GoldenViewerReadToken")
}

pub struct Res {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Res {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    pub fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }
}

pub struct App {
    pub state: AppState,
    router: Router,
    cookies: BTreeMap<String, String>,
    // Last: removed after the app is gone.
    pub dir: DataDir,
}

impl App {
    /// Starts the app on `dir` the way `vigilant-vole serve` does (including
    /// migrations), minus the monitor scheduler.
    pub async fn start(dir: DataDir) -> App {
        let state = vigilant_vole::server::prepare(dir.config())
            .await
            .unwrap_or_else(|e| panic!("startup: {e}"));
        // No checks: they would add heartbeats (and reach the network) while
        // pages are rendered. Once shut down, saving a monitor starts nothing.
        state.scheduler.shutdown().await;
        let router = api::build_router(state.clone());
        App {
            state,
            router,
            cookies: BTreeMap::new(),
            dir,
        }
    }

    /// A brand-new instance (no users yet).
    pub async fn fresh(label: &str) -> App {
        Self::start(DataDir::new(&format!("golden-{label}"))).await
    }

    /// A fresh instance after setup: the admin signed in (session cookie
    /// held), two API tokens, three monitors and a published status page.
    pub async fn seeded(label: &str) -> App {
        let mut app = Self::fresh(label).await;
        let res = app
            .post_form(
                "/setup",
                &[
                    ("username", ADMIN),
                    ("password", ADMIN_PASSWORD),
                    ("confirm", ADMIN_PASSWORD),
                    ("site_name", SITE_NAME),
                    ("timezone", "UTC"),
                ],
            )
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "setup: {}", res.text());
        // Setup leaves a one-time "all set" flash; show it now so later
        // pages render without it.
        app.get("/").await;

        let (admin, read) = (admin_token(), read_token());
        app.state
            .with_conn(move |c| {
                for (name, token, scope) in [
                    ("Golden admin", &admin, TokenScope::Write),
                    ("Golden read-only", &read, TokenScope::Read),
                ] {
                    db::api_tokens::insert(
                        c,
                        1,
                        name,
                        &tokens::display_prefix(token),
                        &tokens::hash(token),
                        scope,
                        None,
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap_or_else(|_| panic!("seeding tokens"));

        for monitor in [
            json!({
                "name": "Website",
                "monitor_type": "http",
                "target": "https://example.com",
                "interval_seconds": 60,
                "retries": 1,
                "tags": ["production", {"name": "env", "value": "prod"}],
            }),
            json!({
                "name": "API",
                "monitor_type": "http",
                "target": "https://api.example.com/health",
                "interval_seconds": 30,
            }),
            json!({
                "name": "Legacy box",
                "monitor_type": "ping",
                "target": "legacy.example.com",
                "active": false,
            }),
        ] {
            let res = app
                .api(Method::POST, "/api/v1/monitors", Some(monitor))
                .await;
            assert!(res.status.is_success(), "seed monitor: {}", res.text());
        }
        let res = app
            .api(
                Method::POST,
                "/api/v1/status-pages",
                Some(json!({
                    "title": "Golden status",
                    "slug": PAGE_SLUG,
                    "custom_domain": PAGE_DOMAIN,
                    "sections": [{"name": "Web", "monitor_ids": [1, 2]}],
                })),
            )
            .await;
        assert!(res.status.is_success(), "seed status page: {}", res.text());
        app
    }

    /// Forget every cookie (sign out without a request).
    pub fn clear_cookies(&mut self) {
        self.cookies.clear();
    }

    pub fn cookie_names(&self) -> Vec<String> {
        self.cookies.keys().cloned().collect()
    }

    /// Sends `req` as is, adding only `Host` (when absent), the cookies held
    /// and the TCP peer address; remembers cookies the response sets.
    pub async fn send(&mut self, mut req: Request<Body>) -> Res {
        let headers = req.headers_mut();
        if !headers.contains_key(header::HOST) {
            headers.insert(header::HOST, HeaderValue::from_static(HOST));
        }
        if !self.cookies.is_empty() && !headers.contains_key(header::COOKIE) {
            let cookie = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            headers.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
        }
        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));

        let res = self.router.clone().oneshot(req).await.unwrap();
        for v in res.headers().get_all(header::SET_COOKIE) {
            let Ok(v) = v.to_str() else { continue };
            let pair = v.split(';').next().unwrap_or_default();
            if let Some((k, val)) = pair.split_once('=') {
                if val.is_empty() || v.contains("Max-Age=0") {
                    self.cookies.remove(k);
                } else {
                    self.cookies.insert(k.to_string(), val.to_string());
                }
            }
        }
        let status = res.status();
        let headers = res.headers().clone();
        let body = to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Res {
            status,
            headers,
            body,
        }
    }

    pub async fn get(&mut self, path: &str) -> Res {
        self.get_with(path, &[]).await
    }

    pub async fn get_with(&mut self, path: &str, headers: &[(&str, &str)]) -> Res {
        let mut req = Request::get(path);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }

    /// A GET without the cookies held (they are kept for later requests).
    pub async fn get_anon(&mut self, path: &str, headers: &[(&str, &str)]) -> Res {
        let saved = std::mem::take(&mut self.cookies);
        let res = self.get_with(path, headers).await;
        self.cookies = saved;
        res
    }

    /// A form POST from one of our own pages (same `Origin`).
    pub async fn post_form(&mut self, path: &str, form: &[(&str, &str)]) -> Res {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let req = Request::post(path)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        self.send(req).await
    }

    /// A REST call with the admin's write token.
    pub async fn api(&mut self, method: Method, path: &str, body: Option<Value>) -> Res {
        self.api_as(&admin_token(), method, path, body).await
    }

    pub async fn api_as(
        &mut self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Res {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let req = match body {
            Some(json) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json.to_string())),
            None => req.body(Body::empty()),
        };
        // Token requests carry no cookies, like a script would.
        let saved = std::mem::take(&mut self.cookies);
        let res = self.send(req.unwrap()).await;
        self.cookies = saved;
        res
    }
}
