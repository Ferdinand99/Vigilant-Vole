//! JSON REST API (`/api/v1/**`), its reference page (`/api-docs`) and the
//! backup page (`/settings/backup`). Owned by the api agent.
//!
//! Conventions (all endpoints):
//! - Auth: session cookie or `Authorization: Bearer vv_…` (read tokens act as
//!   viewers). Writes need an editor, settings/agents/backup an admin.
//! - Errors: `{"error": {"code": "...", "message": "...", "fields": {...}}}`.
//! - Lists: `{"data": [...], "next_cursor": "..." | null}` with `?limit=&cursor=`.
//! - GETs carry a weak `ETag` and honour `If-None-Match`.
//! - Session-authenticated writes must come from our own pages (same-origin,
//!   htmx, or a non-form content type, which browsers only send after CORS
//!   preflight). Token requests are exempt.

mod backup;
mod config;
mod docs;
pub mod error {
    //! Moved to [`crate::kit::http::rest`].
    pub use crate::kit::http::rest::{V1Error, V1Result};
}
mod maintenance;
mod monitors;
mod pages;
mod remote_browsers;
mod resources;
mod system;

#[cfg(test)]
mod tests;

use std::sync::LazyLock;
use std::time::Instant;

use axum::Router;
use axum::middleware;

pub use crate::kit::docs::RouteDoc;
pub use crate::kit::http::rest::*;
pub use docs::{DOCS, ROUTES, reference_markdown};

use super::AppState;

/// When the process started serving (for `/api/v1/health`).
static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Protected routes (behind `require_auth`; `CurrentUser` is available).
pub fn router() -> Router<AppState> {
    LazyLock::force(&STARTED);
    let api = Router::new()
        .merge(monitors::router())
        .merge(resources::router())
        .merge(pages::router())
        .merge(remote_browsers::router())
        .merge(maintenance::router())
        .merge(system::router())
        .merge(config::router())
        .fallback(fallback)
        .route_layer(middleware::from_fn(
            crate::kit::http::rest::session_write_guard,
        ));
    // The Hedgerow app's three routes (devices and push texts), with their
    // full paths. The kit's handlers serve every signed-in caller, read-only
    // tokens included; `ROUTES` documents them. A session needs to come
    // from our own pages for the writes, as for every other route here.
    let push = crate::kit::push::api::router::<AppState>().route_layer(middleware::from_fn(
        crate::kit::http::rest::session_write_guard,
    ));
    Router::new()
        .nest("/api/v1", api)
        .merge(push)
        .merge(backup::router())
}

async fn fallback() -> V1Error {
    V1Error::not_found()
}
