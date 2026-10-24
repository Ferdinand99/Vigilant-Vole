//! Sealing small secrets at rest (provider tokens, passwords, webhook URLs
//! with a key in them) under the instance key (`session.key`, loaded by
//! [`load_or_create_key`](crate::auth::load_or_create_key)).
//!
//! * **Cipher:** AES-256-GCM (aws-lc-rs) with a fresh random 96-bit nonce
//!   per value.
//! * **Key:** HKDF-SHA256 of the instance key's 64 bytes, with the kit's
//!   salt and the caller's *purpose* as info. Every purpose gets its own
//!   key, and none of them is the cookie key. The purpose is also the
//!   authenticated data, so a value sealed for one purpose never opens for
//!   another.
//! * **Format:** [`PREFIX`] followed by unpadded URL-safe base64 of
//!   `nonce ‖ ciphertext ‖ tag`. The prefix names the version, so a later
//!   format can sit next to this one, and it tells sealed values from
//!   plaintext ([`is_sealed`]).
//!
//! A sealed value can only be opened with the key it was sealed with: when
//! `session.key` is lost or replaced, [`open`] returns
//! [`OpenError::Unreadable`] and the secret has to be entered again.

use std::fmt;

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::hkdf::{HKDF_SHA256, Salt};
use aws_lc_rs::rand::{SecureRandom as _, SystemRandom};
use axum_extra::extract::cookie::Key;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Marks a sealed value, and its format's version.
pub const PREFIX: &str = "sealed:v1:";

/// HKDF salt: the kit's own, so the derived keys can't collide with what
/// a product derives from the same instance key.
const SALT: &[u8] = b"vole-kit/crypto/seal/v1";

/// The AES-GCM tag length.
const TAG_LEN: usize = 16;

/// Why a value couldn't be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// The value has no [`PREFIX`]: it was never sealed.
    NotSealed,
    /// It has the prefix but isn't a sealed value (bad base64, too short).
    Malformed,
    /// Sealed with another key or purpose, or changed since: the key that
    /// sealed it is gone (a new `session.key`) or the value was tampered with.
    Unreadable,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OpenError::NotSealed => "the value is not sealed",
            OpenError::Malformed => "the sealed value is malformed",
            OpenError::Unreadable => {
                "the sealed value can't be opened with this instance key (session.key)"
            }
        })
    }
}

impl std::error::Error for OpenError {}

/// One purpose's key, derived once for many values.
pub struct Sealer {
    key: LessSafeKey,
    purpose: Vec<u8>,
}

impl fmt::Debug for Sealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealer")
            .field("purpose", &String::from_utf8_lossy(&self.purpose))
            .finish_non_exhaustive()
    }
}

impl Sealer {
    /// The key for `purpose` (e.g. `"notification-channels"`), derived from
    /// the instance key.
    pub fn new(key: &Key, purpose: &str) -> Self {
        let info = [purpose.as_bytes()];
        let prk = Salt::new(HKDF_SHA256, SALT).extract(key.master());
        let okm = prk
            .expand(&info, &AES_256_GCM)
            .expect("HKDF can expand to one AES-256 key");
        Self {
            key: LessSafeKey::new(UnboundKey::from(okm)),
            purpose: purpose.as_bytes().to_vec(),
        }
    }

    /// `plaintext`, sealed: [`PREFIX`] + base64 of `nonce ‖ ciphertext ‖ tag`.
    pub fn seal(&self, plaintext: &str) -> String {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .expect("the system's random source");
        let mut buf = plaintext.as_bytes().to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(self.purpose.as_slice()),
                &mut buf,
            )
            .expect("AES-GCM seals any short value");
        let mut raw = Vec::with_capacity(NONCE_LEN + buf.len());
        raw.extend_from_slice(&nonce);
        raw.extend_from_slice(&buf);
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(raw))
    }

    /// What [`Sealer::seal`] sealed with the same key and purpose.
    pub fn open(&self, sealed: &str) -> Result<String, OpenError> {
        let raw = decode(sealed)?;
        let (nonce, rest) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| OpenError::Malformed)?;
        let mut buf = rest.to_vec();
        let plain = self
            .key
            .open_in_place(nonce, Aad::from(self.purpose.as_slice()), &mut buf)
            .map_err(|_| OpenError::Unreadable)?;
        String::from_utf8(plain.to_vec()).map_err(|_| OpenError::Unreadable)
    }
}

