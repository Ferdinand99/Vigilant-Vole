//! Session and pending-2FA cookies, reverse-proxy awareness and `?next=`
//! sanitising. Pure functions; the request plumbing is in `api::auth`.
//!
//! Both cookies live in the `SignedCookieJar` (HMAC with the instance key),
//! so their contents can't be forged; they are not secret.
//!
//! Session value: `v2.<user id>.<session_version>.<last seen>.<issued at>`
//! (unix seconds).
//! - The server enforces the 30-day idle timeout from the signed `last seen`
//!   and the 30-day absolute lifetime from `issued at` (the login); the
//!   cookie's Max-Age is only a hint to the browser.
//! - `last seen` slides forward at most once per [`REFRESH_AFTER_SECS`];
//!   `issued at` never moves, so even a busy session ends 30 days after login.
//! - Legacy `v1.<user id>.<version>.<last seen>` cookies are still accepted,
//!   counting from their last refresh (so upgrading signs nobody out, and
//!   they still end within 30 days); the next refresh re-issues them as v2.
//! - Bumping `users.session_version` invalidates every cookie issued before.

use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;

use axum::http::HeaderMap;
use axum_extra::extract::cookie::{Cookie, SameSite};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use time::Duration;

use super::SESSION_COOKIE;

/// Sessions end after this long without a request.
pub const IDLE_TIMEOUT_SECS: i64 = 30 * 24 * 60 * 60;
/// Sessions end this long after login, however active they are.
pub const ABSOLUTE_TIMEOUT_SECS: i64 = 30 * 24 * 60 * 60;
/// Tolerated clock skew for timestamps "from the future".
const CLOCK_SLACK_SECS: i64 = 300;
/// Re-issue the session cookie (sliding expiry) at most this often.
pub const REFRESH_AFTER_SECS: i64 = 60 * 60;
/// Password-verified, waiting for the second factor.
pub const PENDING_2FA_COOKIE: &str = "vv_2fa";
pub const PENDING_2FA_TTL_SECS: i64 = 5 * 60;

/// `VV_TRUST_PROXY=1` (or true/yes/on): honour `X-Forwarded-Proto`,
/// `X-Forwarded-Host` and `X-Forwarded-For` / `X-Real-IP` from a reverse proxy.
/// Off by default, because a directly exposed instance must not believe
/// client-supplied forwarding headers.
pub static TRUST_PROXY: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("VV_TRUST_PROXY")
        .map(|v| crate::db::settings::parse_bool(&v))
        .unwrap_or(false)
});

pub fn trust_proxy() -> bool {
    *TRUST_PROXY
}

/// `VV_SETUP_TOKEN`: when set (non-empty), the first-run `/setup` page only
/// opens with `?token=<value>`, so whoever reaches a fresh instance first
/// can't claim it.
pub static SETUP_TOKEN: LazyLock<Option<String>> = LazyLock::new(|| {
    std::env::var("VV_SETUP_TOKEN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
});

/// Whether `given` unlocks setup when `expected` is the configured token
/// (`None` = no token required). Compares in constant time.
pub fn setup_token_matches(expected: Option<&str>, given: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let given = given.map(str::trim).unwrap_or("");
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionClaims {
    pub user_id: i64,
    pub version: i64,
    /// Unix seconds of the last refresh (login or sliding renewal).
    pub last_seen: i64,
    /// Unix seconds of the login; never changes while the session lives.
    pub issued_at: i64,
}

impl SessionClaims {
    /// A brand-new session (login).
    pub fn new(user_id: i64, version: i64, now: i64) -> Self {
        Self {
            user_id,
            version,
            last_seen: now,
            issued_at: now,
        }
    }

    /// The same session with its idle timer slid forward to `now`.
    pub fn refreshed(&self, now: i64) -> Self {
        Self { last_seen: now, ..*self }
    }

    pub fn encode(&self) -> String {
        format!("v2.{}.{}.{}.{}", self.user_id, self.version, self.last_seen, self.issued_at)
    }

    pub fn decode(value: &str) -> Option<Self> {
        let mut parts = value.split('.');
        let tag = parts.next()?;
        if tag != "v1" && tag != "v2" {
            return None;
        }
        let user_id = parts.next()?.parse().ok()?;
        let version = parts.next()?.parse().ok()?;
        let last_seen = parts.next()?.parse().ok()?;
        // v1 cookies predate the absolute lifetime: count it from their last refresh.
        let issued_at = if tag == "v2" { parts.next()?.parse().ok()? } else { last_seen };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            user_id,
            version,
            last_seen,
            issued_at,
        })
    }

    /// Neither idle- nor lifetime-expired (and not from the future beyond a
    /// little clock slack).
    pub fn is_live(&self, now: i64) -> bool {
        let idle = now - self.last_seen;
        let lifetime = now - self.issued_at;
        (-CLOCK_SLACK_SECS..=IDLE_TIMEOUT_SECS).contains(&idle)
            && (-CLOCK_SLACK_SECS..=ABSOLUTE_TIMEOUT_SECS).contains(&lifetime)
            && self.issued_at <= self.last_seen
    }

    /// Browser-side Max-Age: whichever of the two limits comes first.
    fn max_age_secs(&self) -> i64 {
        let remaining = self.issued_at + ABSOLUTE_TIMEOUT_SECS - self.last_seen;
        remaining.clamp(0, IDLE_TIMEOUT_SECS)
    }

    pub fn needs_refresh(&self, now: i64) -> bool {
        now - self.last_seen >= REFRESH_AFTER_SECS
    }
}

