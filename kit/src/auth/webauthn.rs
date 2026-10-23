//! Passkeys: the relying party's side of WebAuthn (Web Authentication
//! Level 3, §7.1 "Registering a new credential" and §7.2 "Verifying an
//! authentication assertion") for discoverable credentials with user
//! verification.
//!
//! - **Attestation "none".** The options ask for none and the statement is
//!   never checked: any format is accepted and only the credential's public
//!   key is taken (the "none" attestation policy of §7.1).
//! - **Algorithms.** ES256 (P-256), EdDSA (Ed25519) and RS256, verified with
//!   aws-lc-rs, the crypto library rustls already uses. No OpenSSL.
//! - **CBOR** is read with ciborium: the attestation object, the COSE key
//!   and the extensions of the authenticator data, which must be used up
//!   exactly.
//! - **User verification is required**, so a passkey sign-in counts as both
//!   factors.
//!
//! The challenge bookkeeping (random, single use, five minutes) is
//! [`crate::db::passkeys`]; the pages are `auth::pages::passkeys`.

use aws_lc_rs::digest;
use aws_lc_rs::signature::{self, ParsedPublicKey, RsaPublicKeyComponents, UnparsedPublicKey};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ciborium::Value;
use serde::Deserialize;

/// COSE algorithm ECDSA with SHA-256 on P-256.
pub const ES256: i64 = -7;
/// COSE algorithm EdDSA (Ed25519 here).
pub const EDDSA: i64 = -8;
/// COSE algorithm RSASSA-PKCS1-v1_5 with SHA-256.
pub const RS256: i64 = -257;
/// What the options offer, most preferred first.
pub const ALGORITHMS: [i64; 3] = [ES256, EDDSA, RS256];

/// Longest credential id the specification allows.
pub const MAX_CREDENTIAL_ID: usize = 1023;

const FLAG_UP: u8 = 0x01;
const FLAG_UV: u8 = 0x04;
const FLAG_BE: u8 = 0x08;
const FLAG_BS: u8 = 0x10;
const FLAG_AT: u8 = 0x40;
const FLAG_ED: u8 = 0x80;

/// The relying party: the instance's public URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelyingParty {
    /// The RP ID: the public URL's host.
    pub id: String,
    /// The one origin passkeys are accepted from: the public URL's.
    pub origin: String,
}

impl RelyingParty {
    /// The relying party of the public URL `url`, when passkeys can work
    /// there: HTTPS with a domain name, or plain HTTP on `localhost` (a
    /// secure context too). `None` for anything else: other plain-HTTP
    /// addresses aren't a secure context, and an IP address can't be an
    /// RP ID.
    pub fn from_public_url(url: &str) -> Option<Self> {
        let url = url::Url::parse(url.trim()).ok()?;
        let host = match url.host()? {
            url::Host::Domain(domain) => domain.to_ascii_lowercase(),
            url::Host::Ipv4(_) | url::Host::Ipv6(_) => return None,
        };
        let local = host == "localhost" || host.ends_with(".localhost");
        match url.scheme() {
            "https" => {}
            "http" if local => {}
            _ => return None,
        }
        if host.is_empty() || host.ends_with('.') {
            return None;
        }
        Some(Self {
            origin: url.origin().ascii_serialization(),
            id: host,
        })
    }
}

/// Why a ceremony was refused. The message is for logs; users see one
/// neutral text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("malformed {0}")]
    Malformed(&'static str),
    #[error("wrong ceremony type")]
    Type,
    #[error("wrong or used challenge")]
    Challenge,
    #[error("wrong origin")]
    Origin,
    #[error("cross-origin ceremony")]
    CrossOrigin,
    #[error("wrong RP ID")]
    RpId,
    #[error("user not present")]
    UserPresence,
    #[error("user not verified")]
    UserVerification,
    #[error("unsupported algorithm")]
    Algorithm,
    #[error("invalid public key")]
    PublicKey,
    #[error("signature does not verify")]
    Signature,
    #[error("signature counter did not increase")]
    Counter,
    #[error("backup eligibility changed")]
    BackupEligibility,
    #[error("user handle does not match")]
    UserHandle,
    #[error("unknown credential")]
    UnknownCredential,
}

/// Base64url without padding, as WebAuthn's JSON uses it.
pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes base64url without padding (strict: no padding, no stray bits).
pub fn unb64(text: &str, what: &'static str) -> Result<Vec<u8>, Error> {
    URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| Error::Malformed(what))
}

fn sha256(data: &[u8]) -> digest::Digest {
    digest::digest(&digest::SHA256, data)
}

// ---------------------------------------------------------------------------
// Client data
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
    origin: String,
    #[serde(rename = "crossOrigin", default)]
    cross_origin: Option<bool>,
    #[serde(rename = "topOrigin", default)]
    top_origin: Option<String>,
}

/// The parts of `clientDataJSON` the ceremonies check.
#[derive(Debug, Clone)]
pub struct ClientData {
    pub kind: String,
    /// The challenge, decoded.
    pub challenge: Vec<u8>,
    pub origin: String,
    pub cross_origin: bool,
}

impl ClientData {
    /// Parses `clientDataJSON` (§7.1 steps 5–6, §7.2 steps 10–11).
    pub fn parse(json: &[u8]) -> Result<Self, Error> {
        let raw: RawClientData =
            serde_json::from_slice(json).map_err(|_| Error::Malformed("client data"))?;
        Ok(Self {
            challenge: unb64(&raw.challenge, "challenge")?,
            kind: raw.kind,
            origin: raw.origin,
            cross_origin: raw.cross_origin.unwrap_or(false) || raw.top_origin.is_some(),
        })
    }

