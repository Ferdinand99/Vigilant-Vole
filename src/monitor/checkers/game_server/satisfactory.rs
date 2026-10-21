//! Satisfactory (1.0+) dedicated server: the Lightweight Query API over UDP
//! (server state, build and name; no login), plus, with an API token, the
//! HTTPS API's `QueryServerState` for the player count. The HTTPS API
//! serves a self-signed certificate unless the owner installed one, so it
//! isn't verified (the token is sent to the address the monitor names).
//!
//! UDP request: magic `D5 F6`, type 0 (poll), version 1, 8-byte cookie,
//! terminator 1. Reply (little-endian): magic, type 1, version, cookie,
//! state u8, build u32, flags u64, sub-state count u8 + 3 bytes each,
//! name length u16, name.

use std::time::Duration;

use serde_json::{Value, json};

use super::{Status, Target, Verdict, collapse_whitespace, udp_exchange};
use crate::monitor::checkers::util;

const MAGIC: [u8; 2] = [0xD5, 0xF6];

pub fn poll(cookie: u64) -> [u8; 13] {
    let mut out = [0u8; 13];
    out[..4].copy_from_slice(&[MAGIC[0], MAGIC[1], 0, 1]);
    out[4..12].copy_from_slice(&cookie.to_le_bytes());
    out[12] = 1;
    out
}

pub(super) fn parse_reply(buf: &[u8], cookie: u64) -> Verdict<Status> {
    if buf.len() < 4 || buf[..2] != MAGIC || buf[2] != 1 {
        return Verdict::Done(Err("Not a Satisfactory query reply".into()));
    }
    if buf.get(4..12) != Some(&cookie.to_le_bytes()[..]) {
        return Verdict::Ignore;
    }
    Verdict::Done(parse_state(&buf[12..]))
}

fn parse_state(body: &[u8]) -> Result<Status, String> {
    const SHORT: &str = "Truncated Satisfactory query reply";
    let state = *body.first().ok_or(SHORT)?;
    let build = body
        .get(1..5)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(SHORT)?;
    let substates = usize::from(*body.get(13).ok_or(SHORT)?);
    let at = 14 + substates * 3;
    let len = body
        .get(at..at + 2)
        .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
        .ok_or(SHORT)?;
    let name = body.get(at + 2..at + 2 + len).ok_or(SHORT)?;
    let detail = match state {
        0 => return Err("The server reports that it is offline".into()),
        1 => "idle, no game loaded",
        2 => "loading a game",
        3 => "playing",
        _ => "unknown state",
    };
    Ok(Status {
        product: format!("Satisfactory build {build}"),
        players: None,
        name: collapse_whitespace(&String::from_utf8_lossy(name)),
        detail: Some(detail.into()),
    })
}

/// Players online / limit from a `QueryServerState` reply.
pub fn players_of(reply: &Value) -> Option<(u64, u64)> {
    let state = &reply["data"]["serverGameState"];
    Some((
        state["numConnectedPlayers"].as_u64()?,
        state["playerLimit"].as_u64()?,
    ))
}

async fn api_players(
    target: &Target,
    token: &str,
    timeout: Duration,
) -> Result<(u64, u64), String> {
    let host = if target.host.contains(':') {
        format!("[{}]", target.host)
    } else {
        target.host.clone()
    };
    let url = format!("https://{host}:{}/api/v1", target.port);
    let reply = super::fivem::client()?
        .post(url)
        .bearer_auth(token.trim())
        .json(&json!({ "function": "QueryServerState" }))
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("HTTPS API: {}", util::root_cause(&e)))?;
    let status = reply.status();
    let body = super::fivem::read_capped(reply)
        .await
        .map_err(|e| format!("HTTPS API: {e}"))?;
    if !status.is_success() {
        let hint = if status.as_u16() == 401 || status.as_u16() == 403 {
            " (check the API token)"
        } else {
            ""
        };
        return Err(format!("HTTPS API: HTTP {}{hint}", status.as_u16()));
    }
    let v: Value =
        serde_json::from_slice(&body).map_err(|_| "HTTPS API: unreadable reply".to_string())?;
    players_of(&v).ok_or_else(|| "HTTPS API: no player count in the reply".to_string())
}

pub async fn query(
    target: &Target,
    token: Option<&str>,
    timeout: Duration,
) -> Result<(Status, i64), String> {
    let cookie = super::nonce();
    let (mut status, rtt) =
        udp_exchange(&target.host, target.port, &poll(cookie), timeout, |buf| {
            parse_reply(buf, cookie)
        })
        .await?;
    if let Some(token) = token {
        match api_players(target, token, timeout / 2).await {
            Ok(players) => status.players = Some(players),
            Err(e) => {
                let detail = status.detail.take().unwrap_or_default();
                status.detail = Some(format!("{detail} · players unknown ({e})"));
            }
        }
    }
    Ok((status, rtt))
}