/// The step between password and second factor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLogin {
    pub user_id: i64,
    pub version: i64,
    pub issued_at: i64,
    /// Already sanitised redirect target.
    pub next: String,
}

impl PendingLogin {
    pub fn encode(&self) -> String {
        format!(
            "p1.{}.{}.{}.{}",
            self.user_id,
            self.version,
            self.issued_at,
            URL_SAFE_NO_PAD.encode(self.next.as_bytes())
        )
    }

    pub fn decode(value: &str) -> Option<Self> {
        let mut parts = value.split('.');
        if parts.next()? != "p1" {
            return None;
        }
        let user_id = parts.next()?.parse().ok()?;
        let version = parts.next()?.parse().ok()?;
        let issued_at = parts.next()?.parse().ok()?;
        let next = String::from_utf8(URL_SAFE_NO_PAD.decode(parts.next()?).ok()?).ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            user_id,
            version,
            issued_at,
            next: sanitize_next(Some(&next)),
        })
    }

    pub fn is_live(&self, now: i64) -> bool {
        let age = now - self.issued_at;
        (-30..=PENDING_2FA_TTL_SECS).contains(&age)
    }
}

fn base_cookie(name: &'static str, value: String, secure: bool, max_age_secs: i64) -> Cookie<'static> {
    let mut cookie = Cookie::new(name, value);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(secure);
    cookie.set_max_age(Duration::seconds(max_age_secs));
    cookie
}

pub fn session_cookie(claims: &SessionClaims, secure: bool) -> Cookie<'static> {
    base_cookie(SESSION_COOKIE, claims.encode(), secure, claims.max_age_secs())
}

pub fn pending_cookie(pending: &PendingLogin, secure: bool) -> Cookie<'static> {
    base_cookie(PENDING_2FA_COOKIE, pending.encode(), secure, PENDING_2FA_TTL_SECS)
}

