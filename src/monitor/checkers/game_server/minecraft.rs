//! Minecraft: Java Edition Server List Ping (1.7+): handshake (next state
//! "status"), status request, JSON reply, then ping/pong for the latency.
//! <https://minecraft.wiki/w/Java_Edition_protocol/Server_List_Ping>

use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use super::{Status, Target, strip_section_codes};
use crate::monitor::checkers::{conn, util};

/// Largest status reply accepted (server icons make them tens of KiB).
const MAX_PACKET: usize = 1 << 20;
/// Chat components nested deeper than this are ignored.
const MAX_DEPTH: usize = 32;

pub fn write_varint(mut v: u32, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A VarInt from the front of `buf`, advancing it.
pub fn take_varint(buf: &mut &[u8]) -> Result<u32, String> {
    let mut value = 0u32;
    for i in 0..5 {
        let (&byte, rest) = buf.split_first().ok_or("Truncated Minecraft packet")?;
        *buf = rest;
        value |= u32::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("Malformed Minecraft packet (VarInt too long)".into())
}

/// A VarInt whose first byte was already read.
async fn read_varint_after<R: AsyncRead + Unpin>(r: &mut R, first: u8) -> Result<u32, String> {
    let mut value = u32::from(first & 0x7F);
    let mut byte = first;
    let mut i = 1;
    while byte & 0x80 != 0 {
        if i == 5 {
            return Err("Malformed Minecraft packet (VarInt too long)".into());
        }
        byte = r.read_u8().await.map_err(read_error)?;
        value |= u32::from(byte & 0x7F) << (7 * i);
        i += 1;
    }
    Ok(value)
}

fn read_error(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        "The server closed the connection without a status reply (not a Minecraft Java server?)"
            .into()
    } else {
        format!(
            "Reading the status reply failed: {}",
            util::tidy_io_error(&e)
        )
    }
}

/// One length-prefixed packet's body.
async fn read_packet<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>, String> {
    let first = r.read_u8().await.map_err(read_error)?;
    if first == 0xFF {
        return Err("The server answered with a pre-1.7 (legacy) ping reply; only Minecraft 1.7 and newer are supported".into());
    }
    let len = read_varint_after(r, first).await? as usize;
    if len == 0 || len > MAX_PACKET {
        return Err(format!(
            "Not a Minecraft status reply (packet length {len})"
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.map_err(read_error)?;
    Ok(body)
}

fn packet(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    write_varint(body.len() as u32, &mut out);
    out.extend_from_slice(body);
    out
}

/// Handshake (protocol -1 = "just asking", next state 1 = status) followed
/// by the status request.
pub fn status_request(host: &str, port: u16) -> Vec<u8> {
    let mut hs = vec![0x00];
    write_varint(u32::MAX, &mut hs);
    write_varint(host.len() as u32, &mut hs);
    hs.extend_from_slice(host.as_bytes());
    hs.extend_from_slice(&port.to_be_bytes());
    write_varint(1, &mut hs);
    let mut out = packet(&hs);
    out.extend_from_slice(&packet(&[0x00]));
    out
}

/// The JSON text inside a status response packet body.
pub fn status_json(body: &[u8]) -> Result<&[u8], String> {
    let mut rest = body;
    let id = take_varint(&mut rest)?;
    if id != 0 {
        return Err(format!("Not a Minecraft status reply (packet 0x{id:02X})"));
    }
    let len = take_varint(&mut rest)? as usize;
    rest.get(..len)
        .ok_or_else(|| "Truncated Minecraft status reply".to_string())
}

/// Plain text of a chat component: `text`, then `extra` children in order
/// (depth first), like the client renders it.
pub fn chat_text(v: &Value) -> String {
    fn walk(v: &Value, out: &mut String, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        match v {
            Value::String(s) => out.push_str(s),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::Array(items) => items.iter().for_each(|i| walk(i, out, depth + 1)),
            Value::Object(o) => {
                if let Some(text) = o.get("text") {
                    walk(text, out, depth + 1);
                }
                if let Some(extra) = o.get("extra") {
                    walk(extra, out, depth + 1);
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    walk(v, &mut out, 0);
    strip_section_codes(&out)
}

/// Version, players and MOTD from the status JSON.
pub fn parse_status(json: &[u8]) -> Result<Status, String> {
    let v: Value = serde_json::from_slice(json)
        .map_err(|e| format!("Unreadable Minecraft status JSON: {e}"))?;
    if !v.is_object() {
        return Err("Unreadable Minecraft status JSON: not an object".into());
    }
    let version = v["version"]["name"]
        .as_str()
        .map(strip_section_codes)
        .unwrap_or_default();
    let product = match version.chars().next() {
        None => "Minecraft".to_string(),
        Some(c) if c.is_ascii_digit() => format!("Minecraft {version}"),
        Some(_) => version,
    };
    let count = |key: &str| {
        v["players"][key]
            .as_u64()
            .or_else(|| v["players"][key].as_i64().map(|n| n.max(0) as u64))
    };
    let players = match (count("online"), count("max")) {
        (Some(online), Some(max)) => Some((online, max)),
        _ => None,
    };
    Ok(Status {
        product,
        players,
        name: chat_text(&v["description"]),
        detail: None,
    })
}

/// An SRV answer: priority, weight, target host, port.
pub type SrvRecord = (u16, u16, String, u16);

/// The record the client would use: lowest priority, then highest weight.
/// A target of "." means "no service".
pub fn pick_srv(records: &[SrvRecord]) -> Option<(String, u16)> {
    records
        .iter()
        .filter(|(_, _, host, port)| !host.trim_end_matches('.').is_empty() && *port != 0)
        .min_by_key(|(priority, weight, _, _)| (*priority, std::cmp::Reverse(*weight)))
        .map(|(_, _, host, port)| (host.trim_end_matches('.').to_string(), *port))
}

/// `_minecraft._tcp.<host>`; `None` when there is none (or DNS fails).
async fn srv_lookup(host: &str, timeout: Duration) -> Option<(String, u16)> {
    use hickory_resolver::proto::rr::{Name, RData, RecordType};

    let resolver = crate::monitor::checkers::dns::build_resolver(None, timeout).ok()?;
    let name = Name::from_utf8(format!("_minecraft._tcp.{}.", host.trim_end_matches('.'))).ok()?;
    let lookup = tokio::time::timeout(timeout, resolver.lookup(name, RecordType::SRV))
        .await
        .ok()?
        .ok()?;
    let records: Vec<SrvRecord> = lookup
        .answers()
        .iter()
        .filter_map(|r| match &r.data {
            RData::SRV(s) => Some((s.priority, s.weight, s.target.to_ascii(), s.port)),
            _ => None,
        })
        .collect();
    pick_srv(&records)
}

/// Where to connect: the SRV record's server when the target has no port
/// and names a host (not an IP), else the target itself.
async fn address(target: &Target, timeout: Duration) -> (String, u16, bool) {
    if !target.explicit_port && target.host.parse::<std::net::IpAddr>().is_err() {
        let budget = (timeout / 3).min(Duration::from_secs(3));
        if let Some((host, port)) = srv_lookup(&target.host, budget).await {
            return (host, port, true);
        }
    }
    (target.host.clone(), target.port, false)
}

pub async fn query(target: &Target, timeout: Duration) -> Result<(Status, i64), String> {
    let (host, port, via_srv) = address(target, timeout).await;
    let (mut tcp, _) = conn::tcp(&host, port).await?;
    let start = Instant::now();
    tcp.write_all(&status_request(&host, port))
        .await
        .map_err(|e| {
            format!(
                "Sending the status request failed: {}",
                util::tidy_io_error(&e)
            )
        })?;
    let body = read_packet(&mut tcp).await?;
    let status_ms = util::elapsed_ms(start);
    let mut status = parse_status(status_json(&body)?)?;
    if via_srv {
        status.detail = Some(format!(
            "via SRV {}",
            util::truncate(&format!("{host}:{port}"), 60)
        ));
    }

    // Ping/pong: the latency the game shows. Some proxies close after the
    // status reply; the status round trip stands in then.
    let payload = start.elapsed().as_nanos() as i64;
    let mut ping = vec![0x01];
    ping.extend_from_slice(&payload.to_be_bytes());
    let ping_start = Instant::now();
    let pong = async {
        tcp.write_all(&packet(&ping)).await.ok()?;
        let body = read_packet(&mut tcp).await.ok()?;
        (body.len() == 9 && body[0] == 0x01).then(|| util::elapsed_ms(ping_start))
    };
    let rtt = tokio::time::timeout(timeout / 3, pong)
        .await
        .ok()
        .flatten()
        .unwrap_or(status_ms);
    Ok((status, rtt))
}
