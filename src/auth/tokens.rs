//! `vv_…` API tokens: generation, hashing, and reading them from requests.
//!
//! A token is `vv_` + 43 base62 characters (≈256 bits from the OS-seeded
//! CSPRNG). Only its SHA-256 is stored, which is fine for a random secret of
//! that size (no need for a slow KDF), and lets us look it up by hash.

use axum::http::{HeaderMap, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

pub const TOKEN_PREFIX: &str = "vv_";
/// 62^43 > 2^256.
pub const TOKEN_BODY_LEN: usize = 43;
/// Characters kept for display ("vv_AbC123…").
const DISPLAY_LEN: usize = TOKEN_PREFIX.len() + 6;

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// A new random token (show it once, store [`hash`]).
pub fn generate() -> String {
    // Rejection sampling: 248 = 4 × 62, so `b % 62` is uniform for b < 248.
    let mut out = String::with_capacity(TOKEN_PREFIX.len() + TOKEN_BODY_LEN);
    out.push_str(TOKEN_PREFIX);
    let mut buf = [0u8; 64];
    while out.len() < TOKEN_PREFIX.len() + TOKEN_BODY_LEN {
        rand::fill(&mut buf);
        for b in buf {
            if out.len() == TOKEN_PREFIX.len() + TOKEN_BODY_LEN {
                break;
            }
            if b < 248 {
                out.push(char::from(BASE62[usize::from(b % 62)]));
            }
        }
    }
    out
}

/// Cheap shape check before touching the database.
pub fn is_well_formed(token: &str) -> bool {
    token
        .strip_prefix(TOKEN_PREFIX)
        .is_some_and(|body| body.len() == TOKEN_BODY_LEN && body.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Lowercase hex SHA-256 (the `api_tokens.token_hash` column).
pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// The visible start of a token for lists.
pub fn display_prefix(token: &str) -> String {
    token.chars().take(DISPLAY_LEN).collect()
}

/// Credentials from `Authorization: Bearer <token>` or, Kuma/Prometheus-style,
/// `Authorization: Basic base64(<anything>:<token>)`. `None` when absent or
/// not a token-shaped value.
pub fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, rest) = value.split_once(' ')?;
    let rest = rest.trim();
    if scheme.eq_ignore_ascii_case("bearer") {
        return Some(rest.to_string());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = STANDARD.decode(rest).ok()?;
        let decoded = String::from_utf8(decoded).ok()?;
        let (_user, password) = decoded.split_once(':')?;
        return Some(password.to_string());
    }
    None
}

/// Does the request carry an `Authorization` header at all?
pub fn has_authorization(headers: &HeaderMap) -> bool {
    headers.contains_key(header::AUTHORIZATION)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn generated_tokens_are_well_formed() {
        let t = generate();
        assert!(is_well_formed(&t), "{t}");
        assert_eq!(t.len(), 46);
    }

    #[test]
    fn generated_tokens_differ() {
        assert_ne!(generate(), generate());
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let good = generate();
        let short = &good[..good.len() - 1];
        let bad_char = format!("{}!", &good[..good.len() - 1]);
        let wrong_prefix = good.replacen("vv_", "xx_", 1);
        for t in [short, bad_char.as_str(), wrong_prefix.as_str(), "", "vv_"] {
            assert!(!is_well_formed(t), "{t}");
        }
    }

    #[test]
    fn hash_is_sha256_hex() {
        assert_eq!(hash("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn display_prefix_keeps_nine_chars() {
        assert_eq!(display_prefix("vv_ABCDEFGHIJ"), "vv_ABCDEF");
    }

    fn auth(value: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static(value));
        h
    }

    #[test]
    fn bearer_token_is_read() {
        assert_eq!(token_from_headers(&auth("Bearer vv_abc")).as_deref(), Some("vv_abc"));
        assert_eq!(token_from_headers(&auth("bearer  vv_abc ")).as_deref(), Some("vv_abc"));
    }

    #[test]
    fn basic_auth_password_is_the_token() {
        // base64("prometheus:vv_abc")
        assert_eq!(
            token_from_headers(&auth("Basic cHJvbWV0aGV1czp2dl9hYmM=")).as_deref(),
            Some("vv_abc")
        );
    }

    #[test]
    fn other_schemes_are_ignored() {
        assert_eq!(token_from_headers(&auth("Digest abc")), None);
        assert_eq!(token_from_headers(&HeaderMap::new()), None);
    }
}
