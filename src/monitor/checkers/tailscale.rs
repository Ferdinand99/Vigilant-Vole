//! Tailscale ping: `target` = `peer[?type=tsmp|icmp]`.
//!
//! Asks the local `tailscaled` (its LocalAPI, an HTTP server on a unix
//! socket) to ping a peer at the Tailscale layer, like `tailscale ping -c 1`:
//! a disco ping by default (the path WireGuard would take, without either
//! host's IP stack), `type=tsmp` through WireGuard, `type=icmp` through the
//! peer's IP stack. The peer is its machine name, MagicDNS name or Tailscale
//! IP; names are looked up in `/localapi/v0/status`. Up on a pong, with the
//! route it took ("via DERP(fra)" or "direct via 192.0.2.5:41641"); response
//! time = the latency tailscaled measured.
//!
//! The socket is `VV_TAILSCALE_SOCKET`, else the first of [`SOCKET_PATHS`]
//! that exists (in a container: mount the host's socket). Windows reaches
//! tailscaled's named pipe instead (untested).

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::util;
use crate::db::models::{CheckOutcome, HeartbeatStatus, Monitor};

/// Environment variable naming the tailscaled socket.
pub const SOCKET_ENV: &str = "VV_TAILSCALE_SOCKET";
/// Where tailscaled puts its socket: Linux packages, the official container
/// (`TS_SOCKET`), Synology, QNAP, the open-source macOS daemon.
pub const SOCKET_PATHS: &[&str] = &[
    "/var/run/tailscale/tailscaled.sock",
    "/tmp/tailscaled.sock",
    "/var/packages/Tailscale/var/tailscaled.sock",
    "/tmp/tailscale/tailscaled.sock",
    "/var/run/tailscaled.socket",
];
#[cfg(windows)]
const WINDOWS_PIPE: &str = r"\\.\pipe\ProtectedPrefix\Administrators\Tailscale\tailscaled";
/// The Host header LocalAPI insists on.
const LOCALAPI_HOST: &str = "local-tailscaled.sock";
/// Largest LocalAPI answer read (the status lists every peer).
const MAX_RESPONSE: u64 = 16 * 1024 * 1024;

/// What kind of ping (LocalAPI `type=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingType {
    Disco,
    Tsmp,
    Icmp,
}

impl PingType {
    fn api_name(self) -> &'static str {
        match self {
            PingType::Disco => "disco",
            PingType::Tsmp => "TSMP",
            PingType::Icmp => "ICMP",
        }
    }
}

/// A parsed `tailscale_ping` target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub peer: String,
    pub kind: PingType,
}

impl Target {
    pub fn parse(target: &str) -> Result<Target, String> {
        let t = target.trim();
        let (peer, query) = t.split_once('?').unwrap_or((t, ""));
        let peer = peer.trim();
        if peer.is_empty() {
            return Err("Enter the peer's machine name, MagicDNS name or Tailscale IP".into());
        }
        if peer.contains("://") || peer.contains('/') {
            return Err("Enter just the peer's name or Tailscale IP, without a scheme or path".into());
        }
        if peer.chars().any(char::is_whitespace) {
            return Err("The peer name can't contain spaces".into());
        }
        let mut kind = PingType::Disco;
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if !key.eq_ignore_ascii_case("type") {
                return Err(format!("Unknown option \"{}\" (only type=disco, tsmp or icmp)", util::truncate(&key, 20)));
            }
            kind = match value.trim().to_ascii_lowercase().as_str() {
                "disco" | "" => PingType::Disco,
                "tsmp" => PingType::Tsmp,
                "icmp" => PingType::Icmp,
                other => return Err(format!("Unknown ping type \"{}\" (use disco, tsmp or icmp)", util::truncate(other, 20))),
            };
        }
        Ok(Target { peer: peer.trim_end_matches('.').to_string(), kind })
    }
}

/// The parts of `/localapi/v0/status` read here.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct Status {
    backend_state: String,
    #[serde(rename = "Self")]
    self_node: Option<Node>,
    peer: Option<HashMap<String, Node>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct Node {
    host_name: String,
    #[serde(rename = "DNSName")]
    dns_name: String,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    online: bool,
    last_seen: Option<String>,
}

impl Node {
    /// MagicDNS name without the tailnet suffix, else the host name.
    fn short_name(&self) -> &str {
        match self.dns_name.split('.').next() {
            Some(label) if !label.is_empty() => label,
            _ => &self.host_name,
        }
    }

