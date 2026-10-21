//! The v1 error envelope: `{"error": {"code", "message", "fields"?}}`.
//!
//! These responses are built directly (no `ErrorInfo` tag), so the global
//! `error::negotiate` middleware passes them through untouched.

use std::collections::BTreeMap;
use std::fmt;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::api::ApiError;

pub type V1Result<T> = Result<T, V1Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V1Error {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// Field → message (validation errors).
    pub fields: BTreeMap<String, String>,
}

impl V1Error {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        V1Error {
            status,
            code,
            message: message.into(),
            fields: BTreeMap::new(),
        }
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "Not found.")
    }

    pub fn forbidden() -> Self {
        Self::forbidden_msg("You don’t have permission to do that.")
    }

    pub fn forbidden_msg(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    /// 422 with per-field messages; `message` defaults to the first one.
    pub fn validation(message: impl Into<String>, fields: BTreeMap<String, String>) -> Self {
        let mut message = message.into();
        if message.is_empty() {
            message = fields
                .values()
                .next()
                .cloned()
                .unwrap_or_else(|| "Invalid request.".into());
        }
        V1Error {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "validation_failed",
            message,
            fields,
        }
    }

    /// Validation error from `(field, message)` pairs.
    pub fn fields<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        let fields: BTreeMap<String, String> = pairs
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Self::validation("", fields)
    }

    pub fn field(field: &str, message: impl Into<String>) -> Self {
        Self::fields([(field.to_string(), message.into())])
    }

    pub fn internal(err: impl fmt::Display) -> Self {
        tracing::error!(error = %err, "api request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Something went wrong on our side. Please try again.",
        )
    }
}

impl fmt::Display for V1Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for V1Error {}

impl IntoResponse for V1Error {
    fn into_response(self) -> Response {
        let mut error = json!({ "code": self.code, "message": self.message });
        if !self.fields.is_empty() {
            error["fields"] = json!(self.fields);
        }
        (self.status, Json(json!({ "error": error }))).into_response()
    }
}

impl From<ApiError> for V1Error {
    fn from(err: ApiError) -> Self {
        match err {
            ApiError::NotFound => Self::not_found(),
            ApiError::Forbidden => Self::forbidden(),
            ApiError::BadRequest(msg) => Self::bad_request(msg),
            ApiError::Unauthorized => Self::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Sign in or send an API token.",
            ),
            ApiError::Internal(detail) => Self::internal(detail),
        }
    }
}

impl From<crate::db::DbError> for V1Error {
    fn from(err: crate::db::DbError) -> Self {
        ApiError::from(err).into()
    }
}

impl From<rusqlite::Error> for V1Error {
    fn from(err: rusqlite::Error) -> Self {
        ApiError::from(err).into()
    }
}
