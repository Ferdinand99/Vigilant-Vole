//! RADIUS checker: `target` = `radius://host[:port]` (default port 1812).
//!
//! With a user name and password it sends an `Access-Request` (PAP) and is up
//! on `Access-Accept` (an `Access-Challenge` also counts: the server answers
//! and understood the request); `Access-Reject` is down. Without a user name
//! it sends a `Status-Server` request (RFC 5997), which servers answer
//! without touching any account. Both carry a `Message-Authenticator`, and the
//! reply's authenticator is verified with the shared secret (the monitor's
//! secret), so a wrong secret is reported as such. The request is repeated
//! once when no answer arrives.

use std::time::{Duration, Instant};

use hmac::{Hmac, Mac as _};
use md5::{Digest as _, Md5};
use tokio::net::UdpSocket;

use super::endpoint;
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

const ACCESS_REQUEST: u8 = 1;
const ACCESS_ACCEPT: u8 = 2;
const ACCESS_REJECT: u8 = 3;
const ACCESS_CHALLENGE: u8 = 11;
const STATUS_SERVER: u8 = 12;

const USER_NAME: u8 = 1;
const USER_PASSWORD: u8 = 2;
const REPLY_MESSAGE: u8 = 18;
const NAS_IDENTIFIER: u8 = 32;
const MESSAGE_AUTHENTICATOR: u8 = 80;

fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut h = Md5::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn hmac_md5(secret: &[u8], data: &[u8]) -> [u8; 16] {
    let mut mac = <Hmac<Md5> as hmac::KeyInit>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// RFC 2865 §5.2: the User-Password, hidden with the shared secret.
fn hide_password(password: &[u8], secret: &[u8], authenticator: &[u8; 16]) -> Vec<u8> {
    let mut padded = password.to_vec();
    padded.resize(padded.len().div_ceil(16).max(1) * 16, 0);
    let mut out = Vec::with_capacity(padded.len());
    let mut prev: Vec<u8> = authenticator.to_vec();
    for block in padded.chunks(16) {
        let key = md5(&[secret, &prev]);
        let cipher: Vec<u8> = block.iter().zip(key).map(|(p, k)| p ^ k).collect();
        out.extend_from_slice(&cipher);
        prev = cipher;
    }
    out
}

fn attribute(out: &mut Vec<u8>, kind: u8, value: &[u8]) {
    out.push(kind);
    out.push((value.len() + 2) as u8);
    out.extend_from_slice(value);
}

/// An `Access-Request` (with credentials) or `Status-Server` packet.
fn build_request(secret: &[u8], credentials: Option<(&str, &str)>, id: u8, authenticator: [u8; 16]) -> Result<Vec<u8>, String> {
    let mut attrs = Vec::new();
    let code = match credentials {
        Some((user, password)) => {
            if user.len() > 253 || password.len() > 128 {
                return Err("The user name or password is too long for RADIUS".into());
            }
            attribute(&mut attrs, USER_NAME, user.as_bytes());
            attribute(&mut attrs, USER_PASSWORD, &hide_password(password.as_bytes(), secret, &authenticator));
            ACCESS_REQUEST
        }
        None => STATUS_SERVER,
    };
    attribute(&mut attrs, NAS_IDENTIFIER, b"vigilant-vole");
    attribute(&mut attrs, MESSAGE_AUTHENTICATOR, &[0; 16]);
    let length = 20 + attrs.len();
    let mut packet = vec![code, id, (length >> 8) as u8, length as u8];
    packet.extend_from_slice(&authenticator);
    packet.extend_from_slice(&attrs);
    let mac = hmac_md5(secret, &packet);
    let at = packet.len() - 16;
    packet[at..].copy_from_slice(&mac);
    Ok(packet)
}

/// What the server answered.
#[derive(Debug, PartialEq, Eq)]
struct Reply {
    code: u8,
    message: Option<String>,
}

fn parse_reply(packet: &[u8], secret: &[u8], id: u8, request_authenticator: &[u8; 16]) -> Result<Reply, String> {
    if packet.len() < 20 {
        return Err("Not a RADIUS answer".into());
    }
    let length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if packet[1] != id || length < 20 || length > packet.len() {
        return Err("Not a RADIUS answer".into());
    }
    let packet = &packet[..length];
    let expected = md5(&[&packet[..4], request_authenticator, &packet[20..], secret]);
    if packet[4..20] != expected {
        return Err("The reply failed authentication: wrong shared secret?".into());
    }
    let mut message = None;
    let mut rest = &packet[20..];
    while rest.len() >= 2 {
        let len = usize::from(rest[1]);
        if len < 2 || len > rest.len() {
            return Err("Not a RADIUS answer (bad attribute)".into());
        }
        if rest[0] == REPLY_MESSAGE {
            message = Some(String::from_utf8_lossy(&rest[2..len]).trim().to_string()).filter(|m| !m.is_empty());
        }
        rest = &rest[len..];
    }
    Ok(Reply { code: packet[0], message })
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Radius, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let Some(secret) = monitor.secret.as_deref().filter(|s| !s.is_empty()) else {
        return CheckOutcome::down("Enter the shared secret");
    };
    let credentials = monitor.basic_auth_user.as_deref().map(|u| (u, monitor.basic_auth_pass.as_deref().unwrap_or_default()));
    let addr = match util::resolve(&ep.host, ep.port).await {
        Ok(a) => a,
        Err(e) => return CheckOutcome::down(e),
    };
    let id: u8 = rand::random();
    let authenticator: [u8; 16] = rand::random();
    let packet = match build_request(secret.as_bytes(), credentials, id, authenticator) {
        Ok(p) => p,
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
    let start = Instant::now();
    let per_try = (timeout / 2).max(Duration::from_millis(500)).min(timeout);
    let mut buf = vec![0u8; 4096];
    let mut answer = None;
    for _ in 0..2 {
        if let Err(e) = socket.send(&packet).await {
            return CheckOutcome::down(format!("Send failed: {}", util::udp_recv_error(&e)));
        }
        match tokio::time::timeout(per_try, socket.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                answer = Some(parse_reply(&buf[..n], secret.as_bytes(), id, &authenticator));
                break;
            }
            Ok(Err(e)) => return CheckOutcome::down(format!("No answer: {}", util::udp_recv_error(&e))),
            Err(_) => {}
        }
    }
    let ms = util::elapsed_ms(start);
    let reply = match answer {
        None => {
            return CheckOutcome::down(format!(
                "No RADIUS answer from {}:{} within {} (server down, or this client isn't allowed?)",
                addr.ip(),
                ep.port,
                util::format_secs(timeout)
            ));
        }
        Some(Err(e)) => return CheckOutcome::down(e),
        Some(Ok(r)) => r,
    };
    let suffix = reply.message.as_deref().map(|m| format!(": {}", util::truncate(m, 80))).unwrap_or_default();
    match reply.code {
        ACCESS_ACCEPT => CheckOutcome::up(ms).with_message(format!("Access-Accept{suffix}")),
        ACCESS_CHALLENGE => CheckOutcome::up(ms).with_message(format!("Access-Challenge: the server answers{suffix}")),
        ACCESS_REJECT => CheckOutcome::down(format!("Access-Reject: the credentials were refused{suffix}")),
        other => CheckOutcome::down(format!("Unexpected RADIUS answer (code {other})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::HeartbeatStatus;
    use crate::monitor::checkers::test_support::monitor;

    const SECRET: &str = "testing123";

    /// A fake server: checks the Message-Authenticator and the hidden
    /// password (user `vole` / `pw`), answers Status-Server with Accept.
    async fn server(drop_first: bool, answer_secret: &'static str) -> std::net::SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut first = true;
            loop {
                let Ok((n, from)) = socket.recv_from(&mut buf).await else { return };
                if std::mem::take(&mut first) && drop_first {
                    continue;
                }
                let p = buf[..n].to_vec();
                let authenticator: [u8; 16] = p[4..20].try_into().unwrap();
                // Verify the Message-Authenticator with the shared secret.
                let mut zeroed = p.clone();
                let ma_at = zeroed.windows(2).position(|w| w == [MESSAGE_AUTHENTICATOR, 18]).unwrap();
                let given: Vec<u8> = zeroed[ma_at + 2..ma_at + 18].to_vec();
                zeroed[ma_at + 2..ma_at + 18].fill(0);
                if hmac_md5(SECRET.as_bytes(), &zeroed) != given[..] {
                    continue; // servers silently drop packets from unknown clients
                }
                let code = match p[0] {
                    STATUS_SERVER => ACCESS_ACCEPT,
                    _ => {
                        let mut rest = &p[20..];
                        let (mut user, mut hidden) = (Vec::new(), Vec::new());
                        while rest.len() >= 2 {
                            let len = usize::from(rest[1]);
                            match rest[0] {
                                USER_NAME => user = rest[2..len].to_vec(),
                                USER_PASSWORD => hidden = rest[2..len].to_vec(),
                                _ => {}
                            }
                            rest = &rest[len..];
                        }
                        // Decrypt (the inverse of `hide_password`).
                        let mut plain = Vec::new();
                        let mut prev = authenticator.to_vec();
                        for block in hidden.chunks(16) {
                            let key = md5(&[SECRET.as_bytes(), &prev]);
                            plain.extend(block.iter().zip(key).map(|(c, k)| c ^ k));
                            prev = block.to_vec();
                        }
                        while plain.last() == Some(&0) {
                            plain.pop();
                        }
                        if user == b"vole" && plain == b"pw" { ACCESS_ACCEPT } else { ACCESS_REJECT }
                    }
                };
                let mut attrs = Vec::new();
                if code == ACCESS_REJECT {
                    attribute(&mut attrs, REPLY_MESSAGE, b"Bad password");
                }
                let length = 20 + attrs.len();
                let head = [code, p[1], (length >> 8) as u8, length as u8];
                let auth = md5(&[&head, &authenticator, &attrs, answer_secret.as_bytes()]);
                let mut reply = head.to_vec();
                reply.extend_from_slice(&auth);
                reply.extend_from_slice(&attrs);
                socket.send_to(&reply, from).await.unwrap();
            }
        });
        addr
    }

    fn radius(target: &str, user: Option<&str>, pass: &str, secret: &str) -> Monitor {
        let mut m = monitor(MonitorType::Radius, target);
        m.timeout_seconds = 2;
        m.basic_auth_user = user.map(str::to_string);
        m.basic_auth_pass = Some(pass.into());
        m.secret = Some(secret.into());
        m
    }

    #[test]
    fn password_hiding_matches_rfc_2865_structure() {
        let auth = [7u8; 16];
        let hidden = hide_password(b"pw", b"secret", &auth);
        assert_eq!(hidden.len(), 16);
        let key = md5(&[b"secret", &auth]);
        assert_eq!(hidden[0], b'p' ^ key[0]);
        assert_eq!(hidden[1], b'w' ^ key[1]);
        assert_eq!(hidden[2], key[2]);
        assert_eq!(hide_password(&[b'x'; 17], b"s", &auth).len(), 32);
        assert_eq!(hide_password(b"", b"s", &auth).len(), 16);
    }

    #[tokio::test]
    async fn access_request_accept_and_reject() {
        let addr = server(false, SECRET).await;
        let out = check(&radius(&addr.to_string(), Some("vole"), "pw", SECRET)).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("Access-Accept"));
        let out = check(&radius(&addr.to_string(), Some("vole"), "nope", SECRET)).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert_eq!(out.message.as_deref(), Some("Access-Reject: the credentials were refused: Bad password"));
    }

    #[tokio::test]
    async fn status_server_without_a_user() {
        let addr = server(false, SECRET).await;
        let out = check(&radius(&addr.to_string(), None, "", SECRET)).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    }

    #[tokio::test]
    async fn wrong_secret_is_reported() {
        let addr = server(false, SECRET).await;
        // The server drops requests it can't verify, so the monitor sees silence...
        let out = check(&radius(&addr.to_string(), None, "", "other")).await;
        assert!(out.message.unwrap().starts_with("No RADIUS answer from 127.0.0.1"));
        // ...and a server answering with a different secret fails the reply check.
        let addr = server(false, "different").await;
        let out = check(&radius(&addr.to_string(), None, "", SECRET)).await;
        assert_eq!(out.message.as_deref(), Some("The reply failed authentication: wrong shared secret?"));
    }

    #[tokio::test]
    async fn a_lost_datagram_is_retried_and_setup_errors_are_clear() {
        let addr = server(true, SECRET).await;
        let mut m = radius(&addr.to_string(), None, "", SECRET);
        m.timeout_seconds = 3;
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
        m.secret = None;
        assert_eq!(check(&m).await.message.as_deref(), Some("Enter the shared secret"));
    }

    #[test]
    fn garbage_is_not_an_answer() {
        let auth = [0u8; 16];
        assert!(parse_reply(&[], b"s", 1, &auth).is_err());
        assert!(parse_reply(&[2, 1, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], b"s", 1, &auth).is_err());
        assert!(parse_reply(b"HTTP/1.1 400 Bad Request\r\n\r\n", b"s", 1, &auth).is_err());
    }

    /// Against a real server: `VV_TEST_RADIUS=radius://host:port,user,password,secret`.
    #[tokio::test]
    #[ignore = "needs a RADIUS server (VV_TEST_RADIUS)"]
    async fn live_server() {
        let spec = std::env::var("VV_TEST_RADIUS").expect("VV_TEST_RADIUS");
        let [target, user, pass, secret] = spec.splitn(4, ',').collect::<Vec<_>>()[..] else { panic!("target,user,password,secret") };
        let out = check(&radius(target, Some(user), pass, secret)).await;
        println!("{:?}", out.message);
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        println!("{:?}", check(&radius(target, Some(user), "wrong", secret)).await.message);
        println!("{:?}", check(&radius(target, None, "", secret)).await.message);
        println!("{:?}", check(&radius(target, Some(user), pass, "bad-secret")).await.message);
    }
}