    fn ips(&self) -> &[String] {
        self.tailscale_ips.as_deref().unwrap_or_default()
    }

    /// "last seen 2026-10-20 09:14 UTC" when tailscaled knows.
    fn last_seen(&self) -> Option<String> {
        let ts = self.last_seen.as_deref()?.parse::<jiff::Timestamp>().ok()?;
        (ts.as_second() > 0).then(|| format!("last seen {}", ts.strftime("%Y-%m-%d %H:%M UTC")))
    }
}

/// `/localapi/v0/ping`'s answer (`ipnstate.PingResult`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct PingResult {
    #[serde(rename = "NodeIP")]
    node_ip: String,
    node_name: String,
    err: String,
    latency_seconds: f64,
    endpoint: String,
    peer_relay: String,
    #[serde(rename = "DERPRegionID")]
    derp_region_id: i64,
    #[serde(rename = "DERPRegionCode")]
    derp_region_code: String,
    #[serde(rename = "IsLocalIP")]
    is_local_ip: bool,
}

/// The peer a target names.
#[derive(Debug, PartialEq)]
struct Resolved {
    ip: String,
    name: String,
    /// What the control plane says, when the peer is known.
    online: Option<bool>,
    last_seen: Option<String>,
}

/// Finds `peer` in the status: as an IP, a full MagicDNS name, the first
/// label of one, or a host name (in that order of preference).
fn resolve(status: &Status, peer: &str) -> Result<Resolved, String> {
    let peers: Vec<&Node> = status.peer.iter().flat_map(HashMap::values).collect();
    if let Some(self_node) = &status.self_node {
        let is_self = peer.parse::<IpAddr>().map_or_else(
            |_| [self_node.dns_name.trim_end_matches('.'), self_node.short_name(), &self_node.host_name].iter().any(|n| n.eq_ignore_ascii_case(peer)),
            |ip| self_node.ips().iter().any(|own| *own == ip.to_string()),
        );
        if is_self {
            return Err(format!("\"{}\" is this host itself; pick another peer", util::truncate(peer, 60)));
        }
    }
    if let Ok(ip) = peer.parse::<IpAddr>() {
        let ip = ip.to_string();
        let node = peers.iter().find(|n| n.ips().contains(&ip));
        return Ok(Resolved {
            name: node.map_or_else(|| ip.clone(), |n| n.short_name().to_string()),
            online: node.map(|n| n.online),
            last_seen: node.and_then(|n| n.last_seen()),
            ip,
        });
    }
    let rules: [&dyn Fn(&Node) -> bool; 3] = [
        &|n| n.dns_name.trim_end_matches('.').eq_ignore_ascii_case(peer),
        &|n| !n.dns_name.is_empty() && n.short_name().eq_ignore_ascii_case(peer),
        &|n| n.host_name.eq_ignore_ascii_case(peer),
    ];
    let node = rules
        .iter()
        .find_map(|rule| {
            let mut found: Vec<&&Node> = peers.iter().filter(|n| rule(n)).collect();
            // Stable across runs (the status is a map).
            found.sort_by(|a, b| a.dns_name.cmp(&b.dns_name));
            found.first().copied().copied()
        })
        .ok_or_else(|| format!("No peer named \"{}\" in this tailnet (use its machine name, MagicDNS name or Tailscale IP)", util::truncate(peer, 60)))?;
    let ip = node.ips().first().cloned().ok_or_else(|| format!("Peer \"{}\" has no Tailscale IP", node.short_name()))?;
    Ok(Resolved { ip, name: node.short_name().to_string(), online: Some(node.online), last_seen: node.last_seen() })
}

/// Why tailscaled can't ping, from its `BackendState`.
fn backend_problem(state: &str) -> Option<String> {
    Some(match state {
        "Running" | "Starting" => return None,
        "NeedsLogin" => "Tailscale on this host is logged out (NeedsLogin): log it in with tailscale up".into(),
        "NeedsMachineAuth" => "Tailscale on this host waits for an admin to approve the machine (NeedsMachineAuth)".into(),
        "Stopped" => "Tailscale is stopped on this host (run tailscale up)".into(),
        other => format!("Tailscale isn't ready on this host ({})", util::truncate(if other.is_empty() { "no state" } else { other }, 40)),
    })
}

