//! SNMPv3 messages with the User-based Security Model (RFC 3412/3414):
//! HMAC authentication with MD5 or SHA-1 (RFC 3414) and SHA-2 (RFC 7860),
//! privacy with DES-CBC (RFC 3414) and AES-128-CFB (RFC 3826), plus AES-192
//! and AES-256 with the key extension from draft-blumenthal-aes-usm-04 that
//! net-snmp and most agents use. (Cisco's "-C" variant, which extends keys
//! the draft-reeder way, isn't offered.)
//!
//! [`super`] drives the exchange: an engine discovery, then the GET.

use std::collections::HashMap;
use std::sync::Mutex;

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use hmac::Mac;
use sha2::Digest;

use super::{GET_REQUEST, NULL, OCTET_STRING, OID, SEQUENCE, decode_int, encode_int, read_tlv, tlv};

/// A Report PDU (discovery answers and USM errors).
pub const REPORT: u8 = 0xA8;
/// `msgMaxSize` we announce: the largest reply we read.
const MAX_SIZE: i64 = super::MAX_REPLY as i64;
/// `msgSecurityModel` of the USM.
const USM: i64 = 3;
/// Bytes of the repeated password hashed into a key (RFC 3414 A.2.1).
const KEY_STRETCH: usize = 1_048_576;
/// Localized keys kept between checks (they cost a megabyte of hashing).
const KEY_CACHE_MAX: usize = 256;

/// Authentication protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Auth {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl Auth {
    /// The target's `auth=` value (`md5`, `sha`, `sha224` … `sha512`).
    pub fn parse(s: &str) -> Option<Auth> {
        Some(match s.trim().to_ascii_lowercase().replace('-', "").as_str() {
            "md5" => Auth::Md5,
            "sha" | "sha1" => Auth::Sha1,
            "sha224" => Auth::Sha224,
            "sha256" => Auth::Sha256,
            "sha384" => Auth::Sha384,
            "sha512" => Auth::Sha512,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Auth::Md5 => "MD5",
            Auth::Sha1 => "SHA-1",
            Auth::Sha224 => "SHA-224",
            Auth::Sha256 => "SHA-256",
            Auth::Sha384 => "SHA-384",
            Auth::Sha512 => "SHA-512",
        }
    }

    /// Length of the truncated MAC in `msgAuthenticationParameters`.
    fn mac_len(self) -> usize {
        match self {
            Auth::Md5 | Auth::Sha1 => 12,
            Auth::Sha224 => 16,
            Auth::Sha256 => 24,
            Auth::Sha384 => 32,
            Auth::Sha512 => 48,
        }
    }

    fn hash(self, data: &[u8]) -> Vec<u8> {
        match self {
            Auth::Md5 => md5::Md5::digest(data).to_vec(),
            Auth::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Auth::Sha224 => sha2::Sha224::digest(data).to_vec(),
            Auth::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Auth::Sha384 => sha2::Sha384::digest(data).to_vec(),
            Auth::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }

    /// The truncated HMAC of `data`.
    fn mac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        macro_rules! hmac {
            ($h:ty) => {{
                // HMAC takes keys of any length.
                let mut mac = <hmac::Hmac<$h> as hmac::digest::KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
                mac.update(data);
                mac.finalize().into_bytes().to_vec()
            }};
        }
        let mut full = match self {
            Auth::Md5 => hmac!(md5::Md5),
            Auth::Sha1 => hmac!(sha1::Sha1),
            Auth::Sha224 => hmac!(sha2::Sha224),
            Auth::Sha256 => hmac!(sha2::Sha256),
            Auth::Sha384 => hmac!(sha2::Sha384),
            Auth::Sha512 => hmac!(sha2::Sha512),
        };
        full.truncate(self.mac_len());
        full
    }

    /// RFC 3414 A.2: the password repeated to a megabyte, hashed (`Ku`).
    fn password_to_key(self, password: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(KEY_STRETCH + password.len());
        while buf.len() < KEY_STRETCH {
            buf.extend_from_slice(password);
        }
        buf.truncate(KEY_STRETCH);
        self.hash(&buf)
    }

    /// `Kul = H(Ku || engineID || Ku)`.
    fn localize(self, ku: &[u8], engine_id: &[u8]) -> Vec<u8> {
        self.hash(&[ku, engine_id, ku].concat())
    }
}

/// Privacy protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Priv {
    Des,
    Aes128,
    Aes192,
    Aes256,
}

