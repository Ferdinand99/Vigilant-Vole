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
pub mod error {
    //! Moved to [`crate::kit::http::error`].
    pub use crate::kit::http::error::*;
}
pub mod events;
pub mod maintenance;
pub mod metrics;
pub mod monitors;
pub mod notifications;
pub mod page;
pub mod public_status;
pub mod push;
pub mod remote_browsers;
pub mod settings;
pub mod status_pages;
pub mod tags;
pub mod tokens;
pub mod users;
pub mod v1;

use std::sync::Arc;

use axum::extract::FromRef;
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

pub use crate::kit::http::{ApiError, ApiResult, HtmlTemplate};
pub use page::{Flash, PageCtx, UserView, flash};

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

impl FromRef<AppState> for crate::kit::state::Core {
    fn from_ref(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            session_key: state.session_key.clone(),
            http: state.http.clone(),
            cfg: &crate::product::KIT,
        }
    }
}

impl AppState {
    /// The part of the state the shared kit works with.
    pub fn core(&self) -> crate::kit::state::Core {
        axum::extract::FromRef::from_ref(self)
    }

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
        crate::kit::state::with_conn(&self.db, f).await
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
        .merge(remote_browsers::router())
        .merge(v1::router())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let public = Router::new()
        .route("/healthz", get(crate::kit::http::healthz))
        .route(
            "/static/{*path}",
            get(crate::kit::http::assets::static_asset),
        )
        .merge(auth::public_router())
        .merge(public_status::public_router())
        .merge(badges::public_router())
        .merge(push::public_router())
        .merge(agents::public_router())
        .merge(metrics::public_router())
        .merge(crate::mcp::router());

    // Layers run outermost-last: trace → compression → negotiate → language → custom domain → csrf → route.
    protected
        .merge(public)
        .fallback(error::not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::csrf_guard,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            public_status::custom_domain,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::i18n::layer,
        ))
        .layer(middleware::from_fn(error::negotiate))
        .layer(middleware::from_fn(
            crate::kit::http::headers::security_headers,
        ))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

/// The product's static files, for the kit's static file handler.
pub fn static_file(path: &str) -> Option<crate::kit::http::assets::StaticFile> {
    let file = StaticAssets::get(path)?;
    Some(crate::kit::http::assets::StaticFile {
        sha256: file.metadata.sha256_hash(),
        data: file.data,
    })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    use crate::kit::http::assets::guess_content_type;

    #[tokio::test]
    async fn app_pages_get_a_strict_csp_and_status_pages_stay_embeddable() {
        let state = super::agents::test_support::test_state().await;
        let app = super::build_router(state);
        let csp = |res: &axum::response::Response| {
            res.headers()
                .get(axum::http::header::CONTENT_SECURITY_POLICY)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let res = app
            .clone()
            .oneshot(Request::get("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let app_csp = csp(&res);
        assert!(
            app_csp.contains("script-src 'self';") && app_csp.contains("frame-ancestors 'none'"),
            "{app_csp}"
        );
        assert!(!app_csp.contains("unsafe-eval"));
        assert_eq!(res.headers()["x-frame-options"], "DENY");

        let res = app
            .oneshot(Request::get("/status/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let public = csp(&res);
        assert!(
            public.contains("script-src 'self';") && !public.contains("frame-ancestors"),
            "{public}"
        );
        assert!(res.headers().get("x-frame-options").is_none());
    }

    #[test]
    fn content_types_cover_fonts_and_icons() {
        assert_eq!(guess_content_type("fonts/inter.woff2"), "font/woff2");
        assert_eq!(guess_content_type("favicon.ICO"), "image/x-icon");
        assert_eq!(
            guess_content_type("site.webmanifest"),
            "application/manifest+json"
        );
        assert_eq!(
            guess_content_type("unknown.bin"),
            "application/octet-stream"
        );
    }
}
