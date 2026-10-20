//! SNMP checker (v1 and v2c GET): `target` = `snmp://host[:port][/oid][?version=1|2c]`.
//!
//! Asks the device for one OID (default `1.3.6.1.2.1.1.3.0`, sysUpTime) using
//! the monitor's secret as the community string (default `public`). Up when a
//! value comes back; with a keyword set, the value's text must contain it (or,
//! inverted, must not). SNMPv3 isn't supported. The request is repeated once
//! when no answer arrives, because UDP loses packets. Response time = the
//! round trip.

use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use super::endpoint;
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

const DEFAULT_OID: &str = "1.3.6.1.2.1.1.3.0";
const MAX_REPLY: usize = 8192;

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
    while bytes.len() > 1 && ((bytes[0] == 0 && bytes[1] & 0x80 == 0) || (bytes[0] == 0xFF && bytes[1] & 0x80 != 0)) {
        bytes.remove(0);
    }
    tlv(INTEGER, &bytes)
}

/// Dotted OID text → BER content bytes.
pub fn encode_oid(text: &str) -> Result<Vec<u8>, String> {
    let arcs: Vec<u64> = text
        .trim_start_matches('.')
        .split('.')
        .map(|a| a.parse::<u64>().map_err(|_| format!("\"{}\" is not a valid OID", util::truncate(text, 60))))
        .collect::<Result<_, _>>()?;
    if arcs.len() < 2 || arcs[0] > 2 || (arcs[0] < 2 && arcs[1] >= 40) || arcs.len() > 128 {
        return Err(format!("\"{}\" is not a valid OID", util::truncate(text, 60)));
    }
    let mut out = Vec::new();
    for (i, arc) in std::iter::once(arcs[0] * 40 + arcs[1]).chain(arcs[2..].iter().copied()).enumerate() {
        let _ = i;
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
    arcs.iter().map(u64::to_string).collect::<Vec<_>>().join(".")
}

fn get_request(version: i64, community: &str, request_id: i32, oid: &[u8]) -> Vec<u8> {
    let varbind = tlv(SEQUENCE, &[tlv(OID, oid), tlv(NULL, &[])].concat());
    let pdu = [encode_int(i64::from(request_id)), encode_int(0), encode_int(0), tlv(SEQUENCE, &varbind)].concat();
    tlv(SEQUENCE, &[encode_int(version), tlv(OCTET_STRING, community.as_bytes()), tlv(GET_REQUEST, &pdu)].concat())
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
        (rest[..n].iter().fold(0usize, |acc, b| acc << 8 | usize::from(*b)), &rest[n..])
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
        0x40 if content.len() == 4 => Some(format!("{}.{}.{}.{}", content[0], content[1], content[2], content[3])),
        0x41 | 0x42 | 0x46 => Some(decode_uint(content).to_string()),
        0x43 => {
            let ticks = decode_uint(content);
            let secs = ticks / 100;
            Some(format!("{ticks} ({}d {:02}:{:02}:{:02})", secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60))
        }
        NULL => Some("null".into()),
        0x80..=0x82 => return Value::Missing,
        _ => None,
    };
    Value::Text(text.unwrap_or_else(|| format!("(type 0x{tag:02X})")))
}

