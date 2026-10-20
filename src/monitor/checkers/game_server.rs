//! Game server checker: a status query in the game's own protocol.
//!
//! `target`: `game://host[:port]` where the scheme picks the protocol:
//!
//! | scheme          | protocol                                   | default port |
//! |-----------------|--------------------------------------------|--------------|
//! | `minecraft://`  | Java Edition Server List Ping (TCP)        | 25565        |
//! | `bedrock://`    | RakNet unconnected ping (UDP)              | 19132        |
//! | `satisfactory://` | Lightweight Query API (UDP)              | 7777         |
//! | `fivem://`      | `/info.json` + `/dynamic.json` (HTTP)      | 30120        |
//! | `mumble://`     | Mumble UDP ping                            | 64738        |
//!
//! A `minecraft://` target without a port looks up `_minecraft._tcp.<host>`
//! first, like the game does. The keyword (optional) must appear in the
//! server name / MOTD. `secret` is the Satisfactory HTTPS API token: with
//! it the check also reads the player count. Valve/Source games use the
//! Steam type (A2S_INFO), not this one.
//!
//! Message: "Minecraft 1.21.4 · 12/100 players · A Vole SMP"; response
//! time = the protocol's own round trip (Minecraft: ping/pong).

mod bedrock;
mod fivem;
mod minecraft;
mod mumble;
mod satisfactory;
#[cfg(test)]
mod tests;

use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use super::endpoint::{Endpoint, Scheme};
use super::util;
use crate::db::models::{CheckOutcome, Monitor};

/// The target schemes, in [`Game`] order.
static SCHEMES: [&str; 5] = ["minecraft", "bedrock", "satisfactory", "fivem", "mumble"];

/// The games this type speaks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Game {
    Minecraft,
    Bedrock,
    Satisfactory,
    FiveM,
    Mumble,
}

impl Game {
    pub const ALL: [Game; 5] = [Game::Minecraft, Game::Bedrock, Game::Satisfactory, Game::FiveM, Game::Mumble];

    /// The target's scheme.
    pub fn scheme(self) -> &'static str {
        SCHEMES[self as usize]
    }

    pub fn default_port(self) -> u16 {
        match self {
            Game::Minecraft => 25565,
            Game::Bedrock => 19132,
            Game::Satisfactory => 7777,
            Game::FiveM => 30120,
            Game::Mumble => 64738,
        }
    }

    /// Name in the form's game picker.
    pub fn label(self) -> &'static str {
        match self {
            Game::Minecraft => "Minecraft: Java Edition",
            Game::Bedrock => "Minecraft: Bedrock Edition",
            Game::Satisfactory => "Satisfactory",
            Game::FiveM => "FiveM / RedM",
            Game::Mumble => "Mumble",
        }
    }

    pub fn from_scheme(scheme: &str) -> Option<Game> {
        Game::ALL.into_iter().find(|g| g.scheme().eq_ignore_ascii_case(scheme))
    }
}

/// A parsed `game://host[:port]` target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub game: Game,
    pub host: String,
    pub port: u16,
    /// The port was written out (a Minecraft target without one tries SRV).
    pub explicit_port: bool,
}

fn schemes_hint() -> String {
    let all: Vec<String> = Game::ALL.iter().map(|g| format!("{}://", g.scheme())).collect();
    all.join(", ")
}

pub fn parse_target(target: &str) -> Result<Target, String> {
    let t = target.trim();
    if t.is_empty() {
        return Err("Target is empty".into());
    }
    let Some((name, _)) = t.split_once("://") else {
        return Err(format!("Start the address with the game: {}", schemes_hint()));
    };
    let game = Game::from_scheme(name)
        .ok_or_else(|| format!("Unsupported game \"{}://\" (use {}; Valve/Steam games use the Steam type)", util::truncate(name, 20), schemes_hint()))?;
    // Port 0 marks "not given", so the game's own default (or SRV) applies.
    let scheme = Scheme { plain: std::slice::from_ref(&SCHEMES[game as usize]), tls: &[], port: 0, tls_port: 0 };
    let plain = format!("{}{}", game.scheme(), &t[name.len()..]);
    let ep = super::endpoint::parse_with(&scheme, &plain)?;
    if !ep.path.is_empty() || !ep.query.is_empty() {
        return Err("A game server address has no path or options: game://host[:port]".into());
    }
    let explicit_port = ep.port != 0;
    Ok(Target { game, host: ep.host, port: if explicit_port { ep.port } else { game.default_port() }, explicit_port })
}

/// The target as a service [`Endpoint`] (for the form's "same server" test
/// and the metrics labels).
pub fn endpoint(target: &str) -> Result<Endpoint, String> {
    let t = parse_target(target)?;
    Ok(Endpoint { tls: false, host: t.host, port: t.port, path: String::new(), query: Vec::new(), more_hosts: Vec::new(), srv: false })
}

