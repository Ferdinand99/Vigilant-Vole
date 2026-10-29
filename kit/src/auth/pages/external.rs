//! The kit's sign-in and setup pages in
//! [`CredentialMode::External`](crate::config::CredentialMode::External):
//! they only point to the product's own pages.

use axum::extract::{Query, RawQuery};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::auth::session;
use crate::config::{CredentialMode, ExternalCredentials};
use crate::http::ApiError;

/// The product's pages (only mounted when the mode is `External`).
pub(super) fn paths() -> Option<&'static ExternalCredentials> {
    match crate::config::get().features.credentials {
        CredentialMode::External(ext) => Some(ext),
        CredentialMode::Plain => None,
    }
}

#[derive(Deserialize, Default)]
pub(super) struct NextQuery {
    next: Option<String>,
}

/// `GET /login`: on to the product's sign-in page, keeping a `next` that
/// the kit would have followed too.
pub(super) async fn login(Query(query): Query<NextQuery>) -> Response {
    let Some(ext) = paths() else {
        return ApiError::NotFound.into_response();
    };
    let next = session::sanitize_next(query.next.as_deref());
    if next == "/" {
        Redirect::to(ext.login_path).into_response()
    } else {
        Redirect::to(&ExternalCredentials::with_next(ext.login_path, &next)).into_response()
    }
}

/// `GET /setup` and `POST /setup`: on to the product's setup page. The
/// query (the setup token a start-up message links with) goes along; the
/// product checks the token.
pub(super) async fn setup(RawQuery(query): RawQuery) -> Response {
    let Some(ext) = paths() else {
        return ApiError::NotFound.into_response();
    };
    match query.filter(|q| !q.is_empty()) {
        Some(q) => Redirect::to(&format!("{}?{q}", ext.setup_path)).into_response(),
        None => Redirect::to(ext.setup_path).into_response(),
    }
}

/// A route the mode has taken away, where the method alone would otherwise
/// answer `405`.
pub(super) async fn gone() -> Response {
    ApiError::NotFound.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_is_encoded_into_the_query() {
        assert_eq!(
            ExternalCredentials::with_next("/vault/login", "/settings/security?a=b&c=d"),
            "/vault/login?next=%2Fsettings%2Fsecurity%3Fa%3Db%26c%3Dd"
        );
    }
}
