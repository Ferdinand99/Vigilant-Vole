//! The client side of SCRAM (RFC 5802, SHA-256 per RFC 7677), shared by the
//! PostgreSQL (SCRAM-SHA-256) and MongoDB (SCRAM-SHA-256 / SCRAM-SHA-1)
//! checkers. Only the computation lives here; each protocol carries the
//! messages in its own framing. No channel binding (`c=biws`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha1::Sha1;
use sha2::{Digest as _, Sha256};

/// Refuse absurd iteration counts from a hostile server (PostgreSQL uses
/// 4096, MongoDB 15000 for SHA-256 and 10000 for SHA-1).
pub const MAX_ITERATIONS: u32 = 200_000;

/// The hash a SCRAM mechanism is built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha256,
}

impl Hash {
    /// The SASL mechanism name.
    pub fn mechanism(self) -> &'static str {
        match self {
            Hash::Sha1 => "SCRAM-SHA-1",
            Hash::Sha256 => "SCRAM-SHA-256",
        }
    }

    pub fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => {
                let mut mac =
                    Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts any key length");
                mac.update(data);
                mac.finalize().into_bytes().to_vec()
            }
            Hash::Sha256 => {
                let mut mac =
                    Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
                mac.update(data);
                mac.finalize().into_bytes().to_vec()
            }
        }
    }

    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => Sha1::digest(data).to_vec(),
            Hash::Sha256 => Sha256::digest(data).to_vec(),
        }
    }
}

/// `SaltedPassword := Hi(password, salt, i)`: PBKDF2 with the mechanism's
/// HMAC and one output block.
pub fn salted_password(hash: Hash, password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
    let mut first = salt.to_vec();
    first.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hash.hmac(password, &first);
    let mut out = u.clone();
    for _ in 1..iterations {
        u = hash.hmac(password, &u);
        for (o, b) in out.iter_mut().zip(&u) {
            *o ^= b;
        }
    }
    out
}

/// A fresh client nonce (printable, no commas).
pub fn nonce() -> String {
    B64.encode(rand::random::<[u8; 18]>())
}

/// A user name as SCRAM's `saslname`: `=` and `,` escaped.
pub fn saslname(user: &str) -> String {
    user.replace('=', "=3D").replace(',', "=2C")
}

/// The client's final message and the server signature to expect back.
#[derive(Debug)]
pub struct ClientFinal {
    pub message: String,
    pub server_signature: Vec<u8>,
}

/// Answer the server's first message. `password` is what goes into `Hi()`
/// (MongoDB's SCRAM-SHA-1 pre-hashes it).
pub fn client_final(
    hash: Hash,
    password: &[u8],
    client_first_bare: &str,
    server_first: &str,
    client_nonce: &str,
) -> Result<ClientFinal, String> {
    let bad = || "The server sent a malformed SCRAM message".to_string();
    let (mut nonce, mut salt, mut iterations) = (None, None, None);
    for part in server_first.split(',') {
        match part.split_once('=') {
            Some(("r", v)) => nonce = Some(v),
            Some(("s", v)) => salt = Some(B64.decode(v).map_err(|_| bad())?),
            Some(("i", v)) => iterations = Some(v.parse::<u32>().map_err(|_| bad())?),
            _ => {}
        }
    }
    let (nonce, salt, iterations) = (
        nonce.ok_or_else(bad)?,
        salt.ok_or_else(bad)?,
        iterations.ok_or_else(bad)?,
    );
    if !nonce.starts_with(client_nonce) {
        return Err("The server's SCRAM nonce is wrong".into());
    }
    if iterations == 0 || iterations > MAX_ITERATIONS {
        return Err("The server asked for an unreasonable SCRAM iteration count".into());
    }
    let salted = salted_password(hash, password, &salt, iterations);
    let client_key = hash.hmac(&salted, b"Client Key");
    let stored_key = hash.digest(&client_key);
    let without_proof = format!("c=biws,r={nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
    let signature = hash.hmac(&stored_key, auth_message.as_bytes());
    let proof: Vec<u8> = client_key
        .iter()
        .zip(&signature)
        .map(|(a, b)| a ^ b)
        .collect();
    let server_key = hash.hmac(&salted, b"Server Key");
    Ok(ClientFinal {
        message: format!("{without_proof},p={}", B64.encode(proof)),
        server_signature: hash.hmac(&server_key, auth_message.as_bytes()),
    })
}

/// Whether the server's final message (`v=…`) proves it knows the password.
pub fn server_final_ok(server_final: &str, expected: &[u8]) -> bool {
    server_final
        .split(',')
        .find_map(|part| part.strip_prefix("v="))
        .and_then(|v| B64.decode(v).ok())
        .is_some_and(|v| v == expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_rfc_7677_vector() {
        // RFC 7677 §3 (user "user", password "pencil").
        let fin = client_final(
            Hash::Sha256,
            b"pencil",
            "n=user,r=rOprNGfwEbeRWgbNEkqO",
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
            "rOprNGfwEbeRWgbNEkqO",
        )
        .unwrap();
        assert_eq!(
            fin.message,
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        assert!(server_final_ok(
            "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=",
            &fin.server_signature
        ));
        assert!(!server_final_ok("v=AAAA", &fin.server_signature));
    }

    #[test]
    fn sha1_matches_the_rfc_5802_vector() {
        // RFC 5802 §5 (user "user", password "pencil").
        let fin = client_final(
            Hash::Sha1,
            b"pencil",
            "n=user,r=fyko+d2lbbFgONRv9qkxdawL",
            "r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096",
            "fyko+d2lbbFgONRv9qkxdawL",
        )
        .unwrap();
        assert_eq!(
            fin.message,
            "c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts="
        );
        assert!(server_final_ok(
            "v=rmF9pqV8S7suAoZWja4dJRkFsKQ=",
            &fin.server_signature
        ));
    }

    #[test]
    fn hostile_parameters_are_refused() {
        let err = client_final(
            Hash::Sha256,
            b"pw",
            "n=,r=abc",
            "r=abcdef,s=c2FsdA==,i=999999999",
            "abc",
        )
        .unwrap_err();
        assert!(err.contains("iteration"));
        let err = client_final(
            Hash::Sha256,
            b"pw",
            "n=,r=abc",
            "r=zzz,s=c2FsdA==,i=4096",
            "abc",
        )
        .unwrap_err();
        assert!(err.contains("nonce"));
        let err =
            client_final(Hash::Sha1, b"pw", "n=,r=abc", "r=abcdef,i=4096", "abc").unwrap_err();
        assert!(err.contains("malformed"));
    }

    #[test]
    fn user_names_are_escaped() {
        assert_eq!(saslname("a=b,c"), "a=3Db=2Cc");
        assert!(!nonce().contains(','));
    }
}