/// What a server said about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Game and version: "Minecraft 1.21.4", "Mumble 1.5.735".
    pub product: String,
    /// Online / maximum.
    pub players: Option<(u64, u64)>,
    /// Server name or MOTD, formatting codes removed ("" when the protocol has none).
    pub name: String,
    /// Anything else worth showing ("Playing", "via SRV …").
    pub detail: Option<String>,
}

/// "Minecraft 1.21.4 · 12/100 players · A Vole SMP".
pub fn describe(game: Game, s: &Status) -> String {
    let mut parts = vec![s.product.clone()];
    if let Some(detail) = s.detail.as_deref().filter(|d| !d.is_empty()) {
        parts.push(detail.to_string());
    }
    if let Some((online, max)) = s.players {
        let noun = if game == Game::Mumble { "users" } else { "players" };
        parts.push(format!("{online}/{max} {noun}"));
    }
    if !s.name.is_empty() {
        parts.push(util::truncate(&s.name, 80));
    }
    parts.join(" · ")
}

/// Removes Minecraft's `§x` formatting codes and collapses whitespace
/// (MOTDs are often two lines).
pub fn strip_section_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
        } else {
            out.push(c);
        }
    }
    collapse_whitespace(&out)
}

pub fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let target = match parse_target(&monitor.target) {
        Ok(t) => t,
        Err(e) => return CheckOutcome::down(e),
    };
    let timeout = super::timeout_of(monitor);
    let result = match target.game {
        Game::Minecraft => minecraft::query(&target, timeout).await,
        Game::Bedrock => bedrock::query(&target, timeout).await,
        Game::Satisfactory => satisfactory::query(&target, monitor.secret.as_deref().filter(|s| !s.trim().is_empty()), timeout).await,
        Game::FiveM => fivem::query(&target, timeout).await,
        Game::Mumble => mumble::query(&target, timeout).await,
    };
    let (status, rtt) = match result {
        Ok(ok) => ok,
        Err(e) => return CheckOutcome::down(e),
    };
    let message = describe(target.game, &status);
    match monitor.keyword.as_deref().filter(|k| !k.is_empty()) {
        Some(keyword) if status.name.contains(keyword) == monitor.keyword_invert => CheckOutcome::down(format!(
            "Server name {} \"{}\": {message}",
            if monitor.keyword_invert { "contains" } else { "doesn't contain" },
            util::truncate(keyword, 40)
        ))
        .with_response_time(rtt),
        _ => CheckOutcome::up(rtt).with_message(message),
    }
}

// ---------------------------------------------------------------------------
// UDP request/reply with resends
// ---------------------------------------------------------------------------

/// How long to wait for a datagram before sending the request again.
const RESEND_AFTER: Duration = Duration::from_millis(1_000);

/// What [`udp_exchange`] does with a datagram.
enum Verdict<T> {
    /// Not ours (stale reply, wrong nonce): keep waiting.
    Ignore,
    Done(Result<T, String>),
}

/// Sends `request` to `host:port` over UDP and waits for a reply `judge`
/// accepts, sending again every [`RESEND_AFTER`] (datagrams get lost) until
/// most of `timeout` is used. Returns the reply and the round trip since
/// the last send, in milliseconds.
async fn udp_exchange<T>(host: &str, port: u16, request: &[u8], timeout: Duration, judge: impl Fn(&[u8]) -> Verdict<T>) -> Result<(T, i64), String> {
    let addr = util::resolve(host, port).await?;
    let bind = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let socket = UdpSocket::bind(bind).await.map_err(|e| format!("Could not open UDP socket: {}", util::tidy_io_error(&e)))?;
    socket.connect(addr).await.map_err(|e| format!("Could not reach {addr}: {}", util::tidy_io_error(&e)))?;
    let deadline = Instant::now() + timeout.mul_f32(0.9);
    let mut buf = vec![0u8; 2048];
    loop {
        let sent = Instant::now();
        socket.send(request).await.map_err(|e| format!("Could not send query: {}", util::tidy_io_error(&e)))?;
        let resend_at = (sent + RESEND_AFTER).min(deadline);
        loop {
            let wait = resend_at.saturating_duration_since(Instant::now());
            match tokio::time::timeout(wait, socket.recv(&mut buf)).await {
                Ok(Ok(n)) => match judge(&buf[..n]) {
                    Verdict::Ignore => continue,
                    Verdict::Done(result) => return result.map(|v| (v, util::elapsed_ms(sent))),
                },
                Ok(Err(e)) => return Err(format!("No reply from {addr}: {}", util::udp_recv_error(&e))),
                Err(_) => break,
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("No reply from {addr} within {}", util::format_secs(timeout)));
        }
    }
}

/// A 64-bit value that differs between checks (for nonces/cookies).
fn nonce() -> u64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    (nanos as u64) ^ 0x5654_564f_4c45_2021
}
