//! One-time sign-in codes
//! ([`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes)).
//!
//! An admin resets someone's sign-in from the Users page and gets a code,
//! once: 160 random bits in Crockford base32, written as eight groups of
//! four characters (`7GQ2-M4XB-...`). It works once, for 24 hours, and until
//! a newer code for the same user replaces it, or a password change, a
//! two-factor reset, disabling or enabling the account, a sign-in or an
//! admin's "Revoke code" ends it. Using it also deletes the user's API
//! tokens, in the same transaction as the claim: a code is for an account
//! whose credentials may have leaked. Only its SHA-256 is stored,
//! so a leaked database holds no usable code, and a code is found by that
//! hash and then compared with the user it was made for, so no comparison of
//! secrets takes place.
//!
//! In the kit's own mode the person opens `/sign-in-code`, types their user
//! name, the code and a new password, and is signed in. The failures are
//! all one answer: an expired, used, revoked, unknown or someone else's
//! code, an unknown user and a throttled attempt cannot be told apart by
//! what [`redeem`] returns. A product with
//! [`CredentialMode::External`](crate::config::CredentialMode::External)
//! gets the visitor on its own password page instead
//! (`{password_path}?code=1`) and finishes the job itself:
//!
//! ```ignore
//! // On the product's password page, with ?code=1, on submit:
//! let Ok(redeemed) = sign_in_codes::redeem_from(&core, &username, &code, Some(ip)).await? else {
//!     return not_valid(); // the same page for every kind of failure
//! };
//! // The code is used up, the user's sessions and API tokens are gone. Now
//! // store whatever the product signs in with. Only the token that the
//! // redeem gave can do it, once, within ten minutes ...
//! let (user_id, version) = (redeemed.user_id(), redeemed.session_version());
//! sign_in_codes::set_password_hash(&core, redeemed, &marker_hash).await?;
//! // ... and sign the user in (guard::login_jar, after verify_second_factor
//! // when the user has two-factor on).
//! ```
//!
//! The throttles of codes are their own
//! ([`code_account`](crate::auth::rate_limit::SignInLimits::code_account),
//! [`code_ip`](crate::auth::rate_limit::SignInLimits::code_ip)); what the visitor types is never logged by
//! the kit (see [`log_ref`]).

use std::net::IpAddr;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

pub use crate::db::sign_in_codes::{LIFETIME_HOURS, MAX_EMAIL_LEN};

use crate::db::sign_in_codes as db_codes;
use crate::db::users as db_users;
use crate::http::ApiResult;
use crate::state::Core;

/// Characters in a code, without the dashes: 32 x 5 bits = 160 bits.
pub const CODE_LEN: usize = 32;

/// The characters of a code: Crockford's alphabet (digits and letters
/// without I, L, O and U).
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The one failure: the code, the user or the attempt was not good. It says
/// nothing about which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generic;

impl std::fmt::Display for Generic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the sign-in code is not valid")
    }
}

impl std::error::Error for Generic {}

/// A new random code, as shown: eight groups of four characters separated
/// by dashes. Show it once; the stored value is [`hash`] of it.
pub fn generate() -> String {
    let mut raw = [0u8; 20];
    rand::fill(&mut raw);
    let mut out = String::with_capacity(CODE_LEN + 7);
    // 20 bytes = 160 bits = 32 groups of 5 bits, exactly.
    let mut bits: u32 = 0;
    let mut have = 0;
    let mut written = 0;
    for byte in raw {
        bits = (bits << 8) | u32::from(byte);
        have += 8;
        while have >= 5 {
            have -= 5;
            if written > 0 && written % 4 == 0 {
                out.push('-');
            }
            out.push(char::from(ALPHABET[((bits >> have) & 31) as usize]));
            written += 1;
        }
        bits &= (1 << have) - 1;
    }
    out
}

/// The canonical form of what a person typed: upper case, without spaces
/// and dashes, and with Crockford's look-alikes read as the digits (`O` as
/// `0`, `I` and `L` as `1`). `None` unless it is [`CODE_LEN`] characters of
/// the alphabet.
pub fn normalize(input: &str) -> Option<String> {
    let mut out = String::with_capacity(CODE_LEN);
    for c in input.chars() {
        let c = match c.to_ascii_uppercase() {
            ' ' | '-' | '\t' | '\n' | '\r' | '_' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        };
        if !c.is_ascii() || !ALPHABET.contains(&(c as u8)) || out.len() == CODE_LEN {
            return None;
        }
        out.push(c);
    }
    (out.len() == CODE_LEN).then_some(out)
}