    /// The challenge as it is stored (base64url).
    pub fn challenge_b64(&self) -> String {
        b64(&self.challenge)
    }

    /// Type, challenge, origin and no framing (§7.1 steps 7–10, §7.2 steps
    /// 12–15).
    fn check(&self, kind: &str, rp: &RelyingParty, challenge: &[u8]) -> Result<(), Error> {
        if self.kind != kind {
            return Err(Error::Type);
        }
        if self.challenge != challenge {
            return Err(Error::Challenge);
        }
        if self.origin != rp.origin {
            return Err(Error::Origin);
        }
        if self.cross_origin {
            return Err(Error::CrossOrigin);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CBOR, authenticator data, COSE keys
// ---------------------------------------------------------------------------

/// Reads one CBOR item from the start of `bytes`: the value and how many
/// bytes it took.
fn cbor_prefix(bytes: &[u8], what: &'static str) -> Result<(Value, usize), Error> {
    let mut rest = bytes;
    let value: Value = ciborium::de::from_reader(&mut rest).map_err(|_| Error::Malformed(what))?;
    Ok((value, bytes.len() - rest.len()))
}

/// Exactly one CBOR item, nothing after it.
fn cbor_whole(bytes: &[u8], what: &'static str) -> Result<Value, Error> {
    let (value, used) = cbor_prefix(bytes, what)?;
    if used != bytes.len() {
        return Err(Error::Malformed(what));
    }
    Ok(value)
}

/// A map's entries; duplicate keys are refused.
fn cbor_map(value: Value, what: &'static str) -> Result<Vec<(Value, Value)>, Error> {
    let Value::Map(entries) = value else {
        return Err(Error::Malformed(what));
    };
    for (i, (key, _)) in entries.iter().enumerate() {
        if entries[..i].iter().any(|(k, _)| k == key) {
            return Err(Error::Malformed(what));
        }
    }
    Ok(entries)
}

/// The credential data an authenticator attests to when it makes a
/// credential.
#[derive(Debug, Clone)]
pub struct AttestedCredential<'a> {
    pub aaguid: [u8; 16],
    pub credential_id: &'a [u8],
    /// The COSE_Key, exactly as encoded.
    pub public_key: &'a [u8],
}

/// Parsed authenticator data (§6.1).
#[derive(Debug, Clone)]
pub struct AuthenticatorData<'a> {
    pub rp_id_hash: &'a [u8],
    pub flags: u8,
    pub sign_count: u32,
    pub attested: Option<AttestedCredential<'a>>,
}

impl<'a> AuthenticatorData<'a> {
    /// Parses authenticator data; every byte must belong to a field.
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        const WHAT: &str = "authenticator data";
        if data.len() < 37 {
            return Err(Error::Malformed(WHAT));
        }
        let (rp_id_hash, rest) = data.split_at(32);
        let flags = rest[0];
        let sign_count = u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]);
        let mut rest = &rest[5..];

        let attested = if flags & FLAG_AT != 0 {
            if rest.len() < 18 {
                return Err(Error::Malformed(WHAT));
            }
            let mut aaguid = [0u8; 16];
            aaguid.copy_from_slice(&rest[..16]);
            let len = usize::from(u16::from_be_bytes([rest[16], rest[17]]));
            rest = &rest[18..];
            if len == 0 || len > MAX_CREDENTIAL_ID || rest.len() < len {
                return Err(Error::Malformed("credential id"));
            }
            let (credential_id, after) = rest.split_at(len);
            let (_, used) = cbor_prefix(after, "credential public key")?;
            let (public_key, after) = after.split_at(used);
            rest = after;
            Some(AttestedCredential {
                aaguid,
                credential_id,
                public_key,
            })
        } else {
            None
        };
        if flags & FLAG_ED != 0 {
            let (extensions, used) = cbor_prefix(rest, "extensions")?;
            cbor_map(extensions, "extensions")?;
            rest = &rest[used..];
        }
        if !rest.is_empty() {
            return Err(Error::Malformed(WHAT));
        }
        Ok(Self {
            rp_id_hash,
            flags,
            sign_count,
            attested,
        })
    }

    pub fn backup_eligible(&self) -> bool {
        self.flags & FLAG_BE != 0
    }

    pub fn backed_up(&self) -> bool {
        self.flags & FLAG_BS != 0
    }

    /// The RP ID hash, the user present and verified flags and consistent
    /// backup flags (§7.1 steps 13–17, §7.2 steps 15–19).
    fn check(&self, rp: &RelyingParty) -> Result<(), Error> {
        if self.rp_id_hash != sha256(rp.id.as_bytes()).as_ref() {
            return Err(Error::RpId);
        }
        if self.flags & FLAG_UP == 0 {
            return Err(Error::UserPresence);
        }
        if self.flags & FLAG_UV == 0 {
            return Err(Error::UserVerification);
        }
        if self.backed_up() && !self.backup_eligible() {
            return Err(Error::Malformed("backup flags"));
        }
        Ok(())
    }
}

/// A credential public key we can verify with.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CoseKey {
    /// The uncompressed P-256 point (`04 || x || y`).
    Es256(Vec<u8>),
    Ed25519(Vec<u8>),
    Rs256 {
        n: Vec<u8>,
        e: Vec<u8>,
    },
}

fn int_of(value: &Value) -> Option<i128> {
    match value {
        Value::Integer(i) => Some(i128::from(*i)),
        _ => None,
    }
}

