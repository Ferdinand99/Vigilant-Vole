//! A fresh confirmation for products that check credentials themselves
//! ([`CredentialMode::External`](crate::config::CredentialMode::External)).
//!
//! In the kit's own mode, anything sensitive (adding a passkey, switching
//! two-factor authentication off, creating a write token, promoting an
//! admin ...) asks for the current password again. A product that signs
//! people in on its own pages has no password to check here, so the kit asks
//! for a *grant* instead: a short-lived cookie that the product sets after it
//! has confirmed the user in its own way.
//!
//! The cookie is `<prefix>_reauth` (the session cookie's name with its
//! `_session` ending replaced). It is signed with the session key like the
//! other cookies, and its value is also *sealed* for one purpose: encrypted
//! and authenticated under a key derived from the session key for the grant
//! alone (the [`crypto`](crate::crypto) purpose `reauth-grant`, the same
//! per-purpose keys the kit uses for secrets at rest). Inside is
//! `r2.<user id>.<session version>.<session issued at>.<granted at>`.
//!
//! * **Purpose.** The signature does not cover the cookie's name, so any
//!   other value the instance key has signed could be replayed under the
//!   grant's name; a product that signs text a visitor chose (a "remember
//!   me" name, an invitation code ...) would hand out grants. Only a value
//!   sealed for `reauth-grant` opens here, and no other cookie is.
//! * **Session.** It counts for one user, one session version and one
//!   session (the session cookie's `issued at`, which a login sets and
//!   nothing moves), read from the session cookie in the jar. A grant from
//!   a sign-in on another device, or from before this session began, is
//!   no good, and signing out everywhere (which bumps the version) ends
//!   every grant. The kit's sign-in and sign-out clear it too.
//! * **Time.** It lasts [`TTL_SECS`]. It is `HttpOnly`, `SameSite=Strict`,
//!   `Path=/` and `Secure` when the site is served over https.
//! * **Use.** The most sensitive changes (adding a passkey, making someone
//!   an administrator, creating a write token) use it up: the kit clears it
//!   when they succeed, so the next one needs a new confirmation. The others
//!   keep to the five minutes. Clearing removes the cookie from the browser;
//!   it cannot recall a copy somebody kept, which still expires on its own.
//!
//! ```ignore
//! // At the end of the product's own confirmation (the session cookie is
//! // in `jar`, also right after `login_jar`):
//! let jar = vole_kit::auth::reauth::grant(jar, &core.session_key, secure_cookies(&headers));
//! (jar, Redirect::to(&next))
//! ```

use std::sync::OnceLock;

use axum_extra::extract::cookie::{Cookie, Key, SameSite, SignedCookieJar};
use time::Duration;

use super::CurrentUser;
use super::session::{self, SessionClaims};
use crate::config::{CredentialMode, ExternalCredentials};
use crate::crypto::Sealer;

/// How long a grant counts: five minutes.
pub const TTL_SECS: i64 = 5 * 60;

/// A grant stamped this far ahead of our clock still counts (clock slack).
const CLOCK_SLACK_SECS: i64 = 30;

/// The purpose the grant is sealed for ([`Sealer::new`]).
const PURPOSE: &str = "reauth-grant";

/// Names the format of the sealed value.
const TAG: &str = "r2";

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

/// What a grant says, once opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Grant {
    user_id: i64,
    session_version: i64,
    session_issued_at: i64,
    granted_at: i64,
}

impl Grant {
    fn seal(&self, key: &Key) -> String {
        Sealer::new(key, PURPOSE).seal(&format!(
            "{TAG}.{}.{}.{}.{}",
            self.user_id, self.session_version, self.session_issued_at, self.granted_at
        ))
    }

    /// `None` for anything that is not a grant sealed with this key.
    fn open(key: &Key, value: &str) -> Option<Self> {
        let plain = Sealer::new(key, PURPOSE).open(value).ok()?;
        let mut parts = plain.split('.');
        if parts.next()? != TAG {
            return None;
        }
        let grant = Self {
            user_id: parts.next()?.parse().ok()?,
            session_version: parts.next()?.parse().ok()?,
            session_issued_at: parts.next()?.parse().ok()?,
            granted_at: parts.next()?.parse().ok()?,
        };
        parts.next().is_none().then_some(grant)
    }

