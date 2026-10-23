//! Baseline hardening headers.
//!
//! [`security_headers`] gives every response `X-Content-Type-Options`,
//! `Referrer-Policy`, and by default the app's CSP and `X-Frame-Options:
//! DENY`. A route that isn't an app page marks its responses with a
//! [`HeaderPolicy`] to get the headers that fit it instead:
//!
//! ```ignore
//! // Every response of these routes:
//! let ingest = Router::new()
//!     .route("/ingest/v1/event", post(event))
//!     .route_layer(axum::middleware::map_response(headers::machine));
//! // Or one response:
//! (HeaderPolicy::PublicAsset, [(header::CONTENT_TYPE, "text/javascript")], SNIPPET)
//! ```

use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponseParts, Response, ResponseParts};

/// Response extension marking a page that may be framed wherever it is
/// served (the product decides which; custom domains serve public pages at
/// `/`), so [`security_headers`] leaves framing and the CSP open for it.
/// The same as [`HeaderPolicy::Embeddable`].
#[derive(Debug, Clone, Copy)]
pub struct Embeddable;

/// Which hardening headers [`security_headers`] gives a response. Put it in
/// the response's extensions (it is an [`IntoResponseParts`]), or mark a
/// whole router with [`public_asset`], [`machine`] or [`embeddable`] and
/// `axum::middleware::map_response`. Every policy keeps
/// `X-Content-Type-Options: nosniff` and `Referrer-Policy: same-origin`; a
/// CSP or CORP header the handler set itself is never replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderPolicy {
    /// The default: the app's CSP ([`APP_CSP`]) and `X-Frame-Options: DENY`.
    App,
    /// A page other sites may frame: [`PUBLIC_CSP`], no `X-Frame-Options`.
    Embeddable,
    /// A file other sites load (a JavaScript snippet, an image, a badge):
    /// no CSP and no `X-Frame-Options`, and
    /// `Cross-Origin-Resource-Policy: cross-origin`, so pages with
    /// `Cross-Origin-Embedder-Policy` can load it too.
    PublicAsset,
    /// An endpoint for programs, not browsers (an ingest or webhook
    /// endpoint, a JSON API): [`MACHINE_CSP`], which allows nothing, and no
    /// `X-Frame-Options` (`frame-ancestors 'none'` covers it).
    Machine,
}

impl IntoResponseParts for HeaderPolicy {
    type Error = std::convert::Infallible;

    fn into_response_parts(self, mut res: ResponseParts) -> Result<ResponseParts, Self::Error> {
        res.extensions_mut().insert(self);
        Ok(res)
    }
}

/// Marks a response as [`HeaderPolicy::PublicAsset`]; for
/// `axum::middleware::map_response`.
pub async fn public_asset(res: Response) -> Response {
    with_policy(res, HeaderPolicy::PublicAsset)
}

/// Marks a response as [`HeaderPolicy::Machine`]; for
/// `axum::middleware::map_response`.
pub async fn machine(res: Response) -> Response {
    with_policy(res, HeaderPolicy::Machine)
}

/// Marks a response as [`HeaderPolicy::Embeddable`]; for
/// `axum::middleware::map_response`.
pub async fn embeddable(res: Response) -> Response {
    with_policy(res, HeaderPolicy::Embeddable)
}

fn with_policy(mut res: Response, policy: HeaderPolicy) -> Response {
    res.extensions_mut().insert(policy);
    res
}

/// Content-Security-Policy for the app. Scripts only from our own files (no
/// inline scripts or handlers anywhere; the i18n strings are a JSON data
/// block). Inline styles stay allowed for `style=` attributes and custom CSS.
pub const APP_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; connect-src 'self'; font-src 'self'; frame-ancestors 'none'; \
    base-uri 'self'; form-action 'self'";
/// The same for embeddable public status pages, minus `frame-ancestors`.
pub const PUBLIC_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; connect-src 'self'; font-src 'self'; base-uri 'self'; form-action 'self'";
/// For [`HeaderPolicy::Machine`]: a response no browser should render or
/// frame.
pub const MACHINE_CSP: &str = "default-src 'none'; frame-ancestors 'none'";