/// Turns tailscaled's ping answer into an outcome.
fn ping_outcome(result: &PingResult, peer: &Resolved, kind: PingType, wall_ms: i64) -> CheckOutcome {
    let name = if result.node_name.is_empty() { peer.name.as_str() } else { result.node_name.trim_end_matches('.').split('.').next().unwrap_or(&peer.name) };
    let ip = if result.node_ip.is_empty() { peer.ip.as_str() } else { result.node_ip.as_str() };
    if !result.err.is_empty() {
        if result.is_local_ip {
            return CheckOutcome::down(format!("{ip} is this host itself; pick another peer"));
        }
        return CheckOutcome::down(format!("Ping to {name} ({ip}) failed: {}{}", util::truncate(&result.err, 200), offline_note(peer)));
    }
    let via = if !result.peer_relay.is_empty() {
        format!("via peer relay {}", result.peer_relay)
    } else if result.derp_region_id != 0 {
        format!("via DERP({})", if result.derp_region_code.is_empty() { result.derp_region_id.to_string() } else { result.derp_region_code.clone() })
    } else if !result.endpoint.is_empty() {
        format!("direct via {}", result.endpoint)
    } else {
        format!("via {}", kind.api_name())
    };
    let latency = Duration::from_secs_f64(result.latency_seconds.clamp(0.0, 86_400.0));
    let ms = if result.latency_seconds > 0.0 { i64::try_from(latency.as_millis()).unwrap_or(i64::MAX) } else { wall_ms };
    CheckOutcome::up(ms).with_message(format!("pong from {name} ({ip}) {via} in {ms} ms"))
}

fn offline_note(peer: &Resolved) -> String {
    match (peer.online, &peer.last_seen) {
        (Some(false), Some(seen)) => format!("; Tailscale shows it offline ({seen})"),
        (Some(false), None) => "; Tailscale shows it offline".into(),
        _ => String::new(),
    }
}

/// The socket to use: the environment's, else the first that exists.
fn locate_socket() -> Result<String, String> {
    if let Some(path) = std::env::var(SOCKET_ENV).ok().filter(|p| !p.trim().is_empty()) {
        return Ok(path.trim().to_string());
    }
    #[cfg(windows)]
    {
        Ok(WINDOWS_PIPE.to_string())
    }
    #[cfg(not(windows))]
    {
        SOCKET_PATHS.iter().find(|p| std::path::Path::new(p).exists()).map(|p| (*p).to_string()).ok_or_else(|| {
            format!(
                "No tailscaled socket found: mount the host's /var/run/tailscale/tailscaled.sock into the container, or set {SOCKET_ENV} (looked in {})",
                SOCKET_PATHS.join(", ")
            )
        })
    }
}

/// One LocalAPI call over a fresh connection: `(status code, body)`.
async fn call(socket: &str, method: &str, path: &str) -> Result<(u16, Vec<u8>), String> {
    let explain = |e: std::io::Error| {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::NotFound => format!("The tailscaled socket {socket} doesn't exist (is Tailscale installed and the socket mounted?)"),
            ErrorKind::PermissionDenied => format!("No permission to open {socket}: run with access to the socket (root, or its group)"),
            ErrorKind::ConnectionRefused => format!("tailscaled isn't running ({socket} refuses connections)"),
            _ => format!("Can't reach tailscaled at {socket}: {}", util::tidy_io_error(&e)),
        }
    };
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(socket).await.map_err(explain)?;
        request(stream, method, path).await
    }
    #[cfg(windows)]
    {
        let pipe = tokio::net::windows::named_pipe::ClientOptions::new().open(socket).map_err(explain)?;
        request(pipe, method, path).await
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (explain, method, path);
        Err("Tailscale ping isn't supported on this platform".into())
    }
}

/// Minimal HTTP/1.1: one request, `Connection: close`, read to the end.
async fn request<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, method: &str, path: &str) -> Result<(u16, Vec<u8>), String> {
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {LOCALAPI_HOST}\r\nUser-Agent: vigilant-vole\r\nAccept: application/json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.map_err(|e| format!("Can't talk to tailscaled: {}", util::tidy_io_error(&e)))?;
    let mut raw = Vec::new();
    (&mut stream)
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut raw)
        .await
        .map_err(|e| format!("tailscaled hung up: {}", util::tidy_io_error(&e)))?;
    if raw.len() as u64 > MAX_RESPONSE {
        return Err("tailscaled's answer is too large".into());
    }
    parse_http(&raw)
}

