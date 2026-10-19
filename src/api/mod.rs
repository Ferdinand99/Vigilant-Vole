//! HTTP layer. This file is foundation-owned and wires every feature module
//! exactly once; feature agents only edit their own `api/<feature>.rs`.
//!
//! Each feature module exposes `pub fn router() -> Router<AppState>` (protected:
//! behind `auth::require_auth`, so `CurrentUser` is always available) and,
//! where it serves unauthenticated routes, `pub fn public_router()`.

pub mod agents;
pub mod auth;
pub mod badges;
pub mod dashboard;
pub mod error;
pub mod events;
pub mod maintenance;
pub mod metrics;
pub mod monitors;
pub mod notifications;
pub mod page;
pub mod public_status;
pub mod push;
pub mod settings;
pub mod status_pages;
pub mod tags;
pub mod tokens;
pub mod users;
pub mod v1;

use std::sync::Arc;

use askama::Template;
use axum::extract::{FromRef, Path as AxumPath};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use axum_extra::extract::cookie::Key;
use deadpool_sqlite::Pool;
use rust_embed::Embed;
use tokio::sync::broadcast;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::db::settings::{self as db_settings, Settings};
use crate::monitor::Scheduler;

pub use error::{ApiError, ApiResult};
pub use page::{Flash, PageCtx, UserView, flash};

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

#[derive(Clone)]
pub struct AppState {
    pub db: Pool,
    pub scheduler: Scheduler,
    pub session_key: Key,
    /// Fire-and-forget "something changed" signal; SSE dashboards re-render on it.
    pub update_tx: broadcast::Sender<()>,
    /// Shared outbound HTTP client (notifications, RDAP, etc.). Not for
    /// monitor checks, which need per-monitor TLS/redirect/proxy options.
    pub http: reqwest::Client,
    pub config: Arc<Config>,
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Key {
        state.session_key.clone()
    }
}

impl AppState {
    /// Run a closure on a pooled connection, mapping every failure to `ApiError`.
    ///
    /// ```ignore
    /// let monitor = state.with_conn(move |c| db_monitors::get_monitor(c, id)).await?.ok_or(ApiError::NotFound)?;
    /// ```
    pub async fn with_conn<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        // The closure runs on a blocking thread: carry the request's language
        // along so labels built there (status pages, pickers) are translated.
        let lang = crate::i18n::current();
        Ok(crate::db::interact(&self.db, move |c| crate::i18n::sync_scope(lang, || f(c))).await?)
    }

    /// Current settings (one small query).
    pub async fn settings(&self) -> Result<Settings, ApiError> {
        self.with_conn(|c| db_settings::load(c)).await
    }

    /// Tell connected dashboards to refresh.
    pub fn notify_update(&self) {
        // No subscribers is fine.
        let _ = self.update_tx.send(());
    }
}

pub fn build_router(state: AppState) -> Router {
    let protected = Router::new()
        .merge(dashboard::router())
        .merge(monitors::router())
        .merge(events::router())
        .merge(tags::router())
        .merge(notifications::router())
        .merge(status_pages::router())
        .merge(maintenance::router())
        .merge(auth::router())
        .merge(users::router())
        .merge(settings::router())
        .merge(tokens::router())
        .merge(agents::router())
        .merge(metrics::router())
        .merge(v1::router())
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    let public = Router::new()
        .route("/healthz", get(healthz))
        .route("/static/{*path}", get(static_asset))
        .merge(auth::public_router())
        .merge(public_status::public_router())
        .merge(badges::public_router())
        .merge(push::public_router())
        .merge(agents::public_router())
        .merge(metrics::public_router());

    // Layers run outermost-last: trace → compression → negotiate → language → custom domain → csrf → route.
    protected
        .merge(public)
        .fallback(error::not_found)
        .layer(middleware::from_fn_with_state(state.clone(), auth::csrf_guard))
        .layer(middleware::from_fn_with_state(state.clone(), public_status::custom_domain))
        .layer(middleware::from_fn_with_state(state.clone(), crate::i18n::layer))
        .layer(middleware::from_fn(error::negotiate))
        .layer(middleware::from_fn(security_headers))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Content-Security-Policy for the app. Scripts only from our own files (no
/// inline scripts or handlers anywhere; the i18n strings are a JSON data
/// block). Inline styles stay allowed for `style=` attributes and custom CSS.
const APP_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; connect-src 'self'; font-src 'self'; frame-ancestors 'none'; \
    base-uri 'self'; form-action 'self'";
/// The same for embeddable public status pages, minus `frame-ancestors`.
const PUBLIC_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; connect-src 'self'; font-src 'self'; base-uri 'self'; form-action 'self'";

/// Baseline hardening headers. Public status pages and the status-page JSON stay
/// frameable so people can embed them, everything else (the admin UI) cannot be.
async fn security_headers(req: axum::extract::Request, next: middleware::Next) -> Response {
    let by_path = req.uri().path().starts_with("/status/") || req.uri().path().starts_with("/api/status-page/");
    let mut res = next.run(req).await;
    // Status pages served on a custom domain at `/` mark themselves.
    let embeddable = by_path || res.extensions().get::<public_status::PublicPage>().is_some();
    let headers = res.headers_mut();
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    if !embeddable {
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    headers
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static(if embeddable { PUBLIC_CSP } else { APP_CSP }));
    res
}

async fn healthz() -> &'static str {
    "ok"
}

/// Renders an askama template; render failures become `ApiError::Internal`.
pub struct HtmlTemplate<T>(pub T);

impl<T: Template> IntoResponse for HtmlTemplate<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => ApiError::from(err).into_response(),
        }
    }
}

async fn static_asset(AxumPath(path): AxumPath<String>, headers: HeaderMap) -> Response {
    let Some(file) = StaticAssets::get(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", hex::encode(&file.metadata.sha256_hash()[..16]));
    // Debug builds serve straight from disk, so don't let the browser cache edits.
    let cache_control = if cfg!(debug_assertions) {
        "no-cache"
    } else {
        "public, max-age=3600"
    };
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));

    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, guess_content_type(&path))], file.data).into_response()
    };
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_control));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    response
}

fn guess_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    use super::guess_content_type;

    #[tokio::test]
    async fn app_pages_get_a_strict_csp_and_status_pages_stay_embeddable() {
        let state = super::agents::test_support::test_state().await;
        let app = super::build_router(state);
        let csp = |res: &super::Response| {
            res.headers()
                .get(super::header::CONTENT_SECURITY_POLICY)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let res = app.clone().oneshot(Request::get("/login").body(Body::empty()).unwrap()).await.unwrap();
        let app_csp = csp(&res);
        assert!(app_csp.contains("script-src 'self';") && app_csp.contains("frame-ancestors 'none'"), "{app_csp}");
        assert!(!app_csp.contains("unsafe-eval"));
        assert_eq!(res.headers()["x-frame-options"], "DENY");

        let res = app.oneshot(Request::get("/status/nope").body(Body::empty()).unwrap()).await.unwrap();
        let public = csp(&res);
        assert!(public.contains("script-src 'self';") && !public.contains("frame-ancestors"), "{public}");
        assert!(res.headers().get("x-frame-options").is_none());
    }

    #[test]
    fn content_types_cover_fonts_and_icons() {
        assert_eq!(guess_content_type("fonts/inter.woff2"), "font/woff2");
        assert_eq!(guess_content_type("favicon.ICO"), "image/x-icon");
        assert_eq!(guess_content_type("site.webmanifest"), "application/manifest+json");
        assert_eq!(guess_content_type("unknown.bin"), "application/octet-stream");
    }
}