/// Baseline hardening headers. Paths under the product's embeddable prefixes
/// and responses marked [`Embeddable`] stay frameable, everything else (the
/// admin UI) cannot be. A [`HeaderPolicy`] on the response chooses another
/// set.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let by_path = crate::config::get()
        .embeddable_prefixes
        .iter()
        .any(|prefix| path.starts_with(prefix));
    let mut res = next.run(req).await;
    let policy = match res.extensions().get::<HeaderPolicy>() {
        Some(policy) => *policy,
        // Status pages served on a custom domain at `/` mark themselves.
        None if by_path || res.extensions().get::<Embeddable>().is_some() => {
            HeaderPolicy::Embeddable
        }
        None => HeaderPolicy::App,
    };
    let headers = res.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    if policy == HeaderPolicy::App {
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    let csp = match policy {
        HeaderPolicy::App => Some(APP_CSP),
        HeaderPolicy::Embeddable => Some(PUBLIC_CSP),
        HeaderPolicy::Machine => Some(MACHINE_CSP),
        HeaderPolicy::PublicAsset => None,
    };
    if let Some(csp) = csp {
        headers
            .entry(header::CONTENT_SECURITY_POLICY)
            .or_insert(HeaderValue::from_static(csp));
    }
    if policy == HeaderPolicy::PublicAsset {
        headers
            .entry("cross-origin-resource-policy")
            .or_insert(HeaderValue::from_static("cross-origin"));
    }
    res
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, StatusCode};
    use axum::middleware::{from_fn, map_response};
    use axum::routing::get;
    use tower::ServiceExt as _;

    use super::*;

    async fn headers_of(path: &str) -> HeaderMap {
        let app = Router::new()
            .route("/page", get(|| async { "page" }))
            .route(
                "/one",
                get(|| async { (HeaderPolicy::PublicAsset, "asset") }),
            )
            .route(
                "/own-csp",
                get(|| async {
                    (
                        HeaderPolicy::Machine,
                        [(header::CONTENT_SECURITY_POLICY, "sandbox")],
                        "x",
                    )
                }),
            )
            .merge(
                Router::new()
                    .route("/js/k.js", get(|| async { "js" }))
                    .route_layer(map_response(public_asset)),
            )
            .merge(
                Router::new()
                    .route("/ingest", get(|| async { StatusCode::ACCEPTED }))
                    .route_layer(map_response(machine)),
            )
            .merge(
                Router::new()
                    .route("/widget", get(|| async { "w" }))
                    .route_layer(map_response(embeddable)),
            )
            .layer(from_fn(security_headers));
        let res = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        res.headers().clone()
    }

    fn value<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
        h.get(name).and_then(|v| v.to_str().ok())
    }

    #[tokio::test]
    async fn app_pages_get_the_app_policy() {
        let h = headers_of("/page").await;
        assert_eq!(value(&h, "content-security-policy"), Some(APP_CSP));
        assert_eq!(value(&h, "x-frame-options"), Some("DENY"));
        assert_eq!(value(&h, "x-content-type-options"), Some("nosniff"));
        assert_eq!(value(&h, "referrer-policy"), Some("same-origin"));
        assert_eq!(value(&h, "cross-origin-resource-policy"), None);
    }

    #[tokio::test]
    async fn public_assets_can_be_loaded_anywhere() {
        for path in ["/js/k.js", "/one"] {
            let h = headers_of(path).await;
            assert_eq!(value(&h, "content-security-policy"), None, "{path}");
            assert_eq!(value(&h, "x-frame-options"), None, "{path}");
            assert_eq!(value(&h, "x-content-type-options"), Some("nosniff"));
            assert_eq!(
                value(&h, "cross-origin-resource-policy"),
                Some("cross-origin")
            );
        }
    }

    #[tokio::test]
    async fn machine_endpoints_get_a_policy_that_allows_nothing() {
        let h = headers_of("/ingest").await;
        assert_eq!(value(&h, "content-security-policy"), Some(MACHINE_CSP));
        assert_eq!(value(&h, "x-frame-options"), None);
        assert_eq!(value(&h, "x-content-type-options"), Some("nosniff"));
        let own = headers_of("/own-csp").await;
        assert_eq!(value(&own, "content-security-policy"), Some("sandbox"));
    }

    #[tokio::test]
    async fn embeddable_routes_may_be_framed() {
        let h = headers_of("/widget").await;
        assert_eq!(value(&h, "content-security-policy"), Some(PUBLIC_CSP));
        assert_eq!(value(&h, "x-frame-options"), None);
    }
}
