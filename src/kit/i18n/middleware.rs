//! Picks the language for each request.
//!
//! - Public status pages: the viewer's `Accept-Language`, then the site default.
//! - Everything else: the site default (Settings → General); before an admin
//!   has chosen one, the browser's `Accept-Language`; then English.
//! - Signed-in users with their own language get it via `auth::require_auth`,
//!   which re-scopes the request.
//!
//! Machine endpoints (`/api/*`, `/metrics`, static files) stay English and
//! skip the settings lookup.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use super::{Lang, negotiate, scope};
use crate::kit::db::settings;
use crate::kit::state::Core;

pub async fn layer(State(core): State<Core>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if is_machine_path(path) {
        return next.run(request).await;
    }
    let browser = negotiate(request.headers());
    let site = site_language(&core).await;
    let cfg = crate::kit::config::get();
    let lang = if cfg
        .browser_language_prefixes
        .iter()
        .any(|p| path.starts_with(p))
    {
        browser.or(site)
    } else {
        site.or(browser)
    }
    .unwrap_or_default();
    scope(lang, next.run(request)).await
}

fn is_machine_path(path: &str) -> bool {
    let cfg = crate::kit::config::get();
    cfg.machine_path_prefixes
        .iter()
        .any(|p| path.starts_with(p))
        || cfg.machine_paths.contains(&path)
}

/// The admin-chosen language, if one was saved (also refreshes the cached
/// [`super::site_default`]).
pub async fn site_language(core: &Core) -> Option<Lang> {
    match core
        .with_conn(|c| settings::get(c, settings::LANGUAGE))
        .await
    {
        Ok(code) => {
            let lang = code.as_deref().and_then(Lang::from_code);
            super::set_site_default(lang.unwrap_or_default());
            lang
        }
        Err(err) => {
            tracing::warn!(%err, "failed to load the site language");
            None
        }
    }
}
