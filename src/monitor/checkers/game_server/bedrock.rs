//! Minecraft: Bedrock Edition: RakNet unconnected ping over UDP. The pong
//! carries a `;`-separated status string:
//! `edition;MOTD;protocol;version;online;max;server id;level;game mode;…`.

use std::time::Duration;

use super::{Status, Target, Verdict, strip_section_codes, udp_exchange};

/// RakNet's "offline message" magic.
pub const MAGIC: [u8; 16] = [0x00, 0xFF, 0xFF, 0x00, 0xFE, 0xFE, 0xFE, 0xFE, 0xFD, 0xFD, 0xFD, 0xFD, 0x12, 0x34, 0x56, 0x78];
const UNCONNECTED_PING: u8 = 0x01;
const UNCONNECTED_PONG: u8 = 0x1C;

/// ID_UNCONNECTED_PING: time (our nonce), magic, client GUID.
pub fn ping(nonce: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(33);
    out.push(UNCONNECTED_PING);
    out.extend_from_slice(&nonce.to_be_bytes());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&0x5656_u64.to_be_bytes());
    out
}

/// Reads an ID_UNCONNECTED_PONG. Replies that don't echo `nonce` are not ours.
pub(super) fn parse_pong(buf: &[u8], nonce: u64) -> Verdict<Status> {
    if buf.first() != Some(&UNCONNECTED_PONG) || buf.len() < 35 {
        return Verdict::Done(Err("Not a Bedrock (RakNet) pong".into()));
    }
    if buf[1..9] != nonce.to_be_bytes() {
        return Verdict::Ignore;
    }
    if buf[17..33] != MAGIC {
        return Verdict::Done(Err("Not a Bedrock (RakNet) pong (wrong magic)".into()));
    }
    let len = usize::from(u16::from_be_bytes([buf[33], buf[34]]));
    let Some(raw) = buf.get(35..35 + len) else {
        return Verdict::Done(Err("Truncated Bedrock pong".into()));
    };
    Verdict::Done(parse_status(&String::from_utf8_lossy(raw)))
}

/// The status string: `MCPE;Vole SMP;766;1.21.50;3;10;…`.
pub fn parse_status(text: &str) -> Result<Status, String> {
    let f: Vec<&str> = text.split(';').collect();
    if f.len() < 6 {
        return Err("Unreadable Bedrock status (too few fields)".into());
    }
    let edition = match f[0] {
        "MCEE" => "Minecraft Education",
        _ => "Bedrock",
    };
    let version = f[3].trim();
    let product = if version.is_empty() { edition.to_string() } else { format!("{edition} {version}") };
    let players = match (f[4].trim().parse::<u64>(), f[5].trim().parse::<u64>()) {
        (Ok(online), Ok(max)) => Some((online, max)),
        _ => None,
    };
    Ok(Status { product, players, name: strip_section_codes(f[1]), detail: None })
}

pub async fn query(target: &Target, timeout: Duration) -> Result<(Status, i64), String> {
    let nonce = super::nonce();
    udp_exchange(&target.host, target.port, &ping(nonce), timeout, |buf| parse_pong(buf, nonce)).await
}