fn parse_http(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let bad = || "tailscaled sent something that isn't HTTP".to_string();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(bad)?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad())?;
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|l| l.strip_prefix("HTTP/1.")).and_then(|l| l.get(2..5)).and_then(|c| c.parse::<u16>().ok()).ok_or_else(bad)?;
    let (mut chunked, mut length) = (false, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        } else if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse::<usize>().map_err(|_| bad())?);
        }
    }
    let body = if chunked {
        dechunk(body)?
    } else if let Some(len) = length {
        body.get(..len).ok_or("tailscaled's answer was cut short")?.to_vec()
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let cut = || "tailscaled's answer was cut short".to_string();
    let mut out = Vec::new();
    loop {
        let end = rest.windows(2).position(|w| w == b"\r\n").ok_or_else(cut)?;
        let size_text = std::str::from_utf8(&rest[..end]).map_err(|_| cut())?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16).map_err(|_| cut())?;
        rest = &rest[end + 2..];
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(rest.get(..size).ok_or_else(cut)?);
        rest = rest.get(size..).and_then(|r| r.strip_prefix(b"\r\n")).ok_or_else(cut)?;
    }
}

/// A non-200 answer as a message (LocalAPI errors are `{"error": "…"}` or text).
fn api_error(what: &str, status: u16, body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct ApiError {
        error: String,
    }
    let text = serde_json::from_slice::<ApiError>(body).map(|e| e.error).unwrap_or_else(|_| String::from_utf8_lossy(body).trim().to_string());
    let hint = if status == 403 { " (access denied: the socket's owner decides who may use it)" } else { "" };
    format!("tailscaled refused the {what} (HTTP {status}): {}{hint}", util::truncate(&text, 160))
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let target = match Target::parse(&monitor.target) {
        Ok(t) => t,
        Err(e) => return CheckOutcome::down(e),
    };
    let socket = match locate_socket() {
        Ok(s) => s,
        Err(e) => return CheckOutcome::down(e),
    };
    check_with(&socket, &target, super::timeout_of(monitor)).await
}

async fn check_with(socket: &str, target: &Target, timeout: Duration) -> CheckOutcome {
    let start = Instant::now();
    // Leave the engine's own timeout a moment, so these messages win.
    let deadline = start + timeout.mul_f64(0.95);
    let left = || deadline.saturating_duration_since(Instant::now());

    let status = match tokio::time::timeout(left(), call(socket, "GET", "/localapi/v0/status")).await {
        Err(_) => return CheckOutcome::down(format!("tailscaled at {socket} didn't answer within {}", util::format_secs(timeout))),
        Ok(Err(e)) => return CheckOutcome::down(e),
        Ok(Ok((200, body))) => match serde_json::from_slice::<Status>(&body) {
            Ok(s) => s,
            Err(e) => return CheckOutcome::down(format!("Can't read tailscaled's status: {e}")),
        },
        Ok(Ok((code, body))) => return CheckOutcome::down(api_error("status request", code, &body)),
    };
    if let Some(problem) = backend_problem(&status.backend_state) {
        return CheckOutcome::down(problem);
    }
    let peer = match resolve(&status, &target.peer) {
        Ok(p) => p,
        Err(e) => return CheckOutcome::down(e),
    };
    let path = format!("/localapi/v0/ping?ip={}&type={}", peer.ip, target.kind.api_name());
    let ping_start = Instant::now();
    let answer = match tokio::time::timeout(left(), call(socket, "POST", &path)).await {
        Err(_) => {
            return CheckOutcome::down(format!(
                "No pong from {} ({}) within {}{}",
                peer.name,
                peer.ip,
                util::format_secs(timeout),
                offline_note(&peer)
            ));
        }
        Ok(Err(e)) => return CheckOutcome::down(e),
        Ok(Ok(answer)) => answer,
    };
    match answer {
        (200, body) => match serde_json::from_slice::<PingResult>(&body) {
            Ok(result) => ping_outcome(&result, &peer, target.kind, util::elapsed_ms(ping_start)),
            Err(e) => CheckOutcome::down(format!("Can't read tailscaled's ping answer: {e}")),
        },
        (code, body) => {
            let mut out = CheckOutcome::down(format!("{}{}", api_error("ping", code, &body), offline_note(&peer)));
            // tailscaled says "Starting" while it connects: not a verdict yet.
            if status.backend_state == "Starting" {
                out.status = HeartbeatStatus::Pending;
            }
            out
        }
    }
}

#[cfg(test)]
mod tests;