impl Priv {
    /// The target's `priv=` value (`des`, `aes`, `aes192`, `aes256`).
    pub fn parse(s: &str) -> Option<Priv> {
        Some(match s.trim().to_ascii_lowercase().replace('-', "").as_str() {
            "des" => Priv::Des,
            "aes" | "aes128" => Priv::Aes128,
            "aes192" => Priv::Aes192,
            "aes256" => Priv::Aes256,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Priv::Des => "DES",
            Priv::Aes128 => "AES-128",
            Priv::Aes192 => "AES-192",
            Priv::Aes256 => "AES-256",
        }
    }

    /// Key bytes needed (DES: 8 for the key, 8 for the pre-IV).
    fn key_len(self) -> usize {
        match self {
            Priv::Des | Priv::Aes128 => 16,
            Priv::Aes192 => 24,
            Priv::Aes256 => 32,
        }
    }
}

/// The user and passwords of an SNMPv3 request. The security level follows
/// from which passwords are set: none, authentication, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub auth: Option<(Auth, String)>,
    pub privacy: Option<(Priv, String)>,
}

/// Keys localized to one engine.
#[derive(Clone)]
pub struct Keys {
    auth: Option<(Auth, Vec<u8>)>,
    privacy: Option<(Priv, Vec<u8>)>,
}

impl Keys {
    pub fn authenticates(&self) -> bool {
        self.auth.is_some()
    }

    pub fn encrypts(&self) -> bool {
        self.privacy.is_some()
    }
}

impl Credentials {
    /// Keys for `engine_id`, from the cache when this user was seen before.
    pub fn keys(&self, engine_id: &[u8]) -> Keys {
        let auth = self.auth.as_ref().map(|(a, pw)| (*a, cached_key(*a, None, pw, engine_id)));
        let privacy = match (&self.auth, &self.privacy) {
            (Some((a, _)), Some((p, pw))) => Some((*p, cached_key(*a, Some(*p), pw, engine_id))),
            _ => None,
        };
        Keys { auth, privacy }
    }
}

/// The localized (and for privacy, extended) key for a password.
fn derive_key(auth: Auth, privacy: Option<Priv>, password: &str, engine_id: &[u8]) -> Vec<u8> {
    let mut key = auth.localize(&auth.password_to_key(password.as_bytes()), engine_id);
    if let Some(p) = privacy {
        // Blumenthal: Kul' = Kul || H(Kul) || H(H(Kul)) … (only when Kul is short).
        let mut tail = key.clone();
        while key.len() < p.key_len() {
            tail = auth.hash(&tail);
            key.extend_from_slice(&tail);
        }
        key.truncate(p.key_len());
    }
    key
}

type KeyCache = HashMap<[u8; 32], Vec<u8>>;

fn cached_key(auth: Auth, privacy: Option<Priv>, password: &str, engine_id: &[u8]) -> Vec<u8> {
    static CACHE: Mutex<Option<KeyCache>> = Mutex::new(None);
    // The cache is indexed by a hash, so it holds no password.
    let mut id = sha2::Sha256::new();
    id.update(format!("{auth:?}/{privacy:?}/").as_bytes());
    id.update((password.len() as u64).to_be_bytes());
    id.update(password.as_bytes());
    id.update(engine_id);
    let id: [u8; 32] = id.finalize().into();
    if let Some(key) = CACHE.lock().ok().and_then(|c| c.as_ref()?.get(&id).cloned()) {
        return key;
    }
    let key = derive_key(auth, privacy, password, engine_id);
    if let Ok(mut cache) = CACHE.lock() {
        let cache = cache.get_or_insert_with(HashMap::new);
        if cache.len() >= KEY_CACHE_MAX {
            cache.clear();
        }
        cache.insert(id, key.clone());
    }
    key
}

/// AES in 128-bit CFB mode (RFC 3826); a short last block is fine.
fn aes_cfb(key: &[u8], iv: &[u8; 16], data: &mut [u8], decrypt: bool) {
    macro_rules! run {
        ($cipher:ty) => {{
            let cipher = <$cipher>::new_from_slice(key).expect("key length matches the cipher");
            let mut feedback = aes::Block::from(*iv);
            for chunk in data.chunks_mut(16) {
                let mut stream = feedback;
                cipher.encrypt_block(&mut stream);
                for (i, byte) in chunk.iter_mut().enumerate() {
                    let cipher_byte = if decrypt { *byte } else { *byte ^ stream[i] };
                    *byte ^= stream[i];
                    feedback[i] = cipher_byte;
                }
            }
        }};
    }
    match key.len() {
        16 => run!(aes::Aes128),
        24 => run!(aes::Aes192),
        _ => run!(aes::Aes256),
    }
}