/// The reply to our request: the value or an error text.
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
        other => return Err(format!("The device reported an SNMP error ({other})")),
    }
    let (_, varbinds, _) = read_tlv(rest).ok_or_else(bad)?;
    let (_, varbind, _) = read_tlv(varbinds).ok_or_else(bad)?;
    let (_, _name, rest) = read_tlv(varbind).ok_or_else(bad)?;
    let (value_tag, value, _) = read_tlv(rest).ok_or_else(bad)?;
    Ok(describe(value_tag, value))
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Snmp, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let version = match ep.query("version").unwrap_or("2c").to_ascii_lowercase().as_str() {
        "1" => 0,
        "2c" | "2" => 1,
        other => return CheckOutcome::down(format!("Unsupported SNMP version \"{}\" (use 1 or 2c)", util::truncate(other, 10))),
    };
    let oid_text = if ep.path.is_empty() { DEFAULT_OID } else { ep.path.as_str() };
    let oid = match encode_oid(oid_text) {
        Ok(o) => o,
        Err(e) => return CheckOutcome::down(e),
    };
    let community = monitor.secret.as_deref().filter(|c| !c.is_empty()).unwrap_or("public");
    let addr = match util::resolve(&ep.host, ep.port).await {
        Ok(a) => a,
        Err(e) => return CheckOutcome::down(e),
    };
    let timeout = super::timeout_of(monitor);
    let socket = match UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).await {
        Ok(s) => s,
        Err(e) => return CheckOutcome::down(format!("Can't open a UDP socket: {}", util::tidy_io_error(&e))),
    };
    if let Err(e) = socket.connect(addr).await {
        return CheckOutcome::down(format!("Can't reach {}:{}: {}", addr.ip(), ep.port, util::tidy_io_error(&e)));
    }
    let request_id = i32::from(rand::random::<u16>()) + 1;
    let packet = get_request(version, community, request_id, &oid);
    let start = Instant::now();
    // Two tries share the timeout: a lost datagram shouldn't mean "down".
    let per_try = (timeout / 2).max(Duration::from_millis(500)).min(timeout);
    let mut buf = vec![0u8; MAX_REPLY];
    let mut answer = None;
    for _ in 0..2 {
        if let Err(e) = socket.send(&packet).await {
            return CheckOutcome::down(format!("Send failed: {}", util::udp_recv_error(&e)));
        }
        let deadline = Instant::now() + per_try;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, socket.recv(&mut buf)).await {
                Ok(Ok(n)) => match parse_response(&buf[..n], request_id) {
                    // A stray datagram (old try, wrong id): keep listening.
                    Err(e) if e == "Not an SNMP answer" && left > Duration::ZERO => continue,
                    parsed => {
                        answer = Some(parsed);
                        break;
                    }
                },
                Ok(Err(e)) => return CheckOutcome::down(format!("No answer: {}", util::udp_recv_error(&e))),
                Err(_) => break,
            }
        }
        if answer.is_some() {
            break;
        }
    }
    let ms = util::elapsed_ms(start);
    let value = match answer {
        None => {
            return CheckOutcome::down(format!(
                "No SNMP answer from {}:{} within {} (wrong community, SNMP off or blocked?)",
                addr.ip(),
                ep.port,
                util::format_secs(timeout)
            ));
        }
        Some(Err(e)) => return CheckOutcome::down(e),
        Some(Ok(Value::Missing)) => return CheckOutcome::down(format!("The device has no value for {oid_text}")),
        Some(Ok(Value::Text(t))) => t,
    };
    let shown = util::truncate(&value, 120);
    match monitor.keyword.as_deref().filter(|k| !k.is_empty()) {
        Some(keyword) if value.contains(keyword) == monitor.keyword_invert => CheckOutcome::down(format!(
            "{oid_text} = {shown}: {} \"{}\"",
            if monitor.keyword_invert { "contains" } else { "doesn't contain" },
            util::truncate(keyword, 40)
        )),
        _ => CheckOutcome::up(ms).with_message(format!("{oid_text} = {shown}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::HeartbeatStatus;
    use crate::monitor::checkers::test_support::monitor;

    fn response(version: i64, id: i64, status: i64, value: Vec<u8>) -> Vec<u8> {
        let varbind = tlv(SEQUENCE, &[tlv(OID, &encode_oid(DEFAULT_OID).unwrap()), value].concat());
        let pdu = [encode_int(id), encode_int(status), encode_int(0), tlv(SEQUENCE, &varbind)].concat();
        tlv(SEQUENCE, &[encode_int(version), tlv(OCTET_STRING, b"public"), tlv(GET_RESPONSE, &pdu)].concat())
    }

    /// A fake agent: community `public`, answers `value` for the default OID.
    /// `drop_first` swallows the first request (packet loss).
    async fn agent(value: Vec<u8>, status: i64, drop_first: bool) -> std::net::SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let mut first = true;
            loop {
                let Ok((n, from)) = socket.recv_from(&mut buf).await else { return };
                if std::mem::take(&mut first) && drop_first {
                    continue;
                }
                let (_, msg, _) = read_tlv(&buf[..n]).unwrap();
                let (_, version, rest) = read_tlv(msg).unwrap();
                let (_, community, rest) = read_tlv(rest).unwrap();
                if community != b"public" {
                    continue; // agents stay silent on a wrong community
                }
                let (_, pdu, _) = read_tlv(rest).unwrap();
                let (_, id, _) = read_tlv(pdu).unwrap();
                let reply = response(decode_int(version).unwrap(), decode_int(id).unwrap(), status, value.clone());
                socket.send_to(&reply, from).await.unwrap();
            }
        });
        addr
    }

    fn snmp(target: &str) -> Monitor {
        let mut m = monitor(MonitorType::Snmp, target);
        m.timeout_seconds = 2;
        m
    }

    #[test]
    fn oids_round_trip_through_ber() {
        let enc = encode_oid("1.3.6.1.2.1.1.3.0").unwrap();
        assert_eq!(enc, [0x2B, 6, 1, 2, 1, 1, 3, 0]);
        assert_eq!(decode_oid(&enc), "1.3.6.1.2.1.1.3.0");
        assert_eq!(decode_oid(&encode_oid(".1.3.6.1.4.1.8072.1.3.2.3.1.1").unwrap()), "1.3.6.1.4.1.8072.1.3.2.3.1.1");
        assert_eq!(encode_oid("1.3.6.1.4.1.2021.10.1.5.1").unwrap().len(), 11);
        assert!(encode_oid("1").is_err() && encode_oid("1.x").is_err() && encode_oid("3.1").is_err() && encode_oid("1.40").is_err());
    }

    #[test]
    fn integers_use_the_shortest_signed_form() {
        assert_eq!(encode_int(0), [2, 1, 0]);
        assert_eq!(encode_int(127), [2, 1, 127]);
        assert_eq!(encode_int(128), [2, 2, 0, 128]);
        assert_eq!(encode_int(-1), [2, 1, 255]);
        assert_eq!(decode_int(&[0, 128]), Some(128));
        assert_eq!(decode_int(&[255]), Some(-1));
    }

    #[test]
    fn request_matches_a_known_packet() {
        // A v2c GET for sysUpTime.0, request id 1: net-snmp's snmpd answers it.
        let want = hex::decode("302602010104067075626c6963a019020101020100020100300e300c06082b060102010103000500").unwrap();
        assert_eq!(get_request(1, "public", 1, &encode_oid(DEFAULT_OID).unwrap()), want);
    }

    #[tokio::test]
    async fn value_is_read_and_shown() {
        let addr = agent(tlv(0x43, &[0x00, 0xA0, 0x0F, 0x40]), 0, false).await; // 10_489_664 ticks
        let out = check(&snmp(&addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("1.3.6.1.2.1.1.3.0 = 10489664 (1d 05:08:16)"));
    }

    #[tokio::test]
    async fn keyword_and_invert_apply_to_the_value() {
        let addr = agent(tlv(OCTET_STRING, b"Linux router 6.1"), 0, false).await;
        let mut m = snmp(&addr.to_string());
        m.keyword = Some("Linux".into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
        m.keyword = Some("Windows".into());
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().ends_with("doesn't contain \"Windows\""));
        m.keyword_invert = true;
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
        m.keyword = Some("Linux".into());
        assert!(check(&m).await.message.unwrap().ends_with("contains \"Linux\""));
    }

    #[tokio::test]
    async fn community_version_and_missing_values() {
        let addr = agent(tlv(INTEGER, &[42]), 0, false).await;
        let mut m = snmp(&format!("snmp://{addr}?version=1"));
        assert_eq!(check(&m).await.message.as_deref(), Some("1.3.6.1.2.1.1.3.0 = 42"));
        m.secret = Some("private".into());
        let out = check(&m).await;
        assert!(out.message.unwrap().starts_with("No SNMP answer from 127.0.0.1"));

        let addr = agent(tlv(0x81, &[]), 0, false).await;
        let out = check(&snmp(&addr.to_string())).await;
        assert_eq!(out.message.as_deref(), Some("The device has no value for 1.3.6.1.2.1.1.3.0"));
        let addr = agent(tlv(NULL, &[]), 2, false).await;
        let out = check(&snmp(&format!("{addr}/1.3.6.1.9.9"))).await;
        assert_eq!(out.message.as_deref(), Some("The device has no value for 1.3.6.1.9.9"));
        assert!(check(&snmp("127.0.0.1?version=3")).await.message.unwrap().starts_with("Unsupported SNMP version"));
        assert!(check(&snmp("127.0.0.1/1.x")).await.message.unwrap().contains("not a valid OID"));
    }

    #[tokio::test]
    async fn a_lost_datagram_is_retried() {
        let addr = agent(tlv(INTEGER, &[7]), 0, true).await;
        let mut m = snmp(&addr.to_string());
        m.timeout_seconds = 3;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    }

    #[test]
    fn garbage_is_not_an_answer() {
        assert!(parse_response(&[], 1).is_err());
        assert!(parse_response(&[0x30, 0x84, 0xFF, 0xFF, 0xFF, 0xFF], 1).is_err());
        assert!(parse_response(b"HTTP/1.1 400", 1).is_err());
    }

    /// Against a real agent: `VV_TEST_SNMP=snmp://host:port/oid,community`.
    #[tokio::test]
    #[ignore = "needs an SNMP agent (VV_TEST_SNMP)"]
    async fn live_agent() {
        let spec = std::env::var("VV_TEST_SNMP").expect("VV_TEST_SNMP");
        let (target, community) = spec.split_once(',').expect("target,community");
        let mut m = snmp(target);
        m.secret = Some(community.into());
        let out = check(&m).await;
        println!("{:?}", out.message);
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        m.secret = Some("wrong".into());
        println!("{:?}", check(&m).await.message);
    }
}