/// Parses a COSE_Key (RFC 9052/9053) of one of [`ALGORITHMS`]; returns the
/// algorithm and the key. Its `alg` must be present.
fn parse_cose_key(bytes: &[u8]) -> Result<(i64, CoseKey), Error> {
    const WHAT: &str = "credential public key";
    let entries = cbor_map(cbor_whole(bytes, WHAT)?, WHAT)?;
    let get = |label: i128| {
        entries
            .iter()
            .find(|(k, _)| int_of(k) == Some(label))
            .map(|(_, v)| v)
    };
    let int = |label: i128| get(label).and_then(int_of);
    let bytes_of = |label: i128| match get(label) {
        Some(Value::Bytes(b)) => Some(b.clone()),
        _ => None,
    };
    let kty = int(1).ok_or(Error::Malformed(WHAT))?;
    let alg = int(3).ok_or(Error::Malformed(WHAT))?;
    let key = match (kty, alg) {
        // EC2, ES256, crv P-256
        (2, -7) => {
            let (x, y) = (bytes_of(-2), bytes_of(-3));
            match (int(-1), x, y) {
                (Some(1), Some(x), Some(y)) if x.len() == 32 && y.len() == 32 => {
                    let mut point = Vec::with_capacity(65);
                    point.push(0x04);
                    point.extend_from_slice(&x);
                    point.extend_from_slice(&y);
                    ParsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, &point)
                        .map_err(|_| Error::PublicKey)?;
                    CoseKey::Es256(point)
                }
                _ => return Err(Error::PublicKey),
            }
        }
        // OKP, EdDSA, crv Ed25519
        (1, -8) => match (int(-1), bytes_of(-2)) {
            (Some(6), Some(x)) if x.len() == 32 => {
                ParsedPublicKey::new(&signature::ED25519, &x).map_err(|_| Error::PublicKey)?;
                CoseKey::Ed25519(x)
            }
            _ => return Err(Error::PublicKey),
        },
        // RSA, RS256
        (3, -257) => match (bytes_of(-1), bytes_of(-2)) {
            (Some(n), Some(e)) => {
                let n = strip_zeros(&n);
                let e = strip_zeros(&e);
                // 2048 to 8192 bits; a small odd exponent.
                if !(256..=1024).contains(&n.len()) || e.is_empty() || e.len() > 4 {
                    return Err(Error::PublicKey);
                }
                if e.last().is_none_or(|b| b & 1 == 0) {
                    return Err(Error::PublicKey);
                }
                CoseKey::Rs256 {
                    n: n.to_vec(),
                    e: e.to_vec(),
                }
            }
            _ => return Err(Error::PublicKey),
        },
        _ => return Err(Error::Algorithm),
    };
    Ok((alg as i64, key))
}

fn strip_zeros(bytes: &[u8]) -> &[u8] {
    let first = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len());
    &bytes[first..]
}

impl CoseKey {
    fn verify(&self, message: &[u8], sig: &[u8]) -> Result<(), Error> {
        let ok = match self {
            CoseKey::Es256(point) => {
                UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, point)
                    .verify(message, sig)
            }
            CoseKey::Ed25519(x) => {
                UnparsedPublicKey::new(&signature::ED25519, x).verify(message, sig)
            }
            CoseKey::Rs256 { n, e } => RsaPublicKeyComponents { n, e }.verify(
                &signature::RSA_PKCS1_2048_8192_SHA256,
                message,
                sig,
            ),
        };
        ok.map_err(|_| Error::Signature)
    }
}

// ---------------------------------------------------------------------------
// Registration (§7.1)
// ---------------------------------------------------------------------------

/// A verified new credential, ready to store.
#[derive(Debug, Clone)]
pub struct Registration {
    pub credential_id: Vec<u8>,
    /// The COSE_Key as the authenticator encoded it.
    pub public_key: Vec<u8>,
    pub alg: i64,
    pub sign_count: u32,
    pub aaguid: [u8; 16],
    pub backup_eligible: bool,
    pub backed_up: bool,
}

/// Verifies `navigator.credentials.create()`'s response against the
/// challenge the server issued (§7.1). The caller still checks that no
/// stored credential has this id (step 26) and stores it.
pub fn verify_registration(
    rp: &RelyingParty,
    challenge: &[u8],
    client_data_json: &[u8],
    attestation_object: &[u8],
) -> Result<Registration, Error> {
    const WHAT: &str = "attestation object";
    ClientData::parse(client_data_json)?.check("webauthn.create", rp, challenge)?;

    let mut fmt = None;
    let mut att_stmt = None;
    let mut auth_data = None;
    for (key, value) in cbor_map(cbor_whole(attestation_object, WHAT)?, WHAT)? {
        match (key, value) {
            (Value::Text(k), Value::Text(v)) if k == "fmt" => fmt = Some(v),
            (Value::Text(k), Value::Map(v)) if k == "attStmt" => att_stmt = Some(v),
            (Value::Text(k), Value::Bytes(v)) if k == "authData" => auth_data = Some(v),
            _ => return Err(Error::Malformed(WHAT)),
        }
    }
    let (Some(fmt), Some(att_stmt), Some(auth_data)) = (fmt, att_stmt, auth_data) else {
        return Err(Error::Malformed(WHAT));
    };
    // Attestation "none": no statement to check. A "none" statement must be
    // empty; other formats are accepted unchecked (§7.1 steps 21–24 with the
    // "none" attestation policy).
    if fmt == "none" && !att_stmt.is_empty() {
        return Err(Error::Malformed(WHAT));
    }

    let data = AuthenticatorData::parse(&auth_data)?;
    data.check(rp)?;
    let attested = data
        .attested
        .as_ref()
        .ok_or(Error::Malformed("attested credential data"))?;
    let (alg, _) = parse_cose_key(attested.public_key)?;
    Ok(Registration {
        credential_id: attested.credential_id.to_vec(),
        public_key: attested.public_key.to_vec(),
        alg,
        sign_count: data.sign_count,
        aaguid: attested.aaguid,
        backup_eligible: data.backup_eligible(),
        backed_up: data.backed_up(),
    })
}