/// DES-CBC over whole 8-byte blocks (`data.len()` is a multiple of 8).
fn des_cbc(key: &[u8], iv: &[u8; 8], data: &mut [u8], decrypt: bool) {
    let cipher = des::Des::new_from_slice(&key[..8]).expect("8-byte key");
    let mut prev = *iv;
    for chunk in data.as_chunks_mut::<8>().0 {
        let mut block = des::cipher::Block::<des::Des>::from(*chunk);
        if decrypt {
            let ciphertext = *chunk;
            cipher.decrypt_block(&mut block);
            for (out, (b, p)) in chunk.iter_mut().zip(block.iter().zip(prev)) {
                *out = b ^ p;
            }
            prev = ciphertext;
        } else {
            for (b, p) in block.iter_mut().zip(prev) {
                *b ^= p;
            }
            cipher.encrypt_block(&mut block);
            *chunk = block.into();
            prev = *chunk;
        }
    }
}

/// The authoritative engine: its ID and clock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Engine {
    pub id: Vec<u8>,
    pub boots: u32,
    pub time: u32,
}

/// What goes into a GET (or, with no `oid`, an empty discovery probe).
pub struct Request<'a> {
    pub msg_id: i32,
    pub request_id: i32,
    pub engine: &'a Engine,
    pub user: &'a str,
    pub keys: Option<&'a Keys>,
    pub context: &'a str,
    pub oid: Option<&'a [u8]>,
    /// Fresh per message: the privacy salt.
    pub salt: u64,
}

/// An unauthenticated, empty GET that makes the agent report its engine.
pub fn discovery(msg_id: i32, request_id: i32) -> Vec<u8> {
    build(&Request { msg_id, request_id, engine: &Engine::default(), user: "", keys: None, context: "", oid: None, salt: 0 })
}

/// Encodes `req` (encrypting and signing it as its keys say).
pub fn build(req: &Request) -> Vec<u8> {
    let varbinds = match req.oid {
        Some(oid) => tlv(SEQUENCE, &tlv(SEQUENCE, &[tlv(OID, oid), tlv(NULL, &[])].concat())),
        None => tlv(SEQUENCE, &[]),
    };
    let pdu = tlv(GET_REQUEST, &[encode_int(req.request_id.into()), encode_int(0), encode_int(0), varbinds].concat());
    wrap(req, &pdu, true)
}

/// What a fake agent sends back: `pdu` (a Response or Report) wrapped as
/// `req` says (`req.msg_id` is the request's, `req.engine` the agent's).
#[cfg(test)]
pub fn reply(req: &Request, pdu: &[u8]) -> Vec<u8> {
    wrap(req, pdu, false)
}

/// A whole message around an encoded `pdu` (requests are `reportable`).
fn wrap(req: &Request, pdu: &[u8], reportable: bool) -> Vec<u8> {
    let scoped = tlv(SEQUENCE, &[&tlv(OCTET_STRING, &req.engine.id), &tlv(OCTET_STRING, req.context.as_bytes()), pdu].concat());
    let auth = req.keys.and_then(|k| k.auth.as_ref());
    let privacy = req.keys.and_then(|k| k.privacy.as_ref()).filter(|_| auth.is_some());
    let flags = if reportable { 0x04 } else { 0 } | u8::from(auth.is_some()) | if privacy.is_some() { 2 } else { 0 };

    let (data, priv_params) = match privacy {
        Some((Priv::Des, key)) => {
            let mut salt = [0u8; 8];
            salt[..4].copy_from_slice(&req.engine.boots.to_be_bytes());
            salt[4..].copy_from_slice(&(req.salt as u32).to_be_bytes());
            let mut buf = scoped;
            buf.resize(buf.len().div_ceil(8) * 8, 0);
            des_cbc(key, &des_iv(key, &salt), &mut buf, false);
            (tlv(OCTET_STRING, &buf), salt.to_vec())
        }
        Some((_, key)) => {
            let salt = req.salt.to_be_bytes();
            let mut buf = scoped;
            aes_cfb(key, &aes_iv(req.engine, &salt), &mut buf, false);
            (tlv(OCTET_STRING, &buf), salt.to_vec())
        }
        None => (scoped, Vec::new()),
    };

    let mac_len = auth.map_or(0, |(a, _)| a.mac_len());
    let usm_head = [
        tlv(OCTET_STRING, &req.engine.id),
        encode_int(req.engine.boots.into()),
        encode_int(req.engine.time.into()),
        tlv(OCTET_STRING, req.user.as_bytes()),
    ]
    .concat();
    let auth_params = tlv(OCTET_STRING, &vec![0u8; mac_len]);
    let usm = tlv(SEQUENCE, &[usm_head.as_slice(), &auth_params, &tlv(OCTET_STRING, &priv_params)].concat());
    let header = tlv(SEQUENCE, &[encode_int(i64::from(req.msg_id)), encode_int(MAX_SIZE), tlv(OCTET_STRING, &[flags]), encode_int(USM)].concat());
    let version = encode_int(3);
    let security = tlv(OCTET_STRING, &usm);
    let body = [version.as_slice(), &header, &security, &data].concat();
    let mut msg = tlv(SEQUENCE, &body);
    if let Some((a, key)) = auth {
        // Where the zeroed MAC sits: past every header in front of it.
        let at = (msg.len() - body.len())
            + version.len()
            + header.len()
            + (security.len() - usm.len())
            + (usm.len() - (usm_head.len() + auth_params.len() + tlv(OCTET_STRING, &priv_params).len()))
            + usm_head.len()
            + (auth_params.len() - mac_len);
        let mac = a.mac(key, &msg);
        msg[at..at + mac_len].copy_from_slice(&mac);
    }
    msg
}

