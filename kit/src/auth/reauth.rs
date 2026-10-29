//! A fresh confirmation for products that check credentials themselves
//! ([`CredentialMode::External`](crate::config::CredentialMode::External)).
//!
//! In the kit's own mode, anything sensitive (adding a passkey, switching
//! two-factor authentication off, creating a write token, promoting an
//! admin ...) asks for the current password again. A product that signs
//! people in on its own pages has no password to check here, so the kit asks
//! for a *grant* instead: a short-lived signed cookie that the product sets
//! after it has confirmed the user in its own way.
//!
//! The cookie is `<prefix>_reauth` (the session cookie's name with its
//! `_session` ending replaced), signed with the session key like the other
//! cookies, and holds `r1.<user id>.<session version>.<issued at>`. It lasts
//! [`TTL_SECS`], is `HttpOnly`, `SameSite=Strict`, `Path=/` and `Secure` when
//! the site is served over https. It only counts for the user and the session
//! version it was issued for, so signing out everywhere (which bumps the
//! version) ends every grant, and the kit's sign-out clears it.
//!
//! ```ignore
//! // At the end of the product's own confirmation:
//! let jar = vole_kit::auth::reauth::grant(jar, user.id, user.session_version, secure);
//! (jar, Redirect::to(&next))
//! ```

use std::sync::OnceLock;

use axum_extra::extract::cookie::{Cookie, SameSite, SignedCookieJar};
use time::Duration;

use super::CurrentUser;
use super::session::{self, SessionClaims};
use crate::config::{CredentialMode, ExternalCredentials};

/// How long a grant counts: five minutes.
pub const TTL_SECS: i64 = 5 * 60;

/// A grant stamped this far ahead of our clock still counts (clock slack).
const CLOCK_SLACK_SECS: i64 = 30;

/// The grant cookie's name: the session cookie's name with a trailing
/// `_session` replaced by `_reauth` (`kd_session` -> `kd_reauth`), or
/// `_reauth` added to it.
pub fn cookie_name() -> &'static str {
    static NAME: OnceLock<&'static str> = OnceLock::new();
    NAME.get_or_init(|| {
        let session = crate::config::get().session_cookie;
        let base = session.strip_suffix("_session").unwrap_or(session);
        Box::leak(format!("{base}_reauth").into_boxed_str())
    })
}

fn encode(user_id: i64, session_version: i64, issued_at: i64) -> String {
    format!("r1.{user_id}.{session_version}.{issued_at}")
}

fn decode(value: &str) -> Option<(i64, i64, i64)> {
    let mut parts = value.split('.');
    if parts.next()? != "r1" {
        return None;
    }
    let user_id = parts.next()?.parse().ok()?;
    let version = parts.next()?.parse().ok()?;
    let issued_at = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((user_id, version, issued_at))
}

/// Records that `user_id` has just been confirmed, for the session with
/// `session_version`. `secure` is [`secure_cookies`](super::guard::secure_cookies)
/// of the request. Call it after
/// [`login_jar`](super::guard::login_jar) if the session version changed in
/// the same response (`login_jar` clears an earlier grant).
pub fn grant(
    jar: SignedCookieJar,
    user_id: i64,
    session_version: i64,
    secure: bool,
) -> SignedCookieJar {
    grant_at(jar, user_id, session_version, session::now_unix(), secure)
}

/// [`grant`] as of the Unix time `issued_at` (for tests that need an old
/// grant).
pub fn grant_at(
    jar: SignedCookieJar,
    user_id: i64,
    session_version: i64,
    issued_at: i64,
    secure: bool,
) -> SignedCookieJar {
    let mut cookie = Cookie::new(cookie_name(), encode(user_id, session_version, issued_at));
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Strict);
    cookie.set_secure(secure);
    cookie.set_max_age(Duration::seconds(TTL_SECS));
    jar.add(cookie)
}

/// Whether the jar holds a grant for this user and session version that is
/// less than [`TTL_SECS`] old.
pub fn is_fresh(jar: &SignedCookieJar, user_id: i64, session_version: i64) -> bool {
    is_fresh_at(jar, user_id, session_version, session::now_unix())
}

/// [`is_fresh`] as of the Unix time `now`.
pub fn is_fresh_at(jar: &SignedCookieJar, user_id: i64, session_version: i64, now: i64) -> bool {
    let Some((id, version, issued_at)) = jar.get(cookie_name()).and_then(|c| decode(c.value()))
    else {
        return false;
    };
    id == user_id
        && version == session_version
        && (-CLOCK_SLACK_SECS..=TTL_SECS).contains(&(now - issued_at))
}

/// Drops the grant.
pub fn clear(jar: SignedCookieJar) -> SignedCookieJar {
    jar.remove(session::removal_cookie(cookie_name()))
}