// ---------------------------------------------------------------------------
// Authentication (§7.2)
// ---------------------------------------------------------------------------

/// What the server stored about a credential.
#[derive(Debug, Clone, Copy)]
pub struct StoredCredential<'a> {
    pub public_key: &'a [u8],
    pub user_handle: &'a [u8],
    pub sign_count: u32,
    pub backup_eligible: bool,
}

/// A verified assertion: what to store back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assertion {
    pub sign_count: u32,
    pub backed_up: bool,
}

/// Verifies `navigator.credentials.get()`'s response for `stored`, the
/// credential its id names, against the challenge the server issued
/// (§7.2). The sign-in had no allow list, so the response must carry the
/// credential's user handle (step 6).
pub fn verify_assertion(
    rp: &RelyingParty,
    challenge: &[u8],
    stored: StoredCredential<'_>,
    client_data_json: &[u8],
    authenticator_data: &[u8],
    sig: &[u8],
    user_handle: Option<&[u8]>,
) -> Result<Assertion, Error> {
    if user_handle != Some(stored.user_handle) {
        return Err(Error::UserHandle);
    }
    ClientData::parse(client_data_json)?.check("webauthn.get", rp, challenge)?;
    let data = AuthenticatorData::parse(authenticator_data)?;
    if data.attested.is_some() {
        return Err(Error::Malformed("authenticator data"));
    }
    data.check(rp)?;
    // The BE flag is fixed when the credential is made.
    if data.backup_eligible() != stored.backup_eligible {
        return Err(Error::BackupEligibility);
    }
    let (_, key) = parse_cose_key(stored.public_key)?;
    let mut message = authenticator_data.to_vec();
    message.extend_from_slice(sha256(client_data_json).as_ref());
    key.verify(&message, sig)?;
    // Counters: both zero means the authenticator doesn't keep one;
    // otherwise it must have moved forward (a clone would lag behind).
    if (data.sign_count != 0 || stored.sign_count != 0) && data.sign_count <= stored.sign_count {
        return Err(Error::Counter);
    }
    Ok(Assertion {
        sign_count: data.sign_count,
        backed_up: data.backed_up(),
    })
}

/// The name of a well-known passkey provider by its AAGUID, for a new
/// passkey's default name. Browsers that hide the AAGUID send zeros.
pub fn provider_name(aaguid: &[u8; 16]) -> Option<&'static str> {
    const KNOWN: &[(&str, &str)] = &[
        (
            "ea9b8d664d011d213ce4b6b48cb575d4",
            "Google Password Manager",
        ),
        ("adce000235bcc60a648b0b25f1f05503", "Chrome on Mac"),
        ("fbfc3007154e4ecc8c0b6e020557d7bd", "iCloud Keychain"),
        ("dd4ec289e01d41c9bb8970fa845d4bf2", "iCloud Keychain"),
        ("08987058cadc4b81b6e130de50dcbe96", "Windows Hello"),
        ("9ddd1817af5a4672a2b93e3dd95000a9", "Windows Hello"),
        ("6028b017b1d44c02b4b3afcdafc96bb2", "Windows Hello"),
        ("bada5566a7aa401fbd9645619a55120d", "1Password"),
        ("d548826e79b4db40a3d811116f7e8349", "Bitwarden"),
        ("531126d6e717415c93203d9aa6981239", "Dashlane"),
        ("fdb141b25d84443e8a354698c205a502", "KeePassXC"),
    ];
    let hex = hex::encode(aaguid);
    KNOWN
        .iter()
        .find(|(id, _)| *id == hex)
        .map(|(_, name)| *name)
}

pub mod soft {
    //! A software authenticator for tests: it makes ES256, EdDSA or RS256
    //! credentials and answers challenges the way a browser and a platform
    //! authenticator would, producing real attestation objects and
    //! assertions. Products can use it to test their sign-in.

    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{
        self, EcdsaKeyPair, Ed25519KeyPair, KeyPair as _, RsaKeyPair, RsaPublicKeyComponents,
    };
    use ciborium::Value;

    use super::{FLAG_AT, FLAG_BE, FLAG_UP, FLAG_UV, b64, sha256};

    enum Key {
        Es256(EcdsaKeyPair),
        Ed25519(Ed25519KeyPair),
        Rs256(RsaKeyPair),
    }

    /// One resident credential.
    pub struct SoftAuthenticator {
        key: Key,
        pub credential_id: Vec<u8>,
        /// The user handle it was made for (set by [`Self::register`]).
        pub user_handle: Vec<u8>,
        /// The signature counter; `None` keeps it at zero, like most
        /// passkey providers.
        pub counter: Option<u32>,
        /// Flags for the next response (UP, UV and BE by default).
        pub flags: u8,
    }

    fn int(n: i64) -> Value {
        Value::Integer(n.into())
    }

