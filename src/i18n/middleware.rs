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
use axum::http::{HeaderMap, header};
use axum::middleware::Next;
use axum::response::Response;

use super::{Lang, scope};
use crate::api::AppState;
use crate::db::settings;

pub async fn layer(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if is_machine_path(path) {
        return next.run(request).await;
    }
    let browser = negotiate(request.headers());
    let site = site_language(&state).await;
    let lang = if path.starts_with("/status/") {
        browser.or(site)
    } else {
        site.or(browser)
    }
    .unwrap_or_default();
    scope(lang, next.run(request)).await
}

fn is_machine_path(path: &str) -> bool {
    path.starts_with("/static/") || path.starts_with("/api/") || path == "/metrics" || path == "/healthz"
}

/// The admin-chosen language, if one was saved (also refreshes the cached
/// [`super::site_default`]).
pub async fn site_language(state: &AppState) -> Option<Lang> {
    match state.with_conn(|c| settings::get(c, settings::LANGUAGE)).await {
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

/// The best supported language from `Accept-Language` (highest q first).
pub fn negotiate(headers: &HeaderMap) -> Option<Lang> {
    let value = headers.get(header::ACCEPT_LANGUAGE)?.to_str().ok()?;
    parse_accept_language(value)
}

pub(super) fn parse_accept_language(value: &str) -> Option<Lang> {
    let mut ranges: Vec<(f32, usize, &str)> = value
        .split(',')
        .enumerate()
        .filter_map(|(i, part)| {
            let mut pieces = part.split(';');
            let tag = pieces.next()?.trim();
            let q = pieces
                .find_map(|p| p.trim().strip_prefix("q="))
                .map_or(Some(1.0), |q| q.trim().parse::<f32>().ok())?;
            (!tag.is_empty() && q > 0.0).then_some((q, i, tag))
        })
        .collect();
    // Highest q first; ties keep header order.
    ranges.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    ranges.into_iter().find_map(|(_, _, tag)| Lang::from_code(tag))
}