/// The SHA-256 of a code (the `sign_in_codes.code_hash` column). Anything
/// [`normalize`] accepts gives the hash of its canonical form; any other
/// text gives the hash of itself, which no stored code has.
pub fn hash(code: &str) -> [u8; 32] {
    let canonical = normalize(code);
    Sha256::digest(canonical.as_deref().unwrap_or(code).as_bytes()).into()
}

/// The longest user name (characters), as `auth::username_problem` allows:
/// a longer one can't exist.
const MAX_USERNAME: usize = 64;

/// The throttle key of an account's attempts. A name longer than any user
/// name is replaced by its digest, so the key (the visitor chose the text)
/// stays small.
fn account_key(username: &str) -> String {
    let name = username.trim().to_lowercase();
    if name.chars().count() > MAX_USERNAME {
        let digest = Sha256::digest(name.as_bytes());
        format!("code:h:{}", hex::encode(&digest[..16]))
    } else {
        format!("code:{name}")
    }
}

/// The throttle key of a client address: an IPv6 address by its /64, which
/// is what one subscriber gets.
fn ip_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => format!("code:{v4}"),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("code:{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

/// A short stand-in for the user name the visitor typed, for the log: eight
/// hex digits of its digest. The text itself could hold a line break that
/// forges a log line, or a code pasted into the wrong field, so it is never
/// logged; the stand-in still tells attempts at one name from another.
pub fn log_ref(username: &str) -> String {
    let digest = Sha256::digest(username.trim().to_lowercase().as_bytes());
    hex::encode(&digest[..4])
}

/// Why an attempt did not redeem a code.
#[derive(Debug)]
pub(crate) enum Refused {
    /// Too many attempts: try again after this long.
    Throttled(Duration),
    /// The code or the user was not good.
    Generic,
}

/// What a redeemed code gave.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Redeemed {
    pub user_id: i64,
    pub session_version: i64,
}

/// One attempt to redeem `code` for `username`: counted against the
/// account and the address first, then the code is used up and the
/// password (when `new_password` is given) set in one transaction. The
/// password is hashed whether or not the code is good, so the time taken
/// says nothing about it. A success undoes the count.
pub(crate) async fn attempt(
    core: &Core,
    username: &str,
    code: &str,
    ip: Option<IpAddr>,
    new_password: Option<String>,
) -> ApiResult<Result<Redeemed, Refused>> {
    let account = account_key(username);
    let address = ip.map(ip_key);
    let now = Instant::now();
    // Counted before the work, so a burst of parallel guesses can't all pass
    // the check before the first is recorded.
    if let Err(wait) = core.limits.code_account.reserve(&account, now) {
        return Ok(Err(Refused::Throttled(wait)));
    }
    if let Some(address) = &address
        && let Err(wait) = core.limits.code_ip.reserve(address, now)
    {
        core.limits.code_account.release(&account);
        return Ok(Err(Refused::Throttled(wait)));
    }
    let password_hash = match new_password {
        Some(password) => Some(crate::auth::hash_password_async(password).await?),
        None => None,
    };
    let (username, code_hash) = (username.trim().to_string(), hash(code));
    // The same statements whether or not the user exists: the lookup is by
    // name, the code by hash, and the two meet in one UPDATE that matches
    // both.
    let redeemed = core
        .with_conn(move |c| {
            let user_id = db_users::get_user_by_username(c, &username)?.map_or(-1, |u| u.id);
            Ok(
                db_codes::redeem(c, &code_hash, user_id, password_hash.as_deref())?.map(
                    |session_version| Redeemed {
                        user_id,
                        session_version,
                    },
                ),
            )
        })
        .await?;
    match redeemed {
        Some(redeemed) => {
            core.limits.code_account.clear(&account);
            if let Some(address) = &address {
                core.limits.code_ip.release(address);
            }
            Ok(Ok(redeemed))
        }
        None => Ok(Err(Refused::Generic)),
    }
}

