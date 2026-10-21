//! SNMP checker (v1, v2c and v3 GET):
//! `target` = `snmp://host[:port][/oid][?version=1|2c|3[&auth=…&priv=…&context=…]]`.
//!
//! Asks the device for one OID (default `1.3.6.1.2.1.1.3.0`, sysUpTime). Up
//! when a value comes back; with a keyword set, the value's text must contain
//! it (or, inverted, must not). Requests are repeated once when no answer
//! arrives, because UDP loses packets.
//!
//! - v1/v2c: the monitor's secret is the community string (default `public`).
//!   Response time = the round trip.
//! - v3 ([`usm`]): the user name is `basic_auth_user`, the authentication
//!   password `basic_auth_pass` and the privacy password `secret`; which of
//!   them are set decides the security level (noAuthNoPriv, authNoPriv,
//!   authPriv). `auth=` picks MD5, SHA-1 (default), SHA-224/256/384/512 and
//!   `priv=` DES, AES-128 (default), AES-192 or AES-256. The engine is
//!   discovered first; response time = the GET's round trip.

pub mod usm;

use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use super::endpoint::{self, Endpoint};
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

const DEFAULT_OID: &str = "1.3.6.1.2.1.1.3.0";
const MAX_REPLY: usize = 8192;
/// Shortest SNMPv3 password (RFC 3414 11.2; net-snmp refuses shorter).
pub const MIN_PASSWORD_LEN: usize = 8;

// BER tags.
const INTEGER: u8 = 0x02;
const OCTET_STRING: u8 = 0x04;
const NULL: u8 = 0x05;
const OID: u8 = 0x06;
const SEQUENCE: u8 = 0x30;
const GET_REQUEST: u8 = 0xA0;
const GET_RESPONSE: u8 = 0xA2;

fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    match body.len() {
        n if n < 128 => out.push(n as u8),
        n if n < 256 => out.extend_from_slice(&[0x81, n as u8]),
        n => out.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]),
    }
    out.extend_from_slice(body);
    out
}

fn encode_int(v: i64) -> Vec<u8> {
    let mut bytes = v.to_be_bytes().to_vec();
    while bytes.len() > 1
        && ((bytes[0] == 0 && bytes[1] & 0x80 == 0) || (bytes[0] == 0xFF && bytes[1] & 0x80 != 0))
    {
        bytes.remove(0);
    }
    tlv(INTEGER, &bytes)
}

/// Dotted OID text → BER content bytes.
pub fn encode_oid(text: &str) -> Result<Vec<u8>, String> {
    let arcs: Vec<u64> = text
        .trim_start_matches('.')
        .split('.')
        .map(|a| {
            a.parse::<u64>()
                .map_err(|_| format!("\"{}\" is not a valid OID", util::truncate(text, 60)))
        })
        .collect::<Result<_, _>>()?;
    if arcs.len() < 2 || arcs[0] > 2 || (arcs[0] < 2 && arcs[1] >= 40) || arcs.len() > 128 {
        return Err(format!(
            "\"{}\" is not a valid OID",
            util::truncate(text, 60)
        ));
    }
    let mut out = Vec::new();
    for arc in std::iter::once(arcs[0] * 40 + arcs[1]).chain(arcs[2..].iter().copied()) {
        let mut chunk = vec![(arc & 0x7F) as u8];
        let mut rest = arc >> 7;
        while rest > 0 {
            chunk.push((rest & 0x7F) as u8 | 0x80);
            rest >>= 7;
        }
        chunk.reverse();
        out.extend(chunk);
    }
    Ok(out)
}

fn decode_oid(bytes: &[u8]) -> String {
    let mut arcs: Vec<u64> = Vec::new();
    let mut value = 0u64;
    for b in bytes {
        value = value << 7 | u64::from(b & 0x7F);
        if b & 0x80 == 0 {
            if arcs.is_empty() {
                let first = (value / 40).min(2);
                arcs.push(first);
                arcs.push(value - first * 40);
            } else {
                arcs.push(value);
            }
            value = 0;
        }
    }
    arcs.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

fn get_request(version: i64, community: &str, request_id: i32, oid: &[u8]) -> Vec<u8> {
    let varbind = tlv(SEQUENCE, &[tlv(OID, oid), tlv(NULL, &[])].concat());
    let pdu = [
        encode_int(i64::from(request_id)),
        encode_int(0),
        encode_int(0),
        tlv(SEQUENCE, &varbind),
    ]
    .concat();
    tlv(
        SEQUENCE,
        &[
            encode_int(version),
            tlv(OCTET_STRING, community.as_bytes()),
            tlv(GET_REQUEST, &pdu),
        ]
        .concat(),
    )
}

/// One BER element: `(tag, content, rest)`.
fn read_tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = b.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7F);
        if n == 0 || n > 3 || rest.len() < n {
            return None;
        }
        (
            rest[..n]
                .iter()
                .fold(0usize, |acc, b| acc << 8 | usize::from(*b)),
            &rest[n..],
        )
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