/// The raw bytes of a sealed value, checked for the prefix and the length.
fn decode(sealed: &str) -> Result<Vec<u8>, OpenError> {
    let body = sealed.strip_prefix(PREFIX).ok_or(OpenError::NotSealed)?;
    let raw = URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| OpenError::Malformed)?;
    if raw.len() < NONCE_LEN + TAG_LEN {
        return Err(OpenError::Malformed);
    }
    Ok(raw)
}

/// Seals `plaintext` for `purpose` under the instance key. Derives the key
/// each time; use a [`Sealer`] for many values.
pub fn seal(key: &Key, purpose: &str, plaintext: &str) -> String {
    Sealer::new(key, purpose).seal(plaintext)
}

/// Opens a value [`seal`] sealed for `purpose` under the same instance key.
pub fn open(key: &Key, purpose: &str, sealed: &str) -> Result<String, OpenError> {
    Sealer::new(key, purpose).open(sealed)
}

/// Whether `value` is a sealed value (the prefix and a well-formed body),
/// whatever key sealed it. Plaintext never passes this unless it imitates
/// the format exactly.
pub fn is_sealed(value: &str) -> bool {
    decode(value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> Key {
        Key::from(&[byte; 64])
    }

    #[test]
    fn seal_then_open_round_trips() {
        let k = key(1);
        for secret in ["", "x", "https://hooks.example/T00/B00/XXXX", "pässwörd 🔑"] {
            let sealed = seal(&k, "test", secret);
            assert!(sealed.starts_with(PREFIX), "{sealed}");
            assert!(is_sealed(&sealed));
            // No plaintext in the output (short ones may occur in base64 by chance).
            assert!(secret.len() < 8 || !sealed.contains(secret));
            assert_eq!(open(&k, "test", &sealed).as_deref(), Ok(secret));
        }
    }

    #[test]
    fn every_seal_uses_a_new_nonce() {
        let k = key(1);
        let s = Sealer::new(&k, "test");
        let (a, b) = (s.seal("same"), s.seal("same"));
        assert_ne!(a, b);
        assert_eq!(s.open(&a).unwrap(), s.open(&b).unwrap());
    }

    #[test]
    fn another_key_or_purpose_cannot_open() {
        let sealed = seal(&key(1), "channels", "token");
        assert_eq!(
            open(&key(2), "channels", &sealed),
            Err(OpenError::Unreadable)
        );
        assert_eq!(
            open(&key(1), "registries", &sealed),
            Err(OpenError::Unreadable)
        );
    }

    #[test]
    fn tampering_is_detected() {
        let k = key(1);
        let sealed = seal(&k, "test", "token-123");
        let mut raw = URL_SAFE_NO_PAD
            .decode(sealed.strip_prefix(PREFIX).unwrap())
            .unwrap();
        for i in [0, NONCE_LEN, raw.len() - 1] {
            raw[i] ^= 0x01;
            let changed = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(&raw));
            assert_eq!(
                open(&k, "test", &changed),
                Err(OpenError::Unreadable),
                "byte {i}"
            );
            raw[i] ^= 0x01;
        }
        // Cut short, or not base64.
        let short = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(&raw[..NONCE_LEN + 3]));
        assert_eq!(open(&k, "test", &short), Err(OpenError::Malformed));
        assert_eq!(open(&k, "test", "sealed:v1:@@@"), Err(OpenError::Malformed));
    }

    #[test]
    fn plaintext_is_not_sealed() {
        for plain in ["", "token", "v1:abc", "sealed:v1:", "sealed:v1:short"] {
            assert!(!is_sealed(plain), "{plain}");
        }
        assert_eq!(open(&key(1), "t", "token"), Err(OpenError::NotSealed));
    }

    #[test]
    fn the_derived_key_is_not_the_cookie_key() {
        // A value sealed directly under either half of the master key (the
        // cookie crate's signing and encryption keys) doesn't open: the
        // sealing key is derived, never one of them.
        let k = key(9);
        let sealed = seal(&k, "test", "secret");
        for half in [&k.master()[..32], &k.master()[32..]] {
            let raw = URL_SAFE_NO_PAD
                .decode(sealed.strip_prefix(PREFIX).unwrap())
                .unwrap();
            let (nonce, rest) = raw.split_at(NONCE_LEN);
            let direct = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, half).unwrap());
            let mut buf = rest.to_vec();
            assert!(
                direct
                    .open_in_place(
                        Nonce::try_assume_unique_for_key(nonce).unwrap(),
                        Aad::from(b"test".as_slice()),
                        &mut buf
                    )
                    .is_err()
            );
        }
    }
}