/// How long a [`RedeemToken`] can set a credential: ten minutes.
pub const REDEEM_TOKEN_TTL: Duration = Duration::from_secs(10 * 60);

/// The proof that a code was just redeemed, given by [`redeem_from`]: the
/// only thing [`set_password_hash`] takes, so a product cannot set a user's
/// credential through this module without a redeem. It names the user, is
/// used up by `set_password_hash` (it is not `Clone`) and is no good after
/// [`REDEEM_TOKEN_TTL`]. It is kept in memory only: a product that restarts
/// between the two steps asks for a new code.
#[derive(Debug)]
#[must_use = "a redeemed code is spent; use the token to store the product's credential"]
pub struct RedeemToken {
    user_id: i64,
    session_version: i64,
    issued: Instant,
}

impl RedeemToken {
    fn new(redeemed: Redeemed) -> Self {
        Self {
            user_id: redeemed.user_id,
            session_version: redeemed.session_version,
            issued: Instant::now(),
        }
    }

    /// The user whose code it was.
    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// The user's new `session_version`, for [`login_jar`](crate::auth::guard::login_jar).
    pub fn session_version(&self) -> i64 {
        self.session_version
    }

    /// Whether it still counts, as of `now`.
    fn is_fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.issued) <= REDEEM_TOKEN_TTL
    }

    /// The same token, as if it was issued `by` earlier (tests that need an
    /// old one). Not part of a product's own code.
    #[cfg(any(test, feature = "test-support"))]
    pub fn aged(mut self, by: Duration) -> Self {
        self.issued = self.issued.checked_sub(by).unwrap_or(self.issued);
        self
    }
}

/// Redeems a code for a product that signs people in itself
/// ([`CredentialMode::External`](crate::config::CredentialMode::External)):
/// the code is used up (once: of two calls racing for it, one wins), the
/// user's sessions are signed out (their `session_version` moves on) and
/// the user's API tokens are deleted, all in one transaction. The user's
/// two-factor and passkeys are not touched, and nothing else is changed: the
/// product then stores its own credential ([`set_password_hash`], with the
/// [`RedeemToken`] that comes back) and signs the user in.
///
/// Attempts are throttled per account and, when `ip` is given, per client
/// address, with throttles of their own (the sign-in ones are not touched);
/// a throttled attempt is the same [`Generic`] failure. Pass the visitor's
/// address ([`session::client_ip`](crate::auth::session::client_ip)); only
/// without one (no peer address at all) is `None` right, because then the
/// address can't be throttled, only the account.
pub async fn redeem_from(
    core: &Core,
    username: &str,
    code: &str,
    ip: Option<IpAddr>,
) -> ApiResult<Result<RedeemToken, Generic>> {
    if !core.cfg.features.sign_in_codes {
        return Ok(Err(Generic));
    }
    Ok(attempt(core, username, code, ip, None)
        .await?
        .map(RedeemToken::new)
        .map_err(|_| Generic))
}