fn decode_int(b: &[u8]) -> Option<i64> {
    if b.is_empty() || b.len() > 8 {
        return None;
    }
    let mut v = if b[0] & 0x80 != 0 { -1i64 } else { 0 };
    for byte in b {
        v = v << 8 | i64::from(*byte);
    }
    Some(v)
}

fn decode_uint(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |acc, byte| acc << 8 | u64::from(*byte))
}

/// What the device returned for the OID.
#[derive(Debug, PartialEq, Eq)]
enum Value {
    Text(String),
    /// Unavailable on this device (`noSuchObject`, `noSuchInstance`, `endOfMibView`).
    Missing,
}

fn describe(tag: u8, content: &[u8]) -> Value {
    let text = match tag {
        INTEGER => decode_int(content).map(|v| v.to_string()),
        OCTET_STRING | 0x44 => Some(match std::str::from_utf8(content) {
            Ok(s) if s.chars().all(|c| !c.is_control() || c == '\n') => s.trim().to_string(),
            _ => hex::encode(content),
        }),
        OID => Some(decode_oid(content)),
        0x40 if content.len() == 4 => Some(format!(
            "{}.{}.{}.{}",
            content[0], content[1], content[2], content[3]
        )),
        0x41 | 0x42 | 0x46 => Some(decode_uint(content).to_string()),
        0x43 => {
            let ticks = decode_uint(content);
            let secs = ticks / 100;
            Some(format!(
                "{ticks} ({}d {:02}:{:02}:{:02})",
                secs / 86_400,
                secs / 3600 % 24,
                secs / 60 % 60,
                secs % 60
            ))
        }
        NULL => Some("null".into()),
        0x80..=0x82 => return Value::Missing,
        _ => None,
    };
    Value::Text(text.unwrap_or_else(|| format!("(type 0x{tag:02X})")))
}

/// The reply to our v1/v2c request: the value or an error text.
fn parse_response(packet: &[u8], request_id: i32) -> Result<Value, String> {
    let bad = || "Not an SNMP answer".to_string();
    let (tag, message, _) = read_tlv(packet).ok_or_else(bad)?;
    if tag != SEQUENCE {
        return Err(bad());
    }
    let (_, _version, rest) = read_tlv(message).ok_or_else(bad)?;
    let (_, _community, rest) = read_tlv(rest).ok_or_else(bad)?;
    let (pdu_tag, pdu, _) = read_tlv(rest).ok_or_else(bad)?;
    if pdu_tag != GET_RESPONSE {
        return Err(bad());
    }
    parse_pdu(pdu, request_id)
}

/// A Response PDU's content (any version): the value or an error text.
fn parse_pdu(pdu: &[u8], request_id: i32) -> Result<Value, String> {
    let bad = || "Not an SNMP answer".to_string();
    let (_, id, rest) = read_tlv(pdu).ok_or_else(bad)?;
    if decode_int(id) != Some(i64::from(request_id)) {
        return Err(bad());
    }
    let (_, status, rest) = read_tlv(rest).ok_or_else(bad)?;
    let (_, _index, rest) = read_tlv(rest).ok_or_else(bad)?;
    match decode_int(status).ok_or_else(bad)? {
        0 => {}
        2 => return Ok(Value::Missing),
        1 => return Err("The device says the answer is too big".into()),
        6 | 16 => {
            return Err("The device refused to show this OID (authorizationError: the user or community may not read it; check the agent's access settings)".into());
        }
        other => return Err(format!("The device reported an SNMP error ({other})")),
    }
    let (_, varbinds, _) = read_tlv(rest).ok_or_else(bad)?;
    let (_, varbind, _) = read_tlv(varbinds).ok_or_else(bad)?;
    let (_, _name, rest) = read_tlv(varbind).ok_or_else(bad)?;
    let (value_tag, value, _) = read_tlv(rest).ok_or_else(bad)?;
    Ok(describe(value_tag, value))
}

