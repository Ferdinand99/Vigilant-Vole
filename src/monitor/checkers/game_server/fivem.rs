//! FiveM / RedM (FXServer): its HTTP endpoints on the game port.
//! `/info.json` proves it is an FXServer (build, project name, game);
//! `/dynamic.json` has the host name and player count (`/players.json`
//! stands in when a server hides it).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{Status, Target, collapse_whitespace};
use crate::monitor::checkers::util;

/// Largest reply read (a big server's `info.json` lists hundreds of resources).
const MAX_BODY: usize = 2 << 20;

/// Shared client for the game HTTP APIs: no proxy, no redirects, and no
/// certificate check (Satisfactory's API is self-signed by default).
pub(super) fn client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            crate::tls_roots::http_client()
                .user_agent(crate::server::USER_AGENT)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .danger_accept_invalid_certs(true)
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| format!("Could not build HTTP client: {}", util::root_cause(&e)))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The body, refusing more than [`MAX_BODY`] bytes.
pub(super) async fn read_capped(mut reply: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = reply.chunk().await.map_err(|e| util::root_cause(&e))? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err("reply too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn get_json(base: &str, path: &str, timeout: Duration) -> Result<Value, String> {
    let reply = client()?
        .get(format!("{base}{path}"))
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("{path}: {}", util::root_cause(&e)))?;
    let status = reply.status();
    if !status.is_success() {
        return Err(format!("{path}: HTTP {}", status.as_u16()));
    }
    let body = read_capped(reply)
        .await
        .map_err(|e| format!("{path}: {e}"))?;
    serde_json::from_slice(&body)
        .map_err(|_| format!("{path} isn't JSON (not a FiveM/RedM server?)"))
}

/// Removes FiveM's `^0`–`^9` colour codes.
pub fn strip_caret_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '^' && chars.peek().is_some_and(char::is_ascii_digit) {
            chars.next();
        } else {
            out.push(c);
        }
    }
    collapse_whitespace(&out)
}

/// A number that FXServer may send as text (`"48"`).
fn number(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Game and build from `info.json`: "FiveM build 7290", "RedM build 1491".
pub fn product(info: &Value) -> Result<String, String> {
    let vars = info.get("vars").filter(|v| v.is_object());
    let server = info["server"].as_str();
    if vars.is_none() && server.is_none() {
        return Err("/info.json has no server details (not a FiveM/RedM server?)".into());
    }
    let game = match vars.and_then(|v| v["gamename"].as_str()) {
        Some(g) if g.eq_ignore_ascii_case("rdr3") => "RedM",
        _ => "FiveM",
    };
    // "FXServer-master SERVER v1.0.0.7290 win32"
    let build = server
        .and_then(|s| s.split_whitespace().find_map(|w| w.strip_prefix("v1.0.0.")))
        .filter(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_digit()));
    Ok(match build {
        Some(b) => format!("{game} build {b}"),
        None => game.to_string(),
    })
}

/// Name and players from `dynamic.json`, falling back to `info.json`'s vars.
pub fn status_of(
    info: &Value,
    dynamic: Option<&Value>,
    player_list: Option<usize>,
) -> Result<Status, String> {
    let vars = &info["vars"];
    let name = dynamic
        .and_then(|d| d["hostname"].as_str())
        .or_else(|| vars["sv_projectName"].as_str())
        .or_else(|| vars["sv_hostname"].as_str())
        .map(strip_caret_codes)
        .unwrap_or_default();
    let max = dynamic
        .and_then(|d| number(&d["sv_maxclients"]))
        .or_else(|| number(&vars["sv_maxClients"]));
    let online = dynamic
        .and_then(|d| number(&d["clients"]))
        .or(player_list.map(|n| n as u64));
    Ok(Status {
        product: product(info)?,
        players: online.zip(max),
        name,
        detail: None,
    })
}

pub async fn query(target: &Target, timeout: Duration) -> Result<(Status, i64), String> {
    let host = if target.host.contains(':') {
        format!("[{}]", target.host)
    } else {
        target.host.clone()
    };
    let base = format!("http://{host}:{}", target.port);
    let start = Instant::now();
    let info = get_json(&base, "/info.json", timeout).await?;
    let rtt = util::elapsed_ms(start);
    let rest = timeout.saturating_sub(start.elapsed()) / 2;
    let dynamic = get_json(&base, "/dynamic.json", rest).await.ok();
    let players = match &dynamic {
        Some(_) => None,
        None => get_json(&base, "/players.json", rest)
            .await
            .ok()
            .and_then(|p| p.as_array().map(Vec::len)),
    };
    Ok((status_of(&info, dynamic.as_ref(), players)?, rtt))
}