/// Stores `password_hash` as the credential of the user the `token` names,
/// as given: the kit never looks inside it (a product that signs in
/// elsewhere keeps a marker such as `"!external"` there). The token is used
/// up either way. It does not sign anybody out: the redeem already did, and
/// it revokes any code the user still has. `false` when the token is older
/// than [`REDEEM_TOKEN_TTL`] or the user is gone; nothing is stored then.
pub async fn set_password_hash(
    core: &Core,
    token: RedeemToken,
    password_hash: &str,
) -> ApiResult<bool> {
    if !token.is_fresh(Instant::now()) {
        return Ok(false);
    }
    let (user_id, hash) = (token.user_id, password_hash.to_string());
    core.with_conn(move |c| {
        let stored = c.execute(
            "UPDATE users SET password_hash = ?2 WHERE id = ?1",
            rusqlite::params![user_id, hash],
        )? == 1;
        if stored {
            db_codes::revoke_if_enabled(c, user_id)?;
        }
        Ok(stored)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_eight_groups_of_four() {
        let a = generate();
        let b = generate();
        assert_ne!(a, b);
        assert_eq!(a.len(), CODE_LEN + 7);
        let groups: Vec<&str> = a.split('-').collect();
        assert_eq!(groups.len(), 8);
        assert!(groups.iter().all(|g| g.len() == 4));
        assert!(a.bytes().all(|c| c == b'-' || ALPHABET.contains(&c)));
        // Every character of the alphabet turns up, evenly enough.
        let many: String = (0..400).map(|_| generate()).collect();
        for c in ALPHABET {
            assert!(many.as_bytes().contains(c), "{}", char::from(*c));
        }
        assert_eq!(normalize(&a).unwrap().len(), CODE_LEN);
    }

    #[test]
    fn what_people_type_is_read_as_the_code() {
        let code = generate();
        let plain = code.replace('-', "");
        assert_eq!(normalize(&code), Some(plain.clone()));
        assert_eq!(normalize(&plain.to_lowercase()), Some(plain.clone()));
        assert_eq!(
            normalize(&format!("  {}  ", code.replace('-', " "))),
            Some(plain)
        );
        // Look-alikes.
        let sample = "0123".repeat(8);
        let typed = "oiL3".repeat(8);
        assert_eq!(normalize(&typed), Some(sample.replace("0123", "0113")));
        // Wrong length, characters outside the alphabet.
        assert_eq!(normalize(""), None);
        assert_eq!(normalize(&"A".repeat(CODE_LEN - 1)), None);
        assert_eq!(normalize(&"A".repeat(CODE_LEN + 1)), None);
        assert_eq!(normalize(&format!("{}U", "A".repeat(CODE_LEN - 1))), None);
        assert_eq!(normalize(&format!("{}é", "A".repeat(CODE_LEN - 1))), None);
    }

    #[test]
    fn spellings_of_one_code_have_one_hash() {
        let code = generate();
        let plain = code.replace('-', "");
        assert_eq!(hash(&code), hash(&plain));
        assert_eq!(hash(&code), hash(&plain.to_lowercase()));
        assert_ne!(hash(&code), hash(&generate()));
        // A text that is no code hashes as itself.
        assert_eq!(hash("abc"), <[u8; 32]>::from(Sha256::digest(b"abc")));
    }

    #[test]
    fn throttle_keys_are_small_and_cover_the_whole_ipv6_prefix() {
        assert_eq!(account_key("  Ada "), "code:ada");
        let long = "A".repeat(2_000_000);
        let key = account_key(&long);
        assert!(key.len() < 64, "{}", key.len());
        assert_ne!(key, account_key(&format!("{long}B")), "still apart");
        assert_eq!(
            account_key(&"é".repeat(64)),
            format!("code:{}", "é".repeat(64))
        );
        assert!(account_key(&"é".repeat(65)).starts_with("code:h:"));
        // Two addresses of one /64 share a key; another /64 does not.
        let key = |s: &str| ip_key(s.parse().unwrap());
        assert_eq!(key("2001:db8:1:2::1"), key("2001:db8:1:2:ffff::9"));
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
        assert_eq!(key("203.0.113.9"), "code:203.0.113.9");
    }

    #[test]
    fn what_is_logged_for_a_user_name_is_a_short_digest() {
        let r = log_ref("Ada\r\nforged line");
        assert_eq!(r.len(), 8);
        assert!(r.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(log_ref(" ADA "), log_ref("ada"));
        assert_ne!(log_ref("ada"), log_ref("bob"));
    }

    #[test]
    fn a_redeem_token_expires_after_ten_minutes() {
        let token = RedeemToken::new(Redeemed {
            user_id: 1,
            session_version: 2,
        });
        assert_eq!((token.user_id(), token.session_version()), (1, 2));
        let issued = token.issued;
        assert!(token.is_fresh(issued));
        assert!(token.is_fresh(issued + REDEEM_TOKEN_TTL));
        assert!(!token.is_fresh(issued + REDEEM_TOKEN_TTL + Duration::from_secs(1)));
        assert!(
            !token
                .aged(Duration::from_secs(11 * 60))
                .is_fresh(Instant::now())
        );
    }

    #[test]
    fn the_stored_value_is_the_sha256_of_the_canonical_code() {
        let code = "0".repeat(CODE_LEN);
        assert_eq!(
            hex::encode(hash(&code)),
            hex::encode(Sha256::digest(code.as_bytes()))
        );
    }
}