/// The OID of a PDU's first varbind (what a Report counts).
fn first_varbind_oid(pdu: &[u8]) -> Option<String> {
    let (_, _id, rest) = read_tlv(pdu)?;
    let (_, _status, rest) = read_tlv(rest)?;
    let (_, _index, rest) = read_tlv(rest)?;
    let (_, varbinds, _) = read_tlv(rest)?;
    let (_, varbind, _) = read_tlv(varbinds)?;
    let (tag, name, _) = read_tlv(varbind)?;
    (tag == OID).then(|| decode_oid(name))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V1,
    V2c,
    V3,
}

/// What a target's query asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub version: Version,
    /// v3: used when an authentication password is set.
    pub auth: usm::Auth,
    /// v3: used when a privacy password is set.
    pub privacy: usm::Priv,
    /// v3: `contextName` (usually empty).
    pub context: String,
}

/// Reads `version`, `auth`, `priv` and `context` from the target's query.
pub fn options(ep: &Endpoint) -> Result<Options, String> {
    let version = match ep
        .query("version")
        .unwrap_or("2c")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "v1" => Version::V1,
        "2c" | "2" | "v2c" => Version::V2c,
        "3" | "v3" => Version::V3,
        other => {
            return Err(format!(
                "Unsupported SNMP version \"{}\" (use 1, 2c or 3)",
                util::truncate(other, 10)
            ));
        }
    };
    let auth = match ep.query("auth").filter(|a| !a.trim().is_empty()) {
        None => usm::Auth::Sha1,
        Some(a) => usm::Auth::parse(a).ok_or_else(|| {
            format!("Unknown SNMPv3 authentication protocol \"{}\" (use md5, sha, sha224, sha256, sha384 or sha512)", util::truncate(a, 20))
        })?,
    };
    let privacy = match ep.query("priv").filter(|p| !p.trim().is_empty()) {
        None => usm::Priv::Aes128,
        Some(p) => usm::Priv::parse(p).ok_or_else(|| {
            format!(
                "Unknown SNMPv3 privacy protocol \"{}\" (use des, aes, aes192 or aes256)",
                util::truncate(p, 20)
            )
        })?,
    };
    let context = ep.query("context").unwrap_or_default().to_string();
    Ok(Options {
        version,
        auth,
        privacy,
        context,
    })
}

/// Whether `target` asks for SNMPv3 (agents older than 0.3.0 can't run it).
pub fn is_v3(target: &str) -> bool {
    endpoint::parse(MonitorType::Snmp, target).is_ok_and(|ep| {
        ep.query("version")
            .is_some_and(|v| matches!(v.trim(), "3" | "v3" | "V3"))
    })
}

/// The v3 user and passwords from the monitor's credential fields.
pub fn credentials(monitor: &Monitor, opts: &Options) -> Result<usm::Credentials, String> {
    let set = |s: &Option<String>| s.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
    let user = set(&monitor.basic_auth_user).ok_or("SNMPv3 needs a user name")?;
    let auth_pw = set(&monitor.basic_auth_pass);
    let priv_pw = set(&monitor.secret);
    if priv_pw.is_some() && auth_pw.is_none() {
        return Err("A privacy password needs an authentication password too (SNMPv3 doesn't encrypt without authentication)".into());
    }
    if [&auth_pw, &priv_pw]
        .into_iter()
        .flatten()
        .any(|pw| pw.chars().count() < MIN_PASSWORD_LEN)
    {
        return Err(format!(
            "SNMPv3 passwords have at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    Ok(usm::Credentials {
        user,
        auth: auth_pw.map(|pw| (opts.auth, pw)),
        privacy: priv_pw.map(|pw| (opts.privacy, pw)),
    })
}

/// Sends `packet` and waits for the answer `accept` recognises (`None`: not
/// ours, keep listening). A lost datagram is sent once more, each try
/// waiting up to `per_try` and both ending by `deadline`. `Ok(None)`: silence.
async fn exchange<T>(
    socket: &UdpSocket,
    packet: &[u8],
    per_try: Duration,
    deadline: Instant,
    mut accept: impl FnMut(&[u8]) -> Option<T>,
) -> Result<Option<T>, String> {
    let mut buf = vec![0u8; MAX_REPLY];
    for _ in 0..2 {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if let Err(e) = socket.send(packet).await {
            return Err(format!("Send failed: {}", util::udp_recv_error(&e)));
        }
        let try_end = (now + per_try).min(deadline);
        loop {
            let left = try_end.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, socket.recv(&mut buf)).await {
                Ok(Ok(n)) => {
                    if let Some(answer) = accept(&buf[..n]) {
                        return Ok(Some(answer));
                    }
                }
                Ok(Err(e)) => return Err(format!("No answer: {}", util::udp_recv_error(&e))),
                Err(_) => break,
            }
        }
    }
    Ok(None)
}

