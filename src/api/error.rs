//! `ApiError`: the one error type for handlers.
//!
//! `ApiError::into_response` produces an HTML response and tags it with an
//! [`ErrorInfo`] extension. The [`negotiate`] middleware (wired once around the
//! whole router) then rewrites tagged responses for the caller:
//! - paths under `/api/` or `Accept: application/json` → JSON `{"error": "..."}`
//! - htmx requests (`HX-Request`) → plain-text message (so the client can toast
//!   it); `Unauthorized` additionally sets `HX-Redirect: /login`
//! - everything else → a minimal HTML error page (`Unauthorized` → redirect to `/login`)

use std::fmt;

use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use serde_json::json;

use crate::db::DbError;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("forbidden")]
    Forbidden,
    /// The message is shown to the user, so keep it human-readable.
    #[error("{0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    /// The message is logged, never shown to the user.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        ApiError::BadRequest(msg.into())
    }

    pub fn internal(err: impl fmt::Display) -> Self {
        ApiError::Internal(err.to_string())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::Forbidden => StatusCode::FORBIDDEN,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Safe-to-display message.
    pub fn public_message(&self) -> String {
        match self {
            ApiError::NotFound => crate::i18n::t("error.not_found").into(),
            ApiError::Forbidden => crate::i18n::t("error.forbidden").into(),
            ApiError::BadRequest(msg) => msg.clone(),
            ApiError::Unauthorized => crate::i18n::t("error.unauthorized").into(),
            ApiError::Internal(_) => crate::i18n::t("error.internal").into(),
        }
    }
}

/// Attached to every response built from an `ApiError`, for [`negotiate`].
#[derive(Debug, Clone)]
pub struct ErrorInfo {
    pub status: StatusCode,
    pub message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(detail) = &self {
            tracing::error!(error = %detail, "request failed");
        }
        let status = self.status();
        let message = self.public_message();
        let mut response = match self {
            ApiError::Unauthorized => Redirect::to("/login").into_response(),
            _ => (status, Html(error_page(status, &message))).into_response(),
        };
        response.extensions_mut().insert(ErrorInfo { status, message });
        response
    }
}

/// Middleware: re-shape `ApiError` responses for JSON / htmx clients.
pub async fn negotiate(request: Request, next: Next) -> Response {
    let wants_json = request.uri().path().starts_with("/api/")
        || request
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("application/json"));
    let is_htmx = request.headers().contains_key("hx-request");
    let is_v1 = request.uri().path().starts_with("/api/v1/");

    let response = next.run(request).await;
    let Some(info) = response.extensions().get::<ErrorInfo>().cloned() else {
        return response;
    };

    // The REST API documents a structured envelope; keep middleware errors (e.g. the
    // 401 from `require_auth`) consistent with what its own handlers return.
    if is_v1 {
        let code = match info.status {
            StatusCode::BAD_REQUEST => "bad_request",
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::NOT_FOUND => "not_found",
            _ => "internal",
        };
        return (info.status, Json(json!({ "error": { "code": code, "message": info.message } }))).into_response();
    }
    if wants_json {
        return (info.status, Json(json!({ "error": info.message }))).into_response();
    }
    if is_htmx {
        let mut response = (info.status, info.message).into_response();
        if info.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert("hx-redirect", HeaderValue::from_static("/login"));
        }
        return response;
    }
    response
}

/// Fallback handler for unmatched routes.
pub async fn not_found() -> ApiError {
    ApiError::NotFound
}

fn error_page(status: StatusCode, message: &str) -> String {
    let title = match status {
        StatusCode::NOT_FOUND => crate::i18n::t("error.title_404"),
        StatusCode::FORBIDDEN => crate::i18n::t("error.title_403"),
        StatusCode::BAD_REQUEST => crate::i18n::t("error.title_400"),
        StatusCode::UNAUTHORIZED => crate::i18n::t("error.title_401"),
        StatusCode::INTERNAL_SERVER_ERROR => crate::i18n::t("error.title_500"),
        other => other.canonical_reason().unwrap_or("Error"),
    };
    format!(
        "<!doctype html>\n<html lang=\"{lang}\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <title>{code} · {title}</title><link rel=\"icon\" href=\"/static/favicon.svg\" type=\"image/svg+xml\">\
         <link rel=\"stylesheet\" href=\"/static/style.css\"><script src=\"/static/theme.js\"></script></head>\
         <body class=\"error-page\"><main class=\"container\"><div class=\"card empty-state\">\
         <h1>{code} · {title}</h1><p>{message}</p><p><a class=\"btn\" href=\"/\">{back}</a></p>\
         </div></main></body></html>",
        code = status.as_u16(),
        title = escape_html(title),
        message = escape_html(message),
        lang = crate::i18n::current().code(),
        back = escape_html(crate::i18n::t("error.back")),
    )
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

impl From<rusqlite::Error> for ApiError {
    fn from(err: rusqlite::Error) -> Self {
        match err {
            rusqlite::Error::QueryReturnedNoRows => ApiError::NotFound,
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<DbError> for ApiError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::Sql(e) => e.into(),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<deadpool_sqlite::InteractError> for ApiError {
    fn from(err: deadpool_sqlite::InteractError) -> Self {
        ApiError::Internal(err.to_string())
    }
}

impl From<deadpool_sqlite::PoolError> for ApiError {
    fn from(err: deadpool_sqlite::PoolError) -> Self {
        ApiError::Internal(err.to_string())
    }
}

impl From<askama::Error> for ApiError {
    fn from(err: askama::Error) -> Self {
        ApiError::Internal(format!("template error: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    fn app() -> Router {
        Router::new()
            .route("/api/thing", get(|| async { ApiError::NotFound }))
            .route("/page", get(|| async { ApiError::Forbidden }))
            .route("/private", get(|| async { ApiError::Unauthorized }))
            .layer(axum::middleware::from_fn(negotiate))
    }

    async fn call(uri: &str, htmx: bool) -> Response {
        let mut req = Request::builder().uri(uri);
        if htmx {
            req = req.header("hx-request", "true");
        }
        app().oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    async fn body(resp: Response) -> String {
        String::from_utf8(to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn api_paths_get_json_errors() {
        let resp = call("/api/thing", false).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(body(resp).await.starts_with("{\"error\":"));
    }

    #[tokio::test]
    async fn pages_get_html_errors() {
        let resp = call("/page", false).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let html = body(resp).await;
        assert!(html.contains("<!doctype html>"));
        // Same chrome as the app: theme, color scheme and icon.
        for part in ["name=\"color-scheme\"", "/static/favicon.svg", "/static/theme.js"] {
            assert!(html.contains(part), "{part}");
        }
    }

    #[tokio::test]
    async fn htmx_gets_plain_text() {
        let resp = call("/page", true).await;
        assert_eq!(body(resp).await, ApiError::Forbidden.public_message());
    }

    #[tokio::test]
    async fn unauthorized_page_redirects_to_login() {
        let resp = call("/private", false).await;
        assert_eq!(resp.headers()[header::LOCATION], "/login");
    }

    #[tokio::test]
    async fn unauthorized_htmx_sets_hx_redirect() {
        let resp = call("/private", true).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(resp.headers()["hx-redirect"], "/login");
    }

    #[test]
    fn escape_html_escapes_markup() {
        assert_eq!(escape_html("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }
}
