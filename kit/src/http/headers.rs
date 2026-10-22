//! Baseline hardening headers.

use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

/// Response extension marking a page that may be framed wherever it is
/// served (the product decides which; custom domains serve public pages at
/// `/`), so [`security_headers`] leaves framing and the CSP open for it.
#[derive(Debug, Clone, Copy)]
pub struct Embeddable;

/// Content-Security-Policy for the app. Scripts only from our own files (no
/// inline scripts or handlers anywhere; the i18n strings are a JSON data
/// block). Inline styles stay allowed for `style=` attributes and custom CSS.
pub const APP_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; connect-src 'self'; font-src 'self'; frame-ancestors 'none'; \
    base-uri 'self'; form-action 'self'";
/// The same for embeddable public status pages, minus `frame-ancestors`.
pub const PUBLIC_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; connect-src 'self'; font-src 'self'; base-uri 'self'; form-action 'self'";

/// Baseline hardening headers. Paths under the product's embeddable prefixes
/// and responses marked [`Embeddable`] stay frameable, everything else (the
/// admin UI) cannot be.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let by_path = crate::config::get()
        .embeddable_prefixes
        .iter()
        .any(|prefix| path.starts_with(prefix));
    let mut res = next.run(req).await;
    // Status pages served on a custom domain at `/` mark themselves.
    let embeddable = by_path || res.extensions().get::<Embeddable>().is_some();
    let headers = res.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    if !embeddable {
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    headers
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static(if embeddable {
            PUBLIC_CSP
        } else {
            APP_CSP
        }));
    res
}