/// A v1/v2c GET: the value and the round trip in ms.
async fn get_community(
    socket: &UdpSocket,
    version: Version,
    community: &str,
    oid: &[u8],
    timeout: Duration,
    peer: &str,
) -> Result<(Value, i64), String> {
    let request_id = i32::from(rand::random::<u16>()) + 1;
    let packet = get_request(
        if version == Version::V1 { 0 } else { 1 },
        community,
        request_id,
        oid,
    );
    let start = Instant::now();
    // Two tries share the timeout: a lost datagram shouldn't mean "down".
    let per_try = (timeout / 2).max(Duration::from_millis(500)).min(timeout);
    let answer = exchange(
        socket,
        &packet,
        per_try,
        start + timeout,
        |reply| match parse_response(reply, request_id) {
            // A stray datagram (old try, wrong id): keep listening.
            Err(e) if e == "Not an SNMP answer" => None,
            parsed => Some(parsed),
        },
    )
    .await?;
    let ms = util::elapsed_ms(start);
    match answer {
        Some(value) => value.map(|v| (v, ms)),
        None => Err(format!(
            "No SNMP answer from {peer} within {} (wrong community, SNMP off or blocked?)",
            util::format_secs(timeout)
        )),
    }
}

/// A v3 GET: engine discovery, then the request at the credentials' level
/// (once more after a time-window resync). The value and the GET's round trip.
async fn get_v3(
    socket: &UdpSocket,
    creds: &usm::Credentials,
    context: &str,
    oid: &[u8],
    timeout: Duration,
    peer: &str,
) -> Result<(Value, i64), String> {
    let start = Instant::now();
    // Leave the engine's own timeout a moment, so this message wins.
    let deadline = start + timeout.mul_f64(0.95);
    let per_try = (timeout / 4).max(Duration::from_millis(500)).min(timeout);
    let ids = || {
        (
            i32::from(rand::random::<u16>()) + 1,
            i32::from(rand::random::<u16>()) + 1,
        )
    };
    let accept = |msg_id: i32, keys: Option<&usm::Keys>| {
        let keys = keys.cloned();
        move |reply: &[u8]| {
            (usm::msg_id(reply) == Some(i64::from(msg_id)))
                .then(|| usm::parse(reply, keys.as_ref()))
        }
    };

    let (msg_id, request_id) = ids();
    let discovered = exchange(
        socket,
        &usm::discovery(msg_id, request_id),
        per_try,
        deadline,
        accept(msg_id, None),
    )
    .await?
    .ok_or_else(|| {
        format!(
            "No SNMPv3 answer from {peer} within {} (SNMP off, blocked, or no SNMPv3 there?)",
            util::format_secs(timeout)
        )
    })??;
    if discovered.pdu_tag != usm::REPORT || discovered.engine.id.is_empty() {
        return Err(
            "The device didn't answer the SNMPv3 engine discovery with its engine ID".into(),
        );
    }
    let keys = creds.keys(&discovered.engine.id);
    let mut engine = discovered.engine;
    let get_start = Instant::now();
    for attempt in 0..2 {
        let (msg_id, request_id) = ids();
        let request = usm::Request {
            msg_id,
            request_id,
            engine: &engine,
            user: &creds.user,
            keys: Some(&keys),
            context,
            oid: Some(oid),
            salt: rand::random(),
        };
        let reply = exchange(
            socket,
            &usm::build(&request),
            per_try,
            deadline,
            accept(msg_id, Some(&keys)),
        )
        .await?;
        let Some(reply) = reply else {
            let why = if keys.encrypts() {
                ": with a privacy password set, that usually means it (or the privacy protocol) is wrong"
            } else {
                ""
            };
            return Err(format!(
                "{peer} answered the SNMPv3 discovery but not the request within {}{why}",
                util::format_secs(timeout)
            ));
        };
        let reply = reply?;
        if reply.pdu_tag == usm::REPORT {
            let oid = first_varbind_oid(&reply.pdu).unwrap_or_default();
            match usm::report_reason(&oid) {
                Some(usm::ReportReason::NotInTimeWindow) if attempt == 0 => {
                    // The engine's clock moved (or it rebooted): take its time and retry.
                    engine = usm::Engine {
                        id: engine.id,
                        ..reply.engine
                    };
                    continue;
                }
                Some(reason) => return Err(reason.message(&creds.user)),
                None => {
                    return Err(format!(
                        "The device answered with an SNMPv3 report ({})",
                        util::truncate(&oid, 60)
                    ));
                }
            }
        }
        if reply.pdu_tag != GET_RESPONSE {
            return Err("Not an SNMP answer".into());
        }
        if keys.authenticates() && !reply.authenticated || keys.encrypts() && !reply.encrypted {
            return Err("The device's answer lacks the requested signature or encryption".into());
        }
        return match parse_pdu(&reply.pdu, request_id) {
            Ok(value) => Ok((value, util::elapsed_ms(get_start))),
            // net-snmp also says this when the user exists but asks for less security than its access rule.
            Err(e) if e.contains("authorizationError") => Err(format!(
                "The device won't show this OID to \"{}\" at this security level (authorizationError: check the agent's access settings, or set the passwords the user has)",
                util::truncate(&creds.user, 60)
            )),
            Err(e) => Err(e),
        };
    }
    Err(usm::ReportReason::NotInTimeWindow.message(&creds.user))
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Snmp, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let opts = match options(&ep) {
        Ok(o) => o,
        Err(e) => return CheckOutcome::down(e),
    };
    let creds = match opts.version {
        Version::V3 => match credentials(monitor, &opts) {
            Ok(c) => Some(c),
            Err(e) => return CheckOutcome::down(e),
        },
        _ => None,
    };
    let oid_text = if ep.path.is_empty() {
        DEFAULT_OID
    } else {
        ep.path.as_str()
    };
    let oid = match encode_oid(oid_text) {
        Ok(o) => o,
        Err(e) => return CheckOutcome::down(e),
    };
    let addr = match util::resolve(&ep.host, ep.port).await {
        Ok(a) => a,
        Err(e) => return CheckOutcome::down(e),
    };
    let timeout = super::timeout_of(monitor);
    let socket = match UdpSocket::bind(if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await
    {
        Ok(s) => s,
        Err(e) => {
            return CheckOutcome::down(format!(
                "Can't open a UDP socket: {}",
                util::tidy_io_error(&e)
            ));
        }
    };
    if let Err(e) = socket.connect(addr).await {
        return CheckOutcome::down(format!(
            "Can't reach {}:{}: {}",
            addr.ip(),
            ep.port,
            util::tidy_io_error(&e)
        ));
    }
    let peer = format!("{}:{}", addr.ip(), ep.port);
    let answer = match &creds {
        Some(creds) => get_v3(&socket, creds, &opts.context, &oid, timeout, &peer).await,
        None => {
            let community = monitor
                .secret
                .as_deref()
                .filter(|c| !c.is_empty())
                .unwrap_or("public");
            get_community(&socket, opts.version, community, &oid, timeout, &peer).await
        }
    };
    let (value, ms) = match answer {
        Err(e) => return CheckOutcome::down(e),
        Ok((Value::Missing, _)) => {
            return CheckOutcome::down(format!("The device has no value for {oid_text}"));
        }
        Ok((Value::Text(t), ms)) => (t, ms),
    };
    let shown = util::truncate(&value, 120);
    match monitor.keyword.as_deref().filter(|k| !k.is_empty()) {
        Some(keyword) if value.contains(keyword) == monitor.keyword_invert => {
            CheckOutcome::down(format!(
                "{oid_text} = {shown}: {} \"{}\"",
                if monitor.keyword_invert {
                    "contains"
                } else {
                    "doesn't contain"
                },
                util::truncate(keyword, 40)
            ))
        }
        _ => CheckOutcome::up(ms).with_message(format!("{oid_text} = {shown}")),
    }
}

#[cfg(test)]
mod tests;