    /// Whether it was given for exactly this session, and is not too old.
    fn counts_for(&self, session: &SessionClaims, now: i64) -> bool {
        self.user_id == session.user_id
            && self.session_version == session.version
            && self.session_issued_at == session.issued_at
            && (-CLOCK_SLACK_SECS..=TTL_SECS).contains(&(now - self.granted_at))
    }
}

/// Whether `value` (a cookie's value, as the jar verified it) is a grant of
/// this instance. What [`guard`](super::guard) uses to pick one of several
/// grant cookies.
pub(crate) fn is_grant(key: &Key, value: &str) -> bool {
    Grant::open(key, value).is_some()
}

/// The claims of the session cookie in the jar (also one added to it
/// earlier in the same response).
fn session_claims(jar: &SignedCookieJar) -> Option<SessionClaims> {
    jar.get(crate::config::get().session_cookie)
        .and_then(|c| SessionClaims::decode(c.value()))
}

/// Records that the user of the session in `jar` has just been confirmed.
/// `key` is the instance key ([`Core::session_key`](crate::state::Core)),
/// `secure` is [`secure_cookies`](super::guard::secure_cookies) of the
/// request.
///
/// The grant is bound to the session cookie in the jar, which is also the
/// one [`login_jar`](super::guard::login_jar) added earlier in the same
/// response, so call it after `login_jar` when the sign-in itself confirmed
/// the user (`login_jar` clears an earlier grant). With no session in the
/// jar there is nothing to bind to: the jar comes back unchanged (and the
/// pages keep asking).
pub fn grant(jar: SignedCookieJar, key: &Key, secure: bool) -> SignedCookieJar {
    let Some(session) = session_claims(&jar) else {
        tracing::warn!("re-auth grant asked for without a session; none given");
        return jar;
    };
    mint(jar, key, &session, session::now_unix(), secure)
}

fn mint(
    jar: SignedCookieJar,
    key: &Key,
    session: &SessionClaims,
    granted_at: i64,
    secure: bool,
) -> SignedCookieJar {
    let value = Grant {
        user_id: session.user_id,
        session_version: session.version,
        session_issued_at: session.issued_at,
        granted_at,
    }
    .seal(key);
    let mut cookie = Cookie::new(cookie_name(), value);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Strict);
    cookie.set_secure(secure);
    cookie.set_max_age(Duration::seconds(TTL_SECS));
    jar.add(cookie)
}

/// [`grant`] for the given session, as of the Unix time `granted_at`
/// (tests that need an old grant, or one for a session other than the
/// jar's). Not part of a product's own code.
#[cfg(any(test, feature = "test-support"))]
pub fn grant_at(
    jar: SignedCookieJar,
    key: &Key,
    session: &SessionClaims,
    granted_at: i64,
    secure: bool,
) -> SignedCookieJar {
    mint(jar, key, session, granted_at, secure)
}

fn counts(jar: &SignedCookieJar, key: &Key, session: &SessionClaims, now: i64) -> bool {
    jar.get(cookie_name())
        .and_then(|c| Grant::open(key, c.value()))
        .is_some_and(|grant| grant.counts_for(session, now))
}

/// Whether the jar holds a grant, less than [`TTL_SECS`] old, for the
/// session cookie in the same jar.
pub fn is_fresh(jar: &SignedCookieJar, key: &Key) -> bool {
    session_claims(jar).is_some_and(|session| counts(jar, key, &session, session::now_unix()))
}

/// [`is_fresh`] for the given session, as of the Unix time `now`.
#[cfg(any(test, feature = "test-support"))]
pub fn is_fresh_at(jar: &SignedCookieJar, key: &Key, session: &SessionClaims, now: i64) -> bool {
    counts(jar, key, session, now)
}

/// Drops the grant.
pub fn clear(jar: SignedCookieJar) -> SignedCookieJar {
    jar.remove(session::removal_cookie(cookie_name()))
}