    fn cbor(value: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::ser::into_writer(value, &mut out).expect("CBOR encodes");
        out
    }

    /// `clientDataJSON` as a browser writes it.
    pub fn client_data(kind: &str, challenge: &[u8], origin: &str) -> Vec<u8> {
        format!(
            r#"{{"type":"{kind}","challenge":"{}","origin":"{origin}","crossOrigin":false}}"#,
            b64(challenge)
        )
        .into_bytes()
    }

    impl SoftAuthenticator {
        fn with(key: Key) -> Self {
            let mut credential_id = [0u8; 16];
            aws_lc_rs::rand::fill(&mut credential_id).expect("random");
            Self {
                key,
                credential_id: credential_id.to_vec(),
                user_handle: Vec::new(),
                counter: None,
                flags: FLAG_UP | FLAG_UV | FLAG_BE,
            }
        }

        /// A P-256 (ES256) authenticator.
        pub fn es256() -> Self {
            Self::with(Key::Es256(
                EcdsaKeyPair::generate(&signature::ECDSA_P256_SHA256_ASN1_SIGNING).expect("key"),
            ))
        }

        /// An Ed25519 (EdDSA) authenticator.
        pub fn ed25519() -> Self {
            Self::with(Key::Ed25519(Ed25519KeyPair::generate().expect("key")))
        }

        /// An RSA-2048 (RS256) authenticator.
        pub fn rs256() -> Self {
            Self::with(Key::Rs256(
                RsaKeyPair::generate(KeySize::Rsa2048).expect("key"),
            ))
        }

        /// The credential's COSE_Key.
        pub fn cose_key(&self) -> Vec<u8> {
            let entries = match &self.key {
                Key::Es256(kp) => {
                    let point = kp.public_key().as_ref();
                    vec![
                        (int(1), int(2)),
                        (int(3), int(super::ES256)),
                        (int(-1), int(1)),
                        (int(-2), Value::Bytes(point[1..33].to_vec())),
                        (int(-3), Value::Bytes(point[33..65].to_vec())),
                    ]
                }
                Key::Ed25519(kp) => vec![
                    (int(1), int(1)),
                    (int(3), int(super::EDDSA)),
                    (int(-1), int(6)),
                    (int(-2), Value::Bytes(kp.public_key().as_ref().to_vec())),
                ],
                Key::Rs256(kp) => {
                    let parts = RsaPublicKeyComponents::<Vec<u8>>::from(kp.public_key());
                    vec![
                        (int(1), int(3)),
                        (int(3), int(super::RS256)),
                        (int(-1), Value::Bytes(parts.n)),
                        (int(-2), Value::Bytes(parts.e)),
                    ]
                }
            };
            cbor(&Value::Map(entries))
        }

        fn next_count(&mut self) -> u32 {
            match &mut self.counter {
                Some(n) => {
                    *n += 1;
                    *n
                }
                None => 0,
            }
        }

        /// Authenticator data for `rp_id` with the current flags and counter,
        /// plus `attested` when given.
        pub fn auth_data(&mut self, rp_id: &str, attested: bool) -> Vec<u8> {
            let count = self.next_count();
            let mut data = sha256(rp_id.as_bytes()).as_ref().to_vec();
            data.push(self.flags | if attested { FLAG_AT } else { 0 });
            data.extend_from_slice(&count.to_be_bytes());
            if attested {
                data.extend_from_slice(&[0u8; 16]);
                let len = u16::try_from(self.credential_id.len()).expect("short id");
                data.extend_from_slice(&len.to_be_bytes());
                data.extend_from_slice(&self.credential_id);
                data.extend_from_slice(&self.cose_key());
            }
            data
        }

        /// `(clientDataJSON, attestationObject)` for a registration,
        /// attestation "none".
        pub fn register(
            &mut self,
            rp_id: &str,
            origin: &str,
            challenge: &[u8],
            user_handle: &[u8],
        ) -> (Vec<u8>, Vec<u8>) {
            self.user_handle = user_handle.to_vec();
            let client = client_data("webauthn.create", challenge, origin);
            let auth_data = self.auth_data(rp_id, true);
            (client, attestation_object(auth_data))
        }

        /// Signs `authenticator_data || SHA-256(client_data)`.
        pub fn sign(&self, authenticator_data: &[u8], client_data: &[u8]) -> Vec<u8> {
            let mut message = authenticator_data.to_vec();
            message.extend_from_slice(sha256(client_data).as_ref());
            let rng = SystemRandom::new();
            match &self.key {
                Key::Es256(kp) => kp.sign(&rng, &message).expect("sign").as_ref().to_vec(),
                Key::Ed25519(kp) => kp.sign(&message).as_ref().to_vec(),
                Key::Rs256(kp) => {
                    let mut sig = vec![0; kp.public_modulus_len()];
                    kp.sign(&signature::RSA_PKCS1_SHA256, &rng, &message, &mut sig)
                        .expect("sign");
                    sig
                }
            }
        }

        /// `(clientDataJSON, authenticatorData, signature)` for a sign-in.
        pub fn assert(
            &mut self,
            rp_id: &str,
            origin: &str,
            challenge: &[u8],
        ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            let client = client_data("webauthn.get", challenge, origin);
            let auth_data = self.auth_data(rp_id, false);
            let sig = self.sign(&auth_data, &client);
            (client, auth_data, sig)
        }
    }

