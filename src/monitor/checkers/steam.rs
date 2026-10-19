//! Steam / Source game server checker: A2S_INFO over UDP.
//!
//! `target`: `host:port` (default port 27015). Handles the S2C_CHALLENGE
//! (0x41) flow introduced in 2020, Source (`I`) and legacy GoldSrc (`m`)
//! replies. Split (multi-packet) replies are not supported — A2S_INFO fits
//! one datagram on every known server. Message: "de_dust2 · 12/24 players ·
//! Server name"; response time = the final request/reply round trip.

use std::time::Instant;

use tokio::net::UdpSocket;

use super::util;
use crate::db::models::{CheckOutcome, Monitor};

const HEADER: [u8; 4] = [0xFF; 4];
const QUERY: &[u8] = b"TSource Engine Query\0";
const MAX_CHALLENGES: usize = 3;

pub fn encode_request(challenge: Option<[u8; 4]>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + QUERY.len() + 4);
    buf.extend_from_slice(&HEADER);
    buf.extend_from_slice(QUERY);
    if let Some(c) = challenge {
        buf.extend_from_slice(&c);
    }
    buf
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub map: String,
    pub game: String,
    pub players: u8,
    pub max_players: u8,
    pub bots: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Challenge([u8; 4]),
    Info(ServerInfo),
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, String> {
        let b = *self.buf.get(self.pos).ok_or("Truncated A2S_INFO reply")?;
        self.pos += 1;
        Ok(b)
    }

    fn skip(&mut self, n: usize) -> Result<(), String> {
        if self.pos + n > self.buf.len() {
            return Err("Truncated A2S_INFO reply".into());
        }
        self.pos += n;
        Ok(())
    }

    fn cstr(&mut self) -> Result<String, String> {
        let rest = self.buf.get(self.pos..).ok_or("Truncated A2S_INFO reply")?;
        let end = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or("Unterminated string in A2S_INFO reply")?;
        let s = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.pos += end + 1;
        Ok(s)
    }
}

/// Parse one datagram from the server.
pub fn parse_reply(buf: &[u8]) -> Result<Reply, String> {
    if buf.starts_with(&[0xFE, 0xFF, 0xFF, 0xFF]) {
        return Err("Server sent a split reply (not supported for A2S_INFO)".into());
    }
    if !buf.starts_with(&HEADER) || buf.len() < 5 {
        return Err("Not an A2S reply".into());
    }
    let mut r = Reader { buf, pos: 5 };
    match buf[4] {
        0x41 => {
            let c = buf.get(5..9).ok_or("Truncated challenge reply")?;
            Ok(Reply::Challenge([c[0], c[1], c[2], c[3]]))
        }
        0x49 => {
            r.u8()?; // protocol
            let name = r.cstr()?;
            let map = r.cstr()?;
            r.cstr()?; // folder
            let game = r.cstr()?;
            r.skip(2)?; // app id
            let players = r.u8()?;
            let max_players = r.u8()?;
            let bots = r.u8()?;
            Ok(Reply::Info(ServerInfo {
                name,
                map,
                game,
                players,
                max_players,
                bots,
            }))
        }
        0x6D => {
            r.cstr()?; // address
            let name = r.cstr()?;
            let map = r.cstr()?;
            r.cstr()?; // folder
            let game = r.cstr()?;
            let players = r.u8()?;
            let max_players = r.u8()?;
            Ok(Reply::Info(ServerInfo {
                name,
                map,
                game,
                players,
                max_players,
                bots: 0,
            }))
        }
        other => Err(format!("Unexpected A2S reply type 0x{other:02X}")),
    }
}