/// Whether the signed-in user of this request has a fresh grant for the
/// session in the jar (what the kit's pages ask).
pub(crate) fn is_fresh_for(jar: &SignedCookieJar, user: &CurrentUser) -> bool {
    jar.get(crate::config::get().session_cookie)
        .and_then(|c| SessionClaims::decode(c.value()))
        .is_some_and(|claims| claims.user_id == user.id && is_fresh(jar, user.id, claims.version))
}

/// What a page of the kit shows where it would ask for the current
/// password (the template field is called `reauth`).
pub(crate) struct View {
    /// The product signs in elsewhere ([`CredentialMode::External`]): the
    /// page has no password field.
    pub external: bool,
    /// A fresh grant is held: nothing to ask.
    pub fresh: bool,
    /// Where "Confirm it's you" leads: the product's page, coming back to
    /// this one.
    pub url: String,
}

impl View {
    /// For the page at `page` (a same-site path), shown to `user`.
    pub(crate) fn new(jar: &SignedCookieJar, user: &CurrentUser, page: &str) -> Self {
        match crate::config::get().features.credentials {
            CredentialMode::Plain => Self {
                external: false,
                fresh: true,
                url: String::new(),
            },
            CredentialMode::External(ext) => Self {
                external: true,
                fresh: is_fresh_for(jar, user),
                url: ExternalCredentials::with_next(ext.reauth_path, page),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use axum_extra::extract::cookie::Key;

    use super::*;

    fn jar() -> SignedCookieJar {
        SignedCookieJar::new(Key::generate())
    }

    #[test]
    fn a_grant_is_fresh_for_its_user_and_session_only() {
        let now = 1_800_000_000;
        let jar = grant_at(jar(), 7, 3, now, false);
        assert!(is_fresh_at(&jar, 7, 3, now));
        assert!(is_fresh_at(&jar, 7, 3, now + TTL_SECS));
        assert!(!is_fresh_at(&jar, 7, 3, now + TTL_SECS + 1), "expired");
        assert!(!is_fresh_at(&jar, 8, 3, now), "another user");
        assert!(!is_fresh_at(&jar, 7, 4, now), "another session version");
        assert!(is_fresh_at(&jar, 7, 3, now - 10), "a little clock slack");
        assert!(!is_fresh_at(&jar, 7, 3, now - 600), "from the future");
    }

    #[test]
    fn no_grant_and_garbage_are_not_fresh() {
        assert!(!is_fresh_at(&jar(), 7, 3, 1));
        let forged = SignedCookieJar::new(Key::generate()).add(Cookie::new(
            cookie_name(),
            "r1.7.3.not-a-number".to_string(),
        ));
        assert!(!is_fresh_at(&forged, 7, 3, 1));
        for bad in ["", "r2.7.3.1", "r1.7.3", "r1.7.3.1.9", "7.3.1"] {
            assert_eq!(decode(bad), None, "{bad}");
        }
        assert_eq!(decode("r1.7.3.1"), Some((7, 3, 1)));
    }

    #[test]
    fn a_cookie_signed_with_another_key_is_not_a_grant() {
        use axum::http::{HeaderMap, HeaderValue, header};
        use axum::response::IntoResponse as _;

        let now = 1_800_000_000;
        let key = Key::generate();
        let own = grant_at(SignedCookieJar::new(key.clone()), 7, 3, now, false);
        let wire = |jar: SignedCookieJar| {
            let res = (jar, ()).into_response();
            let set = res.headers().get(header::SET_COOKIE).expect("set-cookie");
            let pair = set.to_str().unwrap().split(';').next().unwrap().to_string();
            let mut headers = HeaderMap::new();
            headers.insert(header::COOKIE, HeaderValue::from_str(&pair).unwrap());
            headers
        };
        // The same signed cookie is a grant for the key that signed it ...
        let headers = wire(own);
        assert!(is_fresh_at(
            &SignedCookieJar::from_headers(&headers, key),
            7,
            3,
            now
        ));
        // ... and nothing for any other key.
        assert!(!is_fresh_at(
            &SignedCookieJar::from_headers(&headers, Key::generate()),
            7,
            3,
            now
        ));
    }

    #[test]
    fn the_cookie_is_strict_http_only_and_secure_on_https() {
        let plain = grant_at(jar(), 1, 1, 1, false);
        let c = plain.get(cookie_name()).unwrap();
        assert_eq!(c.same_site(), Some(SameSite::Strict));
        assert_eq!(c.http_only(), Some(true));
        assert_eq!(c.path(), Some("/"));
        assert_eq!(c.secure(), Some(false));
        assert_eq!(c.max_age(), Some(Duration::seconds(TTL_SECS)));
        let secure = grant_at(jar(), 1, 1, 1, true);
        assert_eq!(secure.get(cookie_name()).unwrap().secure(), Some(true));
    }

    #[test]
    fn the_name_follows_the_session_cookie() {
        assert!(cookie_name().ends_with("_reauth"));
        assert!(!cookie_name().contains("session"));
    }
}