/// Whether the signed-in user of this request has a fresh grant for the
/// session in the jar (what the kit's pages ask).
pub(crate) fn is_fresh_for(jar: &SignedCookieJar, key: &Key, user: &CurrentUser) -> bool {
    session_claims(jar).is_some_and(|session| session.user_id == user.id && is_fresh(jar, key))
}

/// Uses the grant up, after one of the most sensitive changes succeeded
/// (a passkey added, an administrator made, a write token created). With
/// the kit's own passwords there is no grant, and the jar is left alone.
pub(crate) fn spend(jar: SignedCookieJar) -> SignedCookieJar {
    match crate::config::get().features.credentials {
        CredentialMode::Plain => jar,
        CredentialMode::External(_) => clear(jar),
    }
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
    /// For the page at `page` (a same-site path), shown to `user`; `key` is
    /// the instance key.
    pub(crate) fn new(jar: &SignedCookieJar, key: &Key, user: &CurrentUser, page: &str) -> Self {
        match crate::config::get().features.credentials {
            CredentialMode::Plain => Self {
                external: false,
                fresh: true,
                url: String::new(),
            },
            CredentialMode::External(ext) => Self {
                external: true,
                fresh: is_fresh_for(jar, key, user),
                url: ExternalCredentials::with_next(ext.reauth_path, page),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn empty(key: &Key) -> SignedCookieJar {
        SignedCookieJar::new(key.clone())
    }

    /// The session of user 7, version 3, begun at `issued_at`.
    fn session(issued_at: i64) -> SessionClaims {
        SessionClaims::new(7, 3, issued_at)
    }

    /// A cookie as the browser sends it back: `jar`'s one cookie.
    fn on_the_wire(jar: SignedCookieJar, key: &Key) -> SignedCookieJar {
        use axum::http::{HeaderMap, HeaderValue, header};
        use axum::response::IntoResponse as _;

        let res = (jar, ()).into_response();
        let set = res.headers().get(header::SET_COOKIE).expect("set-cookie");
        let pair = set.to_str().unwrap().split(';').next().unwrap().to_string();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(&pair).unwrap());
        SignedCookieJar::from_headers(&headers, key.clone())
    }

    #[test]
    fn a_grant_is_fresh_for_its_user_and_session_only() {
        let key = Key::generate();
        let sess = session(NOW - 100);
        let jar = grant_at(empty(&key), &key, &sess, NOW, false);
        assert!(is_fresh_at(&jar, &key, &sess, NOW));
        assert!(is_fresh_at(&jar, &key, &sess, NOW + TTL_SECS));
        assert!(
            !is_fresh_at(&jar, &key, &sess, NOW + TTL_SECS + 1),
            "expired"
        );
        assert!(
            !is_fresh_at(&jar, &key, &SessionClaims::new(8, 3, NOW - 100), NOW),
            "another user"
        );
        assert!(
            !is_fresh_at(&jar, &key, &SessionClaims::new(7, 4, NOW - 100), NOW),
            "another session version"
        );
        assert!(is_fresh_at(&jar, &key, &sess, NOW - 10), "clock slack");
        assert!(
            !is_fresh_at(&jar, &key, &sess, NOW - 600),
            "from the future"
        );
    }

    #[test]
    fn a_grant_from_an_earlier_session_is_refused() {
        let key = Key::generate();
        // Same user, same version, but the grant was given in a session that
        // began earlier (another device, or before this sign-in).
        let earlier = session(NOW - 5000);
        let this = session(NOW - 100);
        let jar = grant_at(empty(&key), &key, &earlier, NOW, false);
        assert!(is_fresh_at(&jar, &key, &earlier, NOW));
        assert!(!is_fresh_at(&jar, &key, &this, NOW), "another session");
    }

    #[test]
    fn a_value_signed_for_another_purpose_is_not_a_grant() {
        let key = Key::generate();
        let sess = session(NOW - 100);
        let plain = format!("{TAG}.7.3.{}.{NOW}", sess.issued_at);
        let old_format = format!("r1.7.3.{NOW}");
        let sealed_elsewhere = Sealer::new(&key, "notification-channels").seal(&plain);
        for value in [plain, old_format, sealed_elsewhere] {
            // The very key that signs the grant signs this value too, as a
            // product's own cookie would be, and it is put under the
            // grant's name.
            let jar = empty(&key).add(Cookie::new(cookie_name(), value.clone()));
            assert!(!is_fresh_at(&jar, &key, &sess, NOW), "{value}");
            assert!(!is_grant(&key, &value), "{value}");
        }
        // Against it, the real thing.
        let real = grant_at(empty(&key), &key, &sess, NOW, false);
        let value = real.get(cookie_name()).unwrap().value().to_string();
        assert!(is_grant(&key, &value));
        assert!(!is_grant(&Key::generate(), &value), "another instance");
    }

    #[test]
    fn no_grant_and_garbage_are_not_fresh() {
        let key = Key::generate();
        let sess = session(1);
        assert!(!is_fresh_at(&empty(&key), &key, &sess, 1));
        let forged = empty(&key).add(Cookie::new(cookie_name(), "r2.7.3.1.1".to_string()));
        assert!(!is_fresh_at(&forged, &key, &sess, 1));
        for bad in ["", "r2", "sealed:v1:", "sealed:v1:AAAA", "7.3.1"] {
            assert_eq!(Grant::open(&key, bad), None, "{bad}");
        }
        // Extra or missing fields inside a genuine seal are refused.
        let sealer = Sealer::new(&key, PURPOSE);
        for bad in ["r2.7.3.1", "r2.7.3.1.1.9", "r1.7.3.1.1", "r2.7.3.1.x"] {
            assert_eq!(Grant::open(&key, &sealer.seal(bad)), None, "{bad}");
        }
        assert_eq!(
            Grant::open(&key, &sealer.seal("r2.7.3.1.2")),
            Some(Grant {
                user_id: 7,
                session_version: 3,
                session_issued_at: 1,
                granted_at: 2
            })
        );
    }

    #[test]
    fn a_cookie_signed_with_another_key_is_not_a_grant() {
        let key = Key::generate();
        let sess = session(NOW - 100);
        let own = grant_at(empty(&key), &key, &sess, NOW, false);
        // The same cookie is a grant for the key that signed it ...
        let received = on_the_wire(own.clone(), &key);
        assert!(is_fresh_at(&received, &key, &sess, NOW));
        // ... and nothing for any other key.
        let other = Key::generate();
        let received = on_the_wire(own, &other);
        assert!(!is_fresh_at(&received, &other, &sess, NOW));
    }

    #[test]
    fn the_cookie_is_strict_http_only_and_secure_on_https() {
        let key = Key::generate();
        let sess = session(1);
        let plain = grant_at(empty(&key), &key, &sess, 1, false);
        let c = plain.get(cookie_name()).unwrap();
        assert_eq!(c.same_site(), Some(SameSite::Strict));
        assert_eq!(c.http_only(), Some(true));
        assert_eq!(c.path(), Some("/"));
        assert_eq!(c.secure(), Some(false));
        assert_eq!(c.max_age(), Some(Duration::seconds(TTL_SECS)));
        let secure = grant_at(empty(&key), &key, &sess, 1, true);
        assert_eq!(secure.get(cookie_name()).unwrap().secure(), Some(true));
    }

    #[test]
    fn grant_binds_to_the_session_in_the_jar() {
        let key = Key::generate();
        // No session: nothing is granted.
        assert!(grant(empty(&key), &key, false).get(cookie_name()).is_none());
        // With one (as login_jar leaves it): bound to its user, version and start.
        let sess = SessionClaims::new(7, 3, session::now_unix() - 50);
        let with_session = empty(&key).add(session::session_cookie(&sess, false));
        let jar = grant(with_session, &key, false);
        assert!(is_fresh(&jar, &key));
        let other = SessionClaims::new(7, 3, sess.issued_at + 1);
        assert!(!counts(&jar, &key, &other, session::now_unix()));
        // Used up.
        let spent = clear(jar);
        assert!(!is_fresh(&spent, &key));
    }

    #[test]
    fn the_name_follows_the_session_cookie() {
        assert!(cookie_name().ends_with("_reauth"));
        assert!(!cookie_name().contains("session"));
    }
}