fn des_iv(key: &[u8], salt: &[u8; 8]) -> [u8; 8] {
    std::array::from_fn(|i| key[8 + i] ^ salt[i])
}

fn aes_iv(engine: &Engine, salt: &[u8]) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[..4].copy_from_slice(&engine.boots.to_be_bytes());
    iv[4..8].copy_from_slice(&engine.time.to_be_bytes());
    iv[8..].copy_from_slice(&salt[..8]);
    iv
}

/// A decoded SNMPv3 message.
#[derive(Debug)]
pub struct Reply {
    pub msg_id: i64,
    pub engine: Engine,
    pub user: Vec<u8>,
    /// The MAC was present and correct.
    pub authenticated: bool,
    pub encrypted: bool,
    pub pdu_tag: u8,
    /// The PDU's content (request id, error status, index, varbinds).
    pub pdu: Vec<u8>,
}

/// The `msgID` of what looks like an SNMPv3 message (to skip strays).
pub fn msg_id(packet: &[u8]) -> Option<i64> {
    let (_, msg, _) = read_tlv(packet)?;
    let (_, version, rest) = read_tlv(msg)?;
    if decode_int(version)? != 3 {
        return None;
    }
    let (_, header, _) = read_tlv(rest)?;
    let (_, id, _) = read_tlv(header)?;
    decode_int(id)
}

