//! HTTP building blocks: errors, security headers, templates, static files.

pub mod assets;
pub mod error;
pub mod headers;

use askama::Template;
use axum::response::{Html, IntoResponse, Response};

pub use error::{ApiError, ApiResult};

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

/// Liveness probe.
pub async fn healthz() -> &'static str {
    "ok"
}