    /// An attestation object of format "none" around `auth_data`.
    pub fn attestation_object(auth_data: Vec<u8>) -> Vec<u8> {
        cbor(&Value::Map(vec![
            (Value::Text("fmt".into()), Value::Text("none".into())),
            (Value::Text("attStmt".into()), Value::Map(vec![])),
            (Value::Text("authData".into()), Value::Bytes(auth_data)),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::soft::{self, SoftAuthenticator};
    use super::*;

    const ORIGIN: &str = "https://vole.example";
    const CHALLENGE: &[u8] = b"0123456789abcdef0123456789abcdef";
    const HANDLE: &[u8] = b"user-handle-0001";

    fn rp() -> RelyingParty {
        RelyingParty::from_public_url(ORIGIN).unwrap()
    }

    /// Registers `auth`, returning what the server would store.
    fn register(auth: &mut SoftAuthenticator) -> Registration {
        let (client, att) = auth.register("vole.example", ORIGIN, CHALLENGE, HANDLE);
        verify_registration(&rp(), CHALLENGE, &client, &att).unwrap()
    }

    fn assert_with(
        reg: &Registration,
        stored_count: u32,
        client: &[u8],
        data: &[u8],
        sig: &[u8],
    ) -> Result<Assertion, Error> {
        verify_assertion(
            &rp(),
            CHALLENGE,
            StoredCredential {
                public_key: &reg.public_key,
                user_handle: HANDLE,
                sign_count: stored_count,
                backup_eligible: reg.backup_eligible,
            },
            client,
            data,
            sig,
            Some(HANDLE),
        )
    }

    #[test]
    fn the_relying_party_is_the_public_urls_host_and_origin() {
        let rp = RelyingParty::from_public_url("https://Status.Example.com/vole/").unwrap();
        assert_eq!(rp.id, "status.example.com");
        assert_eq!(rp.origin, "https://status.example.com");
        let rp = RelyingParty::from_public_url("https://vole.example:8443").unwrap();
        assert_eq!(rp.origin, "https://vole.example:8443");
        let rp = RelyingParty::from_public_url("http://localhost:3001").unwrap();
        assert_eq!(
            (rp.id.as_str(), rp.origin.as_str()),
            ("localhost", "http://localhost:3001")
        );
        for refused in [
            "",
            "not a url",
            "http://vole.example",
            "http://10.0.3.26:3001",
            "https://10.0.3.26",
            "https://[::1]:3001",
            "ftp://vole.example",
        ] {
            assert_eq!(RelyingParty::from_public_url(refused), None, "{refused}");
        }
    }

    #[test]
    fn es256_registers_and_signs_in() {
        let mut auth = SoftAuthenticator::es256();
        auth.counter = Some(0);
        let reg = register(&mut auth);
        assert_eq!(reg.alg, ES256);
        assert_eq!(reg.credential_id, auth.credential_id);
        assert_eq!(reg.sign_count, 1);
        assert!(reg.backup_eligible && !reg.backed_up);
        let (client, data, sig) = auth.assert("vole.example", ORIGIN, CHALLENGE);
        let ok = assert_with(&reg, 1, &client, &data, &sig).unwrap();
        assert_eq!(ok.sign_count, 2);
    }

    #[test]
    fn ed25519_and_rs256_register_and_sign_in() {
        for (mut auth, alg) in [
            (SoftAuthenticator::ed25519(), EDDSA),
            (SoftAuthenticator::rs256(), RS256),
        ] {
            let reg = register(&mut auth);
            assert_eq!(reg.alg, alg);
            let (client, data, sig) = auth.assert("vole.example", ORIGIN, CHALLENGE);
            assert_eq!(
                assert_with(&reg, 0, &client, &data, &sig),
                Ok(Assertion {
                    sign_count: 0,
                    backed_up: false
                })
            );
        }
    }

    #[test]
    fn registration_refuses_the_wrong_origin_challenge_rp_type_and_flags() {
        let rp = rp();
        let mut auth = SoftAuthenticator::es256();
        let attempt =
            |auth: &mut SoftAuthenticator, rp_id: &str, origin: &str, challenge: &[u8]| {
                let (client, att) = auth.register(rp_id, origin, challenge, HANDLE);
                verify_registration(&rp, CHALLENGE, &client, &att)
            };
        assert_eq!(
            attempt(&mut auth, "vole.example", "https://evil.example", CHALLENGE).unwrap_err(),
            Error::Origin
        );
        assert_eq!(
            attempt(&mut auth, "vole.example", "http://vole.example", CHALLENGE).unwrap_err(),
            Error::Origin
        );
        assert_eq!(
            attempt(&mut auth, "vole.example", ORIGIN, b"another challenge").unwrap_err(),
            Error::Challenge
        );
        assert_eq!(
            attempt(&mut auth, "evil.example", ORIGIN, CHALLENGE).unwrap_err(),
            Error::RpId
        );
        auth.flags = FLAG_UP | FLAG_BE;
        assert_eq!(
            attempt(&mut auth, "vole.example", ORIGIN, CHALLENGE).unwrap_err(),
            Error::UserVerification
        );
        auth.flags = FLAG_UV;
        assert_eq!(
            attempt(&mut auth, "vole.example", ORIGIN, CHALLENGE).unwrap_err(),
            Error::UserPresence
        );
        auth.flags = FLAG_UP | FLAG_UV | FLAG_BS;
        assert_eq!(
            attempt(&mut auth, "vole.example", ORIGIN, CHALLENGE).unwrap_err(),
            Error::Malformed("backup flags")
        );

        // A sign-in response is not a registration.
        auth.flags = FLAG_UP | FLAG_UV;
        let client = soft::client_data("webauthn.get", CHALLENGE, ORIGIN);
        let att = soft::attestation_object(auth.auth_data("vole.example", true));
        assert_eq!(
            verify_registration(&rp, CHALLENGE, &client, &att).unwrap_err(),
            Error::Type
        );

        // Framed in another site's page.
        let client = String::from_utf8(soft::client_data("webauthn.create", CHALLENGE, ORIGIN))
            .unwrap()
            .replace(r#""crossOrigin":false"#, r#""crossOrigin":true"#);
        assert_eq!(
            verify_registration(&rp, CHALLENGE, client.as_bytes(), &att).unwrap_err(),
            Error::CrossOrigin
        );
    }

    #[test]
    fn registration_refuses_malformed_objects_and_other_algorithms() {
        let rp = rp();
        let mut auth = SoftAuthenticator::es256();
        let client = soft::client_data("webauthn.create", CHALLENGE, ORIGIN);

        // No attested credential data.
        let att = soft::attestation_object(auth.auth_data("vole.example", false));
        assert!(matches!(
            verify_registration(&rp, CHALLENGE, &client, &att),
            Err(Error::Malformed(_))
        ));
        // Trailing bytes after the key.
        let mut data = auth.auth_data("vole.example", true);
        data.push(0);
        let att = soft::attestation_object(data);
        assert!(matches!(
            verify_registration(&rp, CHALLENGE, &client, &att),
            Err(Error::Malformed(_))
        ));
        // A "none" statement that isn't empty.
        let data = auth.auth_data("vole.example", true);
        let mut att = Vec::new();
        ciborium::ser::into_writer(
            &Value::Map(vec![
                (Value::Text("fmt".into()), Value::Text("none".into())),
                (
                    Value::Text("attStmt".into()),
                    Value::Map(vec![(Value::Text("sig".into()), Value::Bytes(vec![1]))]),
                ),
                (Value::Text("authData".into()), Value::Bytes(data.clone())),
            ]),
            &mut att,
        )
        .unwrap();
        assert!(matches!(
            verify_registration(&rp, CHALLENGE, &client, &att),
            Err(Error::Malformed(_))
        ));
        // Other formats are accepted unchecked: only the key is taken.
        let mut att = Vec::new();
        ciborium::ser::into_writer(
            &Value::Map(vec![
                (Value::Text("fmt".into()), Value::Text("packed".into())),
                (
                    Value::Text("attStmt".into()),
                    Value::Map(vec![(
                        Value::Text("alg".into()),
                        Value::Integer((-7).into()),
                    )]),
                ),
                (Value::Text("authData".into()), Value::Bytes(data)),
            ]),
            &mut att,
        )
        .unwrap();
        assert!(verify_registration(&rp, CHALLENGE, &client, &att).is_ok());

        // ES384 isn't offered.
        let mut es384 = Vec::new();
        ciborium::ser::into_writer(
            &Value::Map(vec![
                (Value::Integer(1.into()), Value::Integer(2.into())),
                (Value::Integer(3.into()), Value::Integer((-35).into())),
            ]),
            &mut es384,
        )
        .unwrap();
        assert_eq!(parse_cose_key(&es384).unwrap_err(), Error::Algorithm);
        // A P-256 key that isn't on the curve.
        let mut bad = Vec::new();
        ciborium::ser::into_writer(
            &Value::Map(vec![
                (Value::Integer(1.into()), Value::Integer(2.into())),
                (Value::Integer(3.into()), Value::Integer((-7).into())),
                (Value::Integer((-1).into()), Value::Integer(1.into())),
                (Value::Integer((-2).into()), Value::Bytes(vec![1; 32])),
                (Value::Integer((-3).into()), Value::Bytes(vec![2; 32])),
            ]),
            &mut bad,
        )
        .unwrap();
        assert_eq!(parse_cose_key(&bad).unwrap_err(), Error::PublicKey);
    }

    #[test]
    fn extensions_after_the_key_are_read_exactly() {
        let mut auth = SoftAuthenticator::es256();
        let mut data = auth.auth_data("vole.example", true);
        data[32] |= FLAG_ED;
        let mut ext = Vec::new();
        ciborium::ser::into_writer(
            &Value::Map(vec![(Value::Text("credProps".into()), Value::Bool(true))]),
            &mut ext,
        )
        .unwrap();
        data.extend_from_slice(&ext);
        let parsed = AuthenticatorData::parse(&data).unwrap();
        assert_eq!(parsed.attested.unwrap().public_key, &auth.cose_key()[..]);
        // The flag without extensions.
        let mut data = auth.auth_data("vole.example", true);
        data[32] |= FLAG_ED;
        assert!(AuthenticatorData::parse(&data).is_err());
    }

    #[test]
    fn sign_in_refuses_wrong_origin_challenge_rp_uv_signature_and_handle() {
        let mut auth = SoftAuthenticator::es256();
        let reg = register(&mut auth);

        // Wrong origin.
        let (client, data, sig) = auth.assert("vole.example", "https://evil.example", CHALLENGE);
        assert_eq!(
            assert_with(&reg, 0, &client, &data, &sig),
            Err(Error::Origin)
        );
        // Wrong challenge (an old or someone else's).
        let (client, data, sig) = auth.assert("vole.example", ORIGIN, b"stale");
        assert_eq!(
            assert_with(&reg, 0, &client, &data, &sig),
            Err(Error::Challenge)
        );
        // Wrong RP ID.
        let (client, data, sig) = auth.assert("evil.example", ORIGIN, CHALLENGE);
        assert_eq!(assert_with(&reg, 0, &client, &data, &sig), Err(Error::RpId));
        // Not verified (no fingerprint, face or PIN).
        auth.flags = FLAG_UP | FLAG_BE;
        let (client, data, sig) = auth.assert("vole.example", ORIGIN, CHALLENGE);
        assert_eq!(
            assert_with(&reg, 0, &client, &data, &sig),
            Err(Error::UserVerification)
        );
        auth.flags = FLAG_UP | FLAG_UV | FLAG_BE;
        // Wrong type.
        let client = soft::client_data("webauthn.create", CHALLENGE, ORIGIN);
        let data = auth.auth_data("vole.example", false);
        let sig = auth.sign(&data, &client);
        assert_eq!(assert_with(&reg, 0, &client, &data, &sig), Err(Error::Type));
        // A bad signature: tampered data, tampered client data, another key.
        let (client, mut data, sig) = auth.assert("vole.example", ORIGIN, CHALLENGE);
        data[33..37].copy_from_slice(&9u32.to_be_bytes());
        assert_eq!(
            assert_with(&reg, 0, &client, &data, &sig),
            Err(Error::Signature)
        );
        let (client, data, mut sig) = auth.assert("vole.example", ORIGIN, CHALLENGE);
        let last = sig.len() - 1;
        sig[last] ^= 1;
        assert_eq!(
            assert_with(&reg, 0, &client, &data, &sig),
            Err(Error::Signature)
        );
        let other = SoftAuthenticator::es256();
        let (client, data, _) = auth.assert("vole.example", ORIGIN, CHALLENGE);
        let sig = other.sign(&data, &client);
        assert_eq!(
            assert_with(&reg, 0, &client, &data, &sig),
            Err(Error::Signature)
        );
        // Another account's handle, or none.
        let (client, data, sig) = auth.assert("vole.example", ORIGIN, CHALLENGE);
        let stored = StoredCredential {
            public_key: &reg.public_key,
            user_handle: HANDLE,
            sign_count: 0,
            backup_eligible: true,
        };
        for handle in [Some(&b"someone-else"[..]), None] {
            assert_eq!(
                verify_assertion(&rp(), CHALLENGE, stored, &client, &data, &sig, handle),
                Err(Error::UserHandle)
            );
        }
        // Backup eligibility can't change.
        auth.flags = FLAG_UP | FLAG_UV;
        let (client, data, sig) = auth.assert("vole.example", ORIGIN, CHALLENGE);
        assert_eq!(
            assert_with(&reg, 0, &client, &data, &sig),
            Err(Error::BackupEligibility)
        );
        // Attested data has no place in a sign-in.
        auth.flags = FLAG_UP | FLAG_UV | FLAG_BE;
        let client = soft::client_data("webauthn.get", CHALLENGE, ORIGIN);
        let data = auth.auth_data("vole.example", true);
        let sig = auth.sign(&data, &client);
        assert!(matches!(
            assert_with(&reg, 0, &client, &data, &sig),
            Err(Error::Malformed(_))
        ));
    }

    #[test]
    fn the_signature_counter_must_increase_unless_both_are_zero() {
        let mut auth = SoftAuthenticator::es256();
        let reg = register(&mut auth);
        // No counter (0/0): fine, again and again.
        for _ in 0..2 {
            let (client, data, sig) = auth.assert("vole.example", ORIGIN, CHALLENGE);
            assert!(assert_with(&reg, 0, &client, &data, &sig).is_ok());
        }
        auth.counter = Some(6);
        let (client, data, sig) = auth.assert("vole.example", ORIGIN, CHALLENGE);
        assert_eq!(
            assert_with(&reg, 6, &client, &data, &sig)
                .unwrap()
                .sign_count,
            7
        );
        // A clone lagging behind, or the same response replayed.
        assert_eq!(
            assert_with(&reg, 7, &client, &data, &sig),
            Err(Error::Counter)
        );
        assert_eq!(
            assert_with(&reg, 9, &client, &data, &sig),
            Err(Error::Counter)
        );
        // An authenticator that stopped counting after it counted.
        auth.counter = None;
        let (client, data, sig) = auth.assert("vole.example", ORIGIN, CHALLENGE);
        assert_eq!(
            assert_with(&reg, 7, &client, &data, &sig),
            Err(Error::Counter)
        );
    }

    #[test]
    fn malformed_client_data_and_encodings_are_refused() {
        assert!(ClientData::parse(b"not json").is_err());
        assert!(ClientData::parse(br#"{"type":"webauthn.get","origin":"x"}"#).is_err());
        assert!(
            ClientData::parse(br#"{"type":"webauthn.get","challenge":"YQ==","origin":"x"}"#)
                .is_err()
        );
        let top = ClientData::parse(
            br#"{"type":"webauthn.get","challenge":"YQ","origin":"x","topOrigin":"https://evil.example"}"#,
        )
        .unwrap();
        assert!(top.cross_origin);
        assert!(unb64("a+b/", "x").is_err());
        assert_eq!(unb64(&b64(b"\xff\xfe"), "x").unwrap(), b"\xff\xfe");
        assert!(AuthenticatorData::parse(&[0; 36]).is_err());
    }

    #[test]
    fn well_known_providers_name_their_passkeys() {
        let gpm: [u8; 16] = hex::decode("ea9b8d664d011d213ce4b6b48cb575d4")
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(provider_name(&gpm), Some("Google Password Manager"));
        assert_eq!(provider_name(&[0; 16]), None);
    }
}