/// "de_dust2 · 12/24 players · Server name".
pub fn describe(info: &ServerInfo) -> String {
    let mut msg = format!(
        "{} · {}/{} players",
        info.map, info.players, info.max_players
    );
    if info.bots > 0 {
        msg.push_str(&format!(" ({} bots)", info.bots));
    }
    if !info.name.trim().is_empty() {
        msg.push_str(" · ");
        msg.push_str(&util::truncate(info.name.trim(), 60));
    }
    msg
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let (host, port) = match util::split_host_port(&monitor.target, Some(27015)) {
        Ok(hp) => hp,
        Err(e) => return CheckOutcome::down(e),
    };
    let addr = match util::resolve(&host, port).await {
        Ok(a) => a,
        Err(e) => return CheckOutcome::down(e),
    };
    let bind = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = match UdpSocket::bind(bind).await {
        Ok(s) => s,
        Err(e) => {
            return CheckOutcome::down(format!(
                "Could not open UDP socket: {}",
                util::tidy_io_error(&e)
            ));
        }
    };
    if let Err(e) = socket.connect(addr).await {
        return CheckOutcome::down(format!(
            "Could not reach {addr}: {}",
            util::tidy_io_error(&e)
        ));
    }

    let mut challenge = None;
    let mut buf = [0u8; 1400];
    for _ in 0..=MAX_CHALLENGES {
        let start = Instant::now();
        if let Err(e) = socket.send(&encode_request(challenge)).await {
            return CheckOutcome::down(format!(
                "Could not send query: {}",
                util::tidy_io_error(&e)
            ));
        }
        let n = match socket.recv(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                return CheckOutcome::down(format!(
                    "No reply from {addr}: {}",
                    util::udp_recv_error(&e)
                ));
            }
        };
        let rtt = util::elapsed_ms(start);
        match parse_reply(&buf[..n]) {
            Ok(Reply::Info(info)) => return CheckOutcome::up(rtt).with_message(describe(&info)),
            Ok(Reply::Challenge(c)) => challenge = Some(c),
            Err(e) => return CheckOutcome::down(e).with_response_time(rtt),
        }
    }
    CheckOutcome::down("Server kept sending challenges instead of server info")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Example Source reply from the Valve developer wiki.
    const SOURCE_REPLY: &[u8] = &[
        0xFF, 0xFF, 0xFF, 0xFF, 0x49, 0x02, 0x67, 0x61, 0x6D, 0x65, 0x32, 0x78, 0x73, 0x2E, 0x63,
        0x6F, 0x6D, 0x20, 0x43, 0x6F, 0x75, 0x6E, 0x74, 0x65, 0x72, 0x2D, 0x53, 0x74, 0x72, 0x69,
        0x6B, 0x65, 0x20, 0x53, 0x6F, 0x75, 0x72, 0x63, 0x65, 0x20, 0x23, 0x31, 0x00, 0x64, 0x65,
        0x5F, 0x64, 0x75, 0x73, 0x74, 0x00, 0x63, 0x73, 0x74, 0x72, 0x69, 0x6B, 0x65, 0x00, 0x43,
        0x6F, 0x75, 0x6E, 0x74, 0x65, 0x72, 0x2D, 0x53, 0x74, 0x72, 0x69, 0x6B, 0x65, 0x3A, 0x20,
        0x53, 0x6F, 0x75, 0x72, 0x63, 0x65, 0x00, 0xF0, 0x00, 0x05, 0x10, 0x04, 0x64, 0x6C, 0x00,
        0x00, 0x31, 0x2E, 0x30, 0x2E, 0x30, 0x2E, 0x32, 0x32, 0x00,
    ];

    #[test]
    fn parses_source_reply() {
        let Reply::Info(info) = parse_reply(SOURCE_REPLY).unwrap() else {
            panic!("expected info")
        };
        assert_eq!(info.name, "game2xs.com Counter-Strike Source #1");
        assert_eq!(info.map, "de_dust");
        assert_eq!(info.game, "Counter-Strike: Source");
        assert_eq!((info.players, info.max_players, info.bots), (5, 16, 4));
        assert_eq!(
            describe(&info),
            "de_dust · 5/16 players (4 bots) · game2xs.com Counter-Strike Source #1"
        );
    }

    #[test]
    fn parses_challenge() {
        assert_eq!(
            parse_reply(&[0xFF, 0xFF, 0xFF, 0xFF, 0x41, 0x0A, 0x08, 0x5E, 0xEA]),
            Ok(Reply::Challenge([0x0A, 0x08, 0x5E, 0xEA]))
        );
    }

    #[test]
    fn parses_goldsrc_reply() {
        let mut b = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x6D];
        for s in [
            "1.2.3.4:27015",
            "HL Server",
            "crossfire",
            "valve",
            "Half-Life",
        ] {
            b.extend_from_slice(s.as_bytes());
            b.push(0);
        }
        b.extend_from_slice(&[3, 12, 47]);
        let Reply::Info(info) = parse_reply(&b).unwrap() else {
            panic!("expected info")
        };
        assert_eq!(
            (info.map.as_str(), info.players, info.max_players),
            ("crossfire", 3, 12)
        );
    }

    #[test]
    fn rejects_bad_replies() {
        assert!(parse_reply(&SOURCE_REPLY[..20]).is_err());
        assert!(
            parse_reply(&[0xFE, 0xFF, 0xFF, 0xFF, 0x00])
                .unwrap_err()
                .contains("split")
        );
        assert!(parse_reply(b"hello").is_err());
        assert!(parse_reply(&[0xFF, 0xFF, 0xFF, 0xFF, 0x42]).is_err());
    }

    #[test]
    fn request_encoding() {
        let r = encode_request(None);
        assert_eq!(&r[..5], &[0xFF, 0xFF, 0xFF, 0xFF, 0x54]);
        assert_eq!(r.len(), 25);
        assert!(r.ends_with(b"Query\0"));
        assert!(encode_request(Some([1, 2, 3, 4])).ends_with(&[0, 1, 2, 3, 4]));
    }

    #[tokio::test]
    async fn challenge_flow_against_local_server() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            // 1st: demand a challenge; 2nd: answer only if it is echoed back.
            if let Ok((_, peer)) = server.recv_from(&mut buf).await {
                let _ = server
                    .send_to(&[0xFF, 0xFF, 0xFF, 0xFF, 0x41, 9, 8, 7, 6], peer)
                    .await;
            }
            if let Ok((n, peer)) = server.recv_from(&mut buf).await
                && buf[..n].ends_with(&[9, 8, 7, 6])
            {
                let _ = server.send_to(SOURCE_REPLY, peer).await;
            }
        });
        let m = crate::monitor::checkers::test_support::monitor(
            crate::db::models::MonitorType::Steam,
            &addr.to_string(),
        );
        let out = check(&m).await;
        assert_eq!(
            out.status,
            crate::db::models::HeartbeatStatus::Up,
            "{:?}",
            out.message
        );
        assert!(out.message.unwrap().starts_with("de_dust · 5/16 players"));
    }
}