/// A cookie that, passed to `jar.remove`, deletes `name` (path must match).
pub fn removal_cookie(name: &'static str) -> Cookie<'static> {
    let mut cookie = Cookie::from(name);
    cookie.set_path("/");
    cookie
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Whether the browser reached us over HTTPS. The app itself only speaks
/// HTTP, so this is only ever true behind a trusted TLS-terminating proxy.
pub fn is_https(headers: &HeaderMap, trust_proxy: bool) -> bool {
    trust_proxy
        && header_str(headers, "x-forwarded-proto")
            .and_then(|v| v.split(',').next())
            .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
}

/// Client IP for rate limiting. Behind a trusted proxy, the address the proxy
/// saw (right-most `X-Forwarded-For` entry, or `X-Real-IP`); otherwise the
/// TCP peer.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>, trust_proxy: bool) -> Option<IpAddr> {
    if trust_proxy {
        let forwarded = header_str(headers, "x-forwarded-for")
            .and_then(|v| v.rsplit(',').next())
            .and_then(|ip| ip.trim().parse().ok());
        let real = || header_str(headers, "x-real-ip").and_then(|ip| ip.trim().parse().ok());
        if let Some(ip) = forwarded.or_else(real) {
            return Some(ip);
        }
    }
    peer.map(|p| p.ip())
}

/// Turns an untrusted `next` parameter into a same-site path, defaulting to
/// `/`. Only plain absolute paths survive: no scheme, no `//host`, no
/// backslashes (browsers treat `/\host` as `//host`), no control characters,
/// and never back into the auth pages.
pub fn sanitize_next(raw: Option<&str>) -> String {
    let Some(next) = raw.map(str::trim) else {
        return "/".into();
    };
    let safe = next.starts_with('/')
        && !next.starts_with("//")
        && next.len() <= 2048
        && !next.chars().any(|c| c == '\\' || c.is_control() || c.is_whitespace());
    let auth_page = ["/login", "/logout", "/setup"]
        .iter()
        .any(|p| next == *p || next.starts_with(&format!("{p}/")) || next.starts_with(&format!("{p}?")));
    if safe && !auth_page { next.to_string() } else { "/".into() }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn session_claims_round_trip() {
        let c = SessionClaims::new(7, 3, 1_700_000_000);
        assert_eq!(SessionClaims::decode(&c.encode()), Some(c));
    }

    #[test]
    fn legacy_and_garbage_session_values_are_rejected() {
        for v in ["7", "", "v1.7.3", "v1.7.3.1.9", "v2.7.3.1", "v2.7.3.1.2.3", "v3.7.3.1.1", "v1.x.3.1"] {
            assert_eq!(SessionClaims::decode(v), None, "{v}");
        }
    }

    #[test]
    fn v1_cookies_still_work_and_count_lifetime_from_their_last_refresh() {
        let c = SessionClaims::decode("v1.7.3.1000").unwrap();
        assert_eq!((c.user_id, c.version, c.last_seen, c.issued_at), (7, 3, 1000, 1000));
        assert!(c.is_live(1000 + IDLE_TIMEOUT_SECS));
        // Refreshing upgrades to v2 without resetting the lifetime.
        let up = c.refreshed(5000);
        assert!(up.encode().starts_with("v2.") && up.issued_at == 1000);
        assert!(!up.is_live(1001 + ABSOLUTE_TIMEOUT_SECS));
    }

    #[test]
    fn active_sessions_still_end_after_the_absolute_lifetime() {
        let mut c = SessionClaims::new(1, 1, 0);
        // Busy user: refreshed every hour, never idle.
        let mut now = 0;
        while now < ABSOLUTE_TIMEOUT_SECS {
            now += REFRESH_AFTER_SECS;
            assert!(c.is_live(now));
            c = c.refreshed(now);
        }
        assert!(!c.is_live(ABSOLUTE_TIMEOUT_SECS + 1));
        assert_eq!(c.issued_at, 0);
    }

    #[test]
    fn cookie_max_age_never_outlives_the_session() {
        let c = SessionClaims::new(1, 1, 0).refreshed(ABSOLUTE_TIMEOUT_SECS - 60);
        let cookie = session_cookie(&c, false);
        assert_eq!(cookie.max_age(), Some(Duration::seconds(60)));
    }

    #[test]
    fn session_expires_after_idle_timeout() {
        let c = SessionClaims::new(1, 1, 1000);
        assert!(c.is_live(1000 + IDLE_TIMEOUT_SECS));
        assert!(!c.is_live(1001 + IDLE_TIMEOUT_SECS));
    }

    #[test]
    fn session_from_the_future_is_rejected() {
        assert!(!SessionClaims::new(1, 1, 10_000).is_live(1000));
    }

    #[test]
    fn session_refreshes_hourly() {
        let c = SessionClaims::new(1, 1, 0);
        assert!(!c.needs_refresh(REFRESH_AFTER_SECS - 1));
        assert!(c.needs_refresh(REFRESH_AFTER_SECS));
    }

    #[test]
    fn pending_login_round_trips_and_expires() {
        let p = PendingLogin {
            user_id: 4,
            version: 2,
            issued_at: 100,
            next: "/monitors/3?tab=events".into(),
        };
        let decoded = PendingLogin::decode(&p.encode()).unwrap();
        assert_eq!(decoded, p);
        assert!(decoded.is_live(100 + PENDING_2FA_TTL_SECS));
        assert!(!decoded.is_live(101 + PENDING_2FA_TTL_SECS));
    }

    #[test]
    fn next_keeps_same_site_paths() {
        assert_eq!(sanitize_next(Some("/monitors/4?tab=events")), "/monitors/4?tab=events");
        assert_eq!(sanitize_next(Some("/settings/security")), "/settings/security");
    }

    #[test]
    fn next_rejects_offsite_and_tricky_targets() {
        for bad in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "/%0d%0aSet-Cookie:x".replace("%0d%0a", "\r\n").as_str(),
            "javascript:alert(1)",
            "evil.example",
            "/ /evil",
            "/login",
            "/login?next=/x",
            "/logout",
            "/setup",
        ] {
            assert_eq!(sanitize_next(Some(bad)), "/", "{bad:?}");
        }
        assert_eq!(sanitize_next(None), "/");
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn https_detection_requires_trusted_proxy() {
        let h = headers(&[("x-forwarded-proto", "https")]);
        assert!(is_https(&h, true));
        assert!(!is_https(&h, false));
        assert!(!is_https(&headers(&[("x-forwarded-proto", "http")]), true));
    }

    #[test]
    fn client_ip_ignores_forwarded_for_unless_trusted() {
        let peer: SocketAddr = "10.0.0.2:5555".parse().unwrap();
        let h = headers(&[("x-forwarded-for", "6.6.6.6, 203.0.113.9")]);
        assert_eq!(client_ip(&h, Some(peer), false), Some(peer.ip()));
        assert_eq!(client_ip(&h, Some(peer), true), "203.0.113.9".parse().ok());
    }

    #[test]
    fn setup_token_is_only_required_when_configured() {
        assert!(setup_token_matches(None, None));
        assert!(setup_token_matches(None, Some("anything")));
        assert!(setup_token_matches(Some("s3cret-token"), Some(" s3cret-token ")));
        for bad in [None, Some(""), Some("s3cret"), Some("s3cret-tokeN"), Some("s3cret-token-x")] {
            assert!(!setup_token_matches(Some("s3cret-token"), bad), "{bad:?}");
        }
    }

    #[test]
    fn session_cookie_flags() {
        let c = session_cookie(&SessionClaims::new(1, 1, 0), true);
        assert_eq!(
            (c.http_only(), c.secure(), c.same_site(), c.path()),
            (Some(true), Some(true), Some(SameSite::Lax), Some("/"))
        );
    }
}
