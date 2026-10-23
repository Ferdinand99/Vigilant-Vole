//! HTTP building blocks: errors, security headers, templates, static files,
//! request budgets ([`ratelimit`]) and live updates ([`live`]).

pub mod assets;
pub mod error;
pub mod headers;
pub mod live;
pub mod ratelimit;
pub mod rest;
mod rest_error;

use askama::Template;
use axum::http::HeaderValue;
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

/// `HX-Trigger` header value for a toast (+ optionally closing the open sheet):
/// the `vv:toast` and `vv:close-sheet` events of the design system's `app.js`.
pub fn toast_trigger(message: &str, kind: &str, close_sheet: bool) -> HeaderValue {
    let mut v = serde_json::json!({ "vv:toast": { "message": message, "kind": kind } });
    if close_sheet {
        v["vv:close-sheet"] = serde_json::Value::Bool(true);
    }
    HeaderValue::from_str(&ascii_json(&v.to_string()))
        .unwrap_or_else(|_| HeaderValue::from_static("{}"))
}

/// Escapes every non-ASCII character of a JSON text as `\uXXXX`, so it fits
/// in an HTTP header without being mangled.
pub fn ascii_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() && !c.is_ascii_control() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}