/// Decodes (verifies, decrypts) a reply. `keys` are what our request used:
/// a signed or encrypted reply needs them.
pub fn parse(packet: &[u8], keys: Option<&Keys>) -> Result<Reply, String> {
    let bad = || "Not an SNMPv3 answer".to_string();
    let (tag, msg, _) = read_tlv(packet).ok_or_else(bad)?;
    if tag != SEQUENCE {
        return Err(bad());
    }
    let (_, version, rest) = read_tlv(msg).ok_or_else(bad)?;
    if decode_int(version) != Some(3) {
        return Err(bad());
    }
    let (_, header, rest) = read_tlv(rest).ok_or_else(bad)?;
    let (_, msg_id, h) = read_tlv(header).ok_or_else(bad)?;
    let (_, _max_size, h) = read_tlv(h).ok_or_else(bad)?;
    let (_, flags, h) = read_tlv(h).ok_or_else(bad)?;
    let (_, model, _) = read_tlv(h).ok_or_else(bad)?;
    let flags = *flags.first().ok_or_else(bad)?;
    if decode_int(model) != Some(USM) {
        return Err("The answer uses a security model other than USM".into());
    }
    let (_, security, data) = read_tlv(rest).ok_or_else(bad)?;
    let (_, usm, _) = read_tlv(security).ok_or_else(bad)?;
    let (_, engine_id, u) = read_tlv(usm).ok_or_else(bad)?;
    let (_, boots, u) = read_tlv(u).ok_or_else(bad)?;
    let (_, time, u) = read_tlv(u).ok_or_else(bad)?;
    let (_, user, u) = read_tlv(u).ok_or_else(bad)?;
    let (_, auth_params, u) = read_tlv(u).ok_or_else(bad)?;
    let (_, priv_params, _) = read_tlv(u).ok_or_else(bad)?;
    let clock = |b: &[u8]| decode_int(b).and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
    let engine = Engine { id: engine_id.to_vec(), boots: clock(boots), time: clock(time) };

    let authenticated = flags & 1 != 0;
    if authenticated {
        let Some((auth, key)) = keys.and_then(|k| k.auth.as_ref()) else {
            return Err("The device signed an answer to an unsigned request".into());
        };
        if auth_params.len() != auth.mac_len() {
            return Err("The answer's authentication code has the wrong length (another authentication protocol?)".into());
        }
        // `auth_params` lies inside `packet`; its offset is plain address arithmetic.
        let at = auth_params.as_ptr() as usize - packet.as_ptr() as usize;
        let mut zeroed = packet.to_vec();
        zeroed[at..at + auth_params.len()].fill(0);
        if auth.mac(key, &zeroed) != auth_params {
            return Err("The answer's authentication code is wrong (wrong authentication password or protocol?)".into());
        }
    }
    let encrypted = flags & 2 != 0;
    let plain;
    let scoped = if encrypted {
        let Some((privacy, key)) = keys.and_then(|k| k.privacy.as_ref()).filter(|_| authenticated) else {
            return Err("The device encrypted an answer to an unencrypted request".into());
        };
        let (tag, cipher_text, _) = read_tlv(data).ok_or_else(bad)?;
        if tag != OCTET_STRING || priv_params.len() != 8 {
            return Err(bad());
        }
        let mut buf = cipher_text.to_vec();
        if *privacy == Priv::Des {
            if buf.len() % 8 != 0 {
                return Err("Can't decrypt the answer (not whole DES blocks)".into());
            }
            let salt: [u8; 8] = priv_params.try_into().map_err(|_| bad())?;
            des_cbc(key, &des_iv(key, &salt), &mut buf, true);
        } else {
            aes_cfb(key, &aes_iv(&engine, priv_params), &mut buf, true);
        }
        plain = buf;
        plain.as_slice()
    } else {
        data
    };
    let undecryptable = || "Can't decrypt the answer (wrong privacy password or protocol?)".to_string();
    let (tag, scoped, _) = read_tlv(scoped).ok_or_else(undecryptable)?;
    if tag != SEQUENCE {
        return Err(undecryptable());
    }
    let (_, _context_engine, s) = read_tlv(scoped).ok_or_else(bad)?;
    let (_, _context_name, s) = read_tlv(s).ok_or_else(bad)?;
    let (pdu_tag, pdu, _) = read_tlv(s).ok_or_else(bad)?;
    Ok(Reply {
        msg_id: decode_int(msg_id).ok_or_else(bad)?,
        engine,
        user: user.to_vec(),
        authenticated,
        encrypted,
        pdu_tag,
        pdu: pdu.to_vec(),
    })
}

/// What a Report's counter OID means (RFC 3414 usmStats).
pub fn report_reason(oid: &str) -> Option<ReportReason> {
    Some(match oid.trim_end_matches(".0") {
        "1.3.6.1.6.3.15.1.1.1" => ReportReason::UnsupportedSecurityLevel,
        "1.3.6.1.6.3.15.1.1.2" => ReportReason::NotInTimeWindow,
        "1.3.6.1.6.3.15.1.1.3" => ReportReason::UnknownUser,
        "1.3.6.1.6.3.15.1.1.4" => ReportReason::UnknownEngineId,
        "1.3.6.1.6.3.15.1.1.5" => ReportReason::WrongDigest,
        "1.3.6.1.6.3.15.1.1.6" => ReportReason::DecryptionError,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportReason {
    UnsupportedSecurityLevel,
    NotInTimeWindow,
    UnknownUser,
    UnknownEngineId,
    WrongDigest,
    DecryptionError,
}

impl ReportReason {
    /// A message for the monitor, naming `user`.
    pub fn message(self, user: &str) -> String {
        let user = super::util::truncate(user, 60);
        match self {
            ReportReason::UnsupportedSecurityLevel => format!(
                "The device doesn't accept this security level for user \"{user}\" (set the passwords it expects: authentication only, or both)"
            ),
            ReportReason::NotInTimeWindow => "The device rejected the request's time twice (notInTimeWindow)".into(),
            ReportReason::UnknownUser => format!("The device doesn't know the SNMPv3 user \"{user}\""),
            ReportReason::UnknownEngineId => "The device rejected its own engine ID (unknownEngineID)".into(),
            ReportReason::WrongDigest => "Wrong authentication password or protocol (the device reported a wrong digest)".into(),
            ReportReason::DecryptionError => "Wrong privacy password or protocol (the device couldn't decrypt the request)".into(),
        }
    }
}

#[cfg(test)]
mod tests;
