//! CSRF policy: state-changing requests must prove they come from our own
//! pages. Accepted proofs, in order:
//! 1. `HX-Request: true` — a custom header, which a cross-site page can only
//!    send after a CORS preflight we never approve;
//! 2. an `Origin` header naming this site;
//! 3. without `Origin`, a `Referer` naming this site.
//!
//! "This site" is the request's `Host`, the `X-Forwarded-Host` (only behind a
//! trusted proxy) or the configured base URL. Requests with neither header
//! are refused. Machine endpoints that authenticate with tokens instead of
//! cookies (push, agents, REST API, metrics, badges) are exempt.

use axum::http::{HeaderMap, Method};
use url::Url;

/// Prefixes of paths that never rely on the session cookie.
const EXEMPT_PREFIXES: &[&str] = &["/api/push", "/api/agent", "/api/v1", "/api/badge", "/metrics"];

/// Is `path` one of the token/public machine endpoints?
pub fn is_exempt_path(path: &str) -> bool {
    EXEMPT_PREFIXES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

fn is_safe_method(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE)
}

/// `(host, port)` with the port dropped when it is the scheme default.
type Authority = (String, Option<u16>);

fn origin_authority(url: &str) -> Option<(String, Authority)> {
    let url = Url::parse(url).ok()?;
    let scheme = url.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    // `Url::port()` is already `None` for the scheme's default port.
    Some((scheme, (host, url.port())))
}

/// Parses a `Host`-style value (`example.com`, `example.com:8080`,
/// `[::1]:3001`) relative to `scheme`, normalising the default port away.
fn host_authority(host: &str, scheme: &str) -> Option<Authority> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    let url = Url::parse(&format!("{scheme}://{host}/")).ok()?;
    Some((url.host_str()?.to_ascii_lowercase(), url.port()))
}

/// Decides whether a request may proceed.
///
/// `base_url` is the configured public URL (optional, may be looked up lazily
/// by the caller only when the cheap checks fail).
pub fn is_allowed(
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    trust_proxy: bool,
    base_url: Option<&str>,
) -> bool {
    if is_safe_method(method) || is_exempt_path(path) {
        return true;
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if header("hx-request").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        return true;
    }

    // Origin wins when present; "null" (sandboxed/opaque) is never ours.
    let source = match header("origin") {
        Some(origin) => origin,
        None => match header("referer") {
            Some(referer) => referer,
            None => return false,
        },
    };
    let Some((scheme, source)) = origin_authority(source) else {
        return false;
    };

    let mut allowed: Vec<Authority> = Vec::with_capacity(3);
    if let Some(host) = header("host").and_then(|h| host_authority(h, &scheme)) {
        allowed.push(host);
    }
    if trust_proxy
        && let Some(host) = header("x-forwarded-host")
            .and_then(|h| h.split(',').next())
            .and_then(|h| host_authority(h, &scheme))
    {
        allowed.push(host);
    }
    if let Some((base_scheme, base)) = base_url.and_then(origin_authority)
        && base_scheme == scheme
    {
        allowed.push(base);
    }
    allowed.contains(&source)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn h(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, HeaderValue::from_static(v));
        }
        map
    }

    fn post(path: &str, headers: &HeaderMap) -> bool {
        is_allowed(&Method::POST, path, headers, false, None)
    }

    #[test]
    fn safe_methods_pass_without_proof() {
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(is_allowed(&m, "/settings/users", &HeaderMap::new(), false, None));
        }
    }

    #[test]
    fn unsafe_methods_without_proof_are_blocked() {
        for m in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(!is_allowed(&m, "/monitors/1", &h(&[("host", "vole.local")]), false, None));
        }
    }

    #[test]
    fn htmx_header_passes() {
        assert!(post("/monitors/1/pause", &h(&[("hx-request", "true")])));
    }

    #[test]
    fn same_origin_passes() {
        assert!(post("/login", &h(&[("host", "vole.local:3001"), ("origin", "http://vole.local:3001")])));
    }

    #[test]
    fn default_ports_are_normalised() {
        assert!(post("/login", &h(&[("host", "vole.local:443"), ("origin", "https://vole.local")])));
        assert!(post("/login", &h(&[("host", "vole.local"), ("origin", "http://vole.local:80")])));
    }

    #[test]
    fn host_comparison_ignores_case() {
        assert!(post("/login", &h(&[("host", "Vole.Local"), ("origin", "http://vole.local")])));
    }

    #[test]
    fn cross_origin_is_blocked() {
        assert!(!post("/login", &h(&[("host", "vole.local"), ("origin", "http://evil.example")])));
    }

    #[test]
    fn different_port_is_cross_origin() {
        assert!(!post("/login", &h(&[("host", "vole.local:3001"), ("origin", "http://vole.local:8080")])));
    }

    #[test]
    fn lookalike_suffix_is_blocked() {
        assert!(!post("/login", &h(&[("host", "vole.local"), ("origin", "http://vole.local.evil.example")])));
    }

    #[test]
    fn null_origin_is_blocked() {
        assert!(!post("/login", &h(&[("host", "vole.local"), ("origin", "null")])));
    }

    #[test]
    fn origin_takes_precedence_over_referer() {
        let headers = h(&[
            ("host", "vole.local"),
            ("origin", "http://evil.example"),
            ("referer", "http://vole.local/settings"),
        ]);
        assert!(!post("/settings/general", &headers));
    }

    #[test]
    fn referer_is_the_fallback() {
        assert!(post("/logout", &h(&[("host", "vole.local"), ("referer", "http://vole.local/settings")])));
        assert!(!post("/logout", &h(&[("host", "vole.local"), ("referer", "http://evil.example/x")])));
    }

    #[test]
    fn forwarded_host_counts_only_behind_trusted_proxy() {
        let headers = h(&[
            ("host", "127.0.0.1:3001"),
            ("x-forwarded-host", "up.example.com"),
            ("origin", "https://up.example.com"),
        ]);
        assert!(is_allowed(&Method::POST, "/login", &headers, true, None));
        assert!(!is_allowed(&Method::POST, "/login", &headers, false, None));
    }

    #[test]
    fn configured_base_url_is_trusted() {
        let headers = h(&[("host", "127.0.0.1:3001"), ("origin", "https://up.example.com")]);
        assert!(is_allowed(&Method::POST, "/login", &headers, false, Some("https://up.example.com/")));
        assert!(!is_allowed(&Method::POST, "/login", &headers, false, Some("http://up.example.com")));
    }

    #[test]
    fn machine_endpoints_are_exempt() {
        for path in ["/api/push/abc", "/api/agent/v1/results", "/api/v1/monitors", "/metrics", "/api/badge/1/status"] {
            assert!(post(path, &HeaderMap::new()), "{path}");
        }
    }

    #[test]
    fn exemption_matches_whole_segments() {
        assert!(!is_exempt_path("/api/v10/x"));
        assert!(!is_exempt_path("/metricsx"));
        assert!(!is_exempt_path("/api/pushy"));
    }
}
