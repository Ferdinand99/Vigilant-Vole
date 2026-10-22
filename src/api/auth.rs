//! Authentication glue. The setup, sign-in, 2FA and sign-out pages are the
//! kit's ([`crate::kit::auth::pages`]); the middleware is
//! [`crate::kit::auth::guard`]. These wrappers take [`AppState`].
//!
//! Public helpers for other modules:
//! - [`session_user`]: the logged-in user on a public page (cookie only)
//! - [`authenticate_token`]: resolve an API token to a (scope-capped) user
//! - [`authenticate_headers`]: same, reading `Authorization: Bearer|Basic`

use axum::Router;
use axum::http::HeaderMap;
use axum_extra::extract::cookie::SignedCookieJar;

use super::{ApiResult, AppState};
use crate::auth::{AuthMethod, CurrentUser};
use crate::kit::auth::guard;
pub use crate::kit::auth::guard::{PeerAddr, csrf_guard, login_jar, require_auth, secure_cookies};
pub use crate::kit::auth::pages::verify_second_factor;
use crate::kit::state::Core;

/// The kit's protected pages: users, API tokens, Security, API reference.
pub fn router() -> Router<AppState> {
    crate::kit::auth::pages::router()
}

/// The kit's setup, sign-in, 2FA and sign-out pages.
pub fn public_router() -> Router<AppState> {
    crate::kit::auth::pages::public_router()
}

// ---------------------------------------------------------------------------
// Small extractors / helpers
// ---------------------------------------------------------------------------

fn core(state: &AppState) -> Core {
    axum::extract::FromRef::from_ref(state)
}

/// Resolves the session cookie to an active user, if any. Protected routes get
/// this via the `CurrentUser` extractor; public pages (e.g. a status page that
/// shows an "Edit" link to logged-in editors) can call it directly.
pub async fn session_user(
    state: &AppState,
    jar: &SignedCookieJar,
) -> ApiResult<Option<CurrentUser>> {
    guard::session_user(&core(state), jar).await
}

pub async fn authenticate_token_detailed(
    state: &AppState,
    token: &str,
) -> ApiResult<Option<(CurrentUser, AuthMethod)>> {
    guard::authenticate_token_detailed(&core(state), token).await
}

/// Resolves an API token to the user it acts as, with the role capped by
/// the token's scope (read → viewer, write → the user's role). `None` for
/// unknown, malformed, expired or revoked tokens and disabled users. Records
/// `last_used_at` (at most once a minute).
pub async fn authenticate_token(state: &AppState, token: &str) -> ApiResult<Option<CurrentUser>> {
    guard::authenticate_token(&core(state), token).await
}

/// [`authenticate_token`] for `Authorization: Bearer <token>` or
/// `Authorization: Basic base64(any:<token>)` (Prometheus `basic_auth`).
/// `None` when the header is missing or the token is invalid.
pub async fn authenticate_headers(
    state: &AppState,
    headers: &HeaderMap,
) -> ApiResult<Option<CurrentUser>> {
    guard::authenticate_headers(&core(state), headers).await
}

// ---------------------------------------------------------------------------
// Middleware + request authentication
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
