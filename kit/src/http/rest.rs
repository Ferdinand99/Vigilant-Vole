//! The conventions of the JSON REST API: the error envelope, extractors
//! that fail with it, cursor pagination, ETag responses and the CSRF guard
//! for cookie-authenticated writes.
//!
//! - Errors: `{"error": {"code": "...", "message": "...", "fields": {...}}}`.
//! - Lists: `{"data": [...], "next_cursor": "..." | null}` with `?limit=&cursor=`.
//! - GETs carry a weak `ETag` and honour `If-None-Match`.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

pub use super::rest_error::{V1Error, V1Result};
use crate::auth::AuthMethod;

// ---------------------------------------------------------------------------
// CSRF for cookie-authenticated API writes
// ---------------------------------------------------------------------------

/// Content types a cross-site `<form>` (or a CORS "simple request") can send.
pub fn is_simple_content_type(headers: &HeaderMap) -> bool {
    let Some(ct) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return true;
    };
    let essence = ct
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        essence.as_str(),
        "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
    )
}

/// Cookie-authenticated writes to `/api/v1` (exempt from the global CSRF
/// guard because tokens don't need it) must still prove they're ours.
pub async fn session_write_guard(request: Request, next: Next) -> Response {
    let is_session = matches!(
        request.extensions().get::<AuthMethod>(),
        Some(AuthMethod::Session)
    );
    let method = request.method().clone();
    let safe = matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
    if !is_session || safe {
        return next.run(request).await;
    }
    let headers = request.headers();
    let preflighted = matches!(method, Method::PUT | Method::PATCH | Method::DELETE)
        || !is_simple_content_type(headers);
    // Same check as the HTML side, against a non-exempt path.
    let same_origin = crate::auth::csrf::is_allowed(
        &method,
        "/api-v1-session",
        headers,
        crate::auth::session::trust_proxy(),
        None,
    );
    if preflighted || same_origin {
        return next.run(request).await;
    }
    tracing::warn!(method = %method, path = %request.uri().path(), "blocked cross-site API write");
    V1Error::forbidden_msg("Cross-site request refused. Use an API token or send JSON.")
        .into_response()
}

// ---------------------------------------------------------------------------
// Extractors with JSON errors
// ---------------------------------------------------------------------------

/// `axum::Json` whose rejections use the v1 error envelope.
pub struct Body<T>(pub T);

impl<S, T> FromRequest<S> for Body<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = V1Error;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        use axum::extract::rejection::JsonRejection;
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Body(v)),
            Err(JsonRejection::MissingJsonContentType(_)) => Err(V1Error::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "Send JSON with Content-Type: application/json.",
            )),
            Err(JsonRejection::JsonDataError(e)) => {
                Err(V1Error::validation(e.body_text(), Default::default()))
            }
            Err(e) => Err(V1Error::bad_request(e.body_text())),
        }
    }
}

/// `axum::extract::Path` with JSON errors.
pub struct Id<T>(pub T);

impl<S, T> FromRequestParts<S> for Id<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = V1Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|p| Id(p.0))
            .map_err(|_| V1Error::not_found())
    }
}

/// `axum::extract::Query` with JSON errors.
pub struct Params<T>(pub T);

impl<S, T> FromRequestParts<S> for Params<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = V1Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|q| Params(q.0))
            .map_err(|e| V1Error::bad_request(e.body_text()))
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 500;

/// `?limit=` clamped to `1..=MAX_LIMIT`.
pub fn limit(raw: Option<i64>) -> i64 {
    raw.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Opaque cursors are the last seen id, as text.
pub fn parse_cursor(raw: Option<&str>) -> V1Result<Option<i64>> {
    match raw.map(str::trim).filter(|c| !c.is_empty()) {
        None => Ok(None),
        Some(c) => c
            .parse::<i64>()
            .map(Some)
            .map_err(|_| V1Error::bad_request("Invalid cursor.")),
    }
}

#[derive(Debug, Serialize)]
pub struct ListPage<T: Serialize> {
    pub data: Vec<T>,
    pub next_cursor: Option<String>,
}

/// One page of `items` (sorted by id) after `cursor`.
pub fn paginate<T: Serialize>(
    items: Vec<T>,
    id: impl Fn(&T) -> i64,
    cursor: Option<i64>,
    limit: i64,
) -> ListPage<T> {
    let limit = usize::try_from(limit).unwrap_or(1);
    let mut data: Vec<T> = items
        .into_iter()
        .filter(|x| cursor.is_none_or(|c| id(x) > c))
        .take(limit + 1)
        .collect();
    let next_cursor = if data.len() > limit {
        data.truncate(limit);
        data.last().map(|x| id(x).to_string())
    } else {
        None
    };
    ListPage { data, next_cursor }
}

/// 200 JSON with a weak ETag; 304 when `If-None-Match` matches.
pub fn json_ok<T: Serialize>(headers: &HeaderMap, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => cached_ok(headers, body, "application/json"),
        Err(e) => V1Error::internal(e).into_response(),
    }
}

/// 200 `body` as `content_type` with a weak ETag; 304 when `If-None-Match` matches.
pub fn cached_ok(headers: &HeaderMap, body: Vec<u8>, content_type: &'static str) -> Response {
    let digest = Sha256::digest(&body);
    let etag = format!("W/\"{}\"", hex::encode(&digest[..12]));
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    let mut res = if matches {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
            body,
        )
            .into_response()
    };
    if let Ok(v) = HeaderValue::from_str(&etag) {
        res.headers_mut().insert(header::ETAG, v);
    }
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    res
}

/// 201 Created with a `Location` header.
pub fn created<T: Serialize>(location: String, value: &T) -> Response {
    let mut res = (StatusCode::CREATED, axum::Json(value)).into_response();
    if let Ok(v) = HeaderValue::from_str(&location) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res
}

pub fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}
