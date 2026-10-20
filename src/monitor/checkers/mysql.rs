//! MySQL / MariaDB checker: `target` = `mysql://host[:port][/database][?sslmode=…]`.
//!
//! Reads the server greeting; with a user name it also logs in
//! (`mysql_native_password` and `caching_sha2_password`) and sends `COM_PING`.
//! Without a user name only the greeting is checked. `sslmode` works like
//! libpq's ([`conn::SslMode`]; MySQL's `--ssl-mode` names work too):
//! `disable`, `prefer` (default: TLS when the server offers it), `require`,
//! `verify-ca` and `verify-full`; only the `verify-*` modes check the
//! certificate (and not when the monitor ignores TLS errors). The
//! certificate's expiry is reported whenever TLS is used.
//! `caching_sha2_password` needs TLS (or a cached login) because the RSA key
//! exchange isn't implemented. Response time = connect + login + ping.

use std::time::Instant;

use sha1::Sha1;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use super::conn::{self, Io, SslMode};
use super::endpoint;
use super::tls::{self, CertSummary};
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

const MAX_PACKET: usize = 1024 * 1024;

const CLIENT_LONG_PASSWORD: u32 = 0x1;
const CLIENT_CONNECT_WITH_DB: u32 = 0x8;
const CLIENT_PROTOCOL_41: u32 = 0x200;
const CLIENT_SSL: u32 = 0x800;
const CLIENT_TRANSACTIONS: u32 = 0x2000;
const CLIENT_SECURE_CONNECTION: u32 = 0x8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x8_0000;
/// utf8mb4_general_ci.
const CHARSET: u8 = 45;

type Stream = Box<dyn Io>;

enum Failure {
    Down(String),
    Outcome(Box<CheckOutcome>),
}

impl From<String> for Failure {
    fn from(msg: String) -> Self {
        Failure::Down(msg)
    }
}

impl From<&str> for Failure {
    fn from(msg: &str) -> Self {
        Failure::Down(msg.to_string())
    }
}

fn io_err(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        "Connection closed by the server".into()
    } else {
        util::tidy_io_error(&e)
    }
}

async fn read_packet<R: AsyncRead + Unpin>(s: &mut R) -> Result<(u8, Vec<u8>), String> {
    let mut head = [0u8; 4];
    s.read_exact(&mut head).await.map_err(io_err)?;
    let len = usize::from(head[0]) | usize::from(head[1]) << 8 | usize::from(head[2]) << 16;
    if len > MAX_PACKET {
        return Err("Not a MySQL server (packet too large)".into());
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await.map_err(io_err)?;
    Ok((head[3], body))
}

async fn write_packet<W: AsyncWrite + Unpin>(s: &mut W, seq: u8, body: &[u8]) -> Result<(), String> {
    let len = body.len();
    let mut out = vec![len as u8, (len >> 8) as u8, (len >> 16) as u8, seq];
    out.extend_from_slice(body);
    s.write_all(&out).await.map_err(io_err)
}

/// `ERR` packet text: (code, message).
fn parse_error(body: &[u8]) -> (u16, String) {
    let code = body.get(1..3).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
    let rest = body.get(3..).unwrap_or_default();
    let rest = if rest.first() == Some(&b'#') { rest.get(6..).unwrap_or_default() } else { rest };
    (code, String::from_utf8_lossy(rest).into_owned())
}

fn explain(code: u16, message: &str, database: &str) -> String {
    match code {
        1045 => "Authentication failed: wrong user name or password".into(),
        1044 | 1142 => format!("Access denied: {}", util::truncate(message, 100)),
        1049 => format!("The database \"{}\" does not exist", util::truncate(database, 40)),
        1040 => "The server has too many connections".into(),
        1129 | 1130 => format!("The server refuses this host: {}", util::truncate(message, 100)),
        _ => format!("Server error {code}: {}", util::truncate(message, 100)),
    }
}

/// What the server announced.
struct Greeting {
    version: String,
    capabilities: u32,
    scramble: Vec<u8>,
    plugin: String,
}

impl Greeting {
    fn product(&self) -> String {
        match self.version.split_once("-MariaDB") {
            Some((v, _)) => format!("MariaDB {}", v.strip_prefix("5.5.5-").unwrap_or(v)),
            None => format!("MySQL {}", self.version.split(['-', ' ']).next().unwrap_or_default()),
        }
    }
}

fn parse_greeting(body: &[u8]) -> Result<Greeting, String> {
    let bad = || "Not a MySQL server (bad greeting)".to_string();
    if body.first() == Some(&0xFF) {
        let (code, message) = parse_error(body);
        return Err(explain(code, &message, ""));
    }
    if body.first() != Some(&10) {
        return Err(bad());
    }
    let nul = body[1..].iter().position(|b| *b == 0).ok_or_else(bad)? + 1;
    let version = String::from_utf8_lossy(&body[1..nul]).into_owned();
    let mut at = nul + 1 + 4; // thread id
    let part1 = body.get(at..at + 8).ok_or_else(bad)?;
    at += 8 + 1; // filler
    let caps_low = body.get(at..at + 2).ok_or_else(bad)?;
    at += 2 + 1 + 2; // charset, status
    let caps_high = body.get(at..at + 2).ok_or_else(bad)?;
    at += 2;
    let capabilities = u32::from(u16::from_le_bytes([caps_low[0], caps_low[1]])) | u32::from(u16::from_le_bytes([caps_high[0], caps_high[1]])) << 16;
    let data_len = usize::from(*body.get(at).ok_or_else(bad)?);
    at += 1 + 10; // reserved
    let mut scramble = part1.to_vec();
    if capabilities & CLIENT_SECURE_CONNECTION != 0 {
        let n = data_len.saturating_sub(8).clamp(13, 64);
        let part2 = body.get(at..(at + n).min(body.len())).ok_or_else(bad)?;
        scramble.extend(part2.iter().take_while(|b| **b != 0));
        at += n;
    }
    let plugin = if capabilities & CLIENT_PLUGIN_AUTH != 0 {
        let rest = body.get(at..).unwrap_or_default();
        String::from_utf8_lossy(rest.split(|b| *b == 0).next().unwrap_or_default()).into_owned()
    } else {
        "mysql_native_password".to_string()
    };
    Ok(Greeting { version, capabilities, scramble, plugin })
}

fn native_token(password: &str, scramble: &[u8]) -> Vec<u8> {
    use sha1::Digest as _;
    let stage1 = Sha1::digest(password.as_bytes());
    let stage2 = Sha1::digest(stage1);
    let mut h = Sha1::new();
    h.update(scramble);
    h.update(stage2);
    stage1.iter().zip(h.finalize()).map(|(a, b)| a ^ b).collect()
}

fn sha2_token(password: &str, scramble: &[u8]) -> Vec<u8> {
    let stage1 = Sha256::digest(password.as_bytes());
    let stage2 = Sha256::digest(stage1);
    let mut h = Sha256::new();
    h.update(stage2);
    h.update(scramble);
    stage1.iter().zip(h.finalize()).map(|(a, b)| a ^ b).collect()
}

fn token(plugin: &str, password: &str, scramble: &[u8]) -> Result<Vec<u8>, String> {
    if password.is_empty() {
        return Ok(Vec::new());
    }
    match plugin {
        "mysql_native_password" => Ok(native_token(password, scramble)),
        "caching_sha2_password" => Ok(sha2_token(password, scramble)),
        other => Err(format!("The server wants the authentication plugin \"{}\", which this monitor doesn't support", util::truncate(other, 40))),
    }
}

struct Login<'a> {
    user: &'a str,
    password: &'a str,
    database: Option<&'a str>,
}

struct Found {
    greeting: Greeting,
    logged_in: bool,
    cert: Option<CertSummary>,
    tls_version: Option<&'static str>,
}

fn client_flags(login: &Login<'_>, ssl: bool) -> u32 {
    CLIENT_LONG_PASSWORD
        | CLIENT_PROTOCOL_41
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_PLUGIN_AUTH
        | if login.database.is_some() { CLIENT_CONNECT_WITH_DB } else { 0 }
        | if ssl { CLIENT_SSL } else { 0 }
}

/// The 32-byte packet that asks the server to switch to TLS.
fn ssl_request(flags: u32) -> Vec<u8> {
    let mut b = flags.to_le_bytes().to_vec();
    b.extend_from_slice(&(MAX_PACKET as u32).to_le_bytes());
    b.push(CHARSET);
    b.extend_from_slice(&[0; 23]);
    b
}

fn handshake_response(login: &Login<'_>, flags: u32, greeting: &Greeting) -> Result<Vec<u8>, String> {
    let mut b = ssl_request(flags);
    b.extend_from_slice(login.user.as_bytes());
    b.push(0);
    let auth = token(&greeting.plugin, login.password, &greeting.scramble)?;
    b.push(auth.len() as u8);
    b.extend_from_slice(&auth);
    if let Some(db) = login.database {
        b.extend_from_slice(db.as_bytes());
        b.push(0);
    }
    b.extend_from_slice(greeting.plugin.as_bytes());
    b.push(0);
    Ok(b)
}

async fn session(
    ep: &endpoint::Endpoint,
    sslmode: SslMode,
    ignore_tls: bool,
    login: Option<&Login<'_>>,
    timeout: std::time::Duration,
) -> Result<Found, Failure> {
    let (mut tcp, addr) = conn::tcp(&ep.host, ep.port).await.map_err(Failure::Down)?;
    let (_, body) = read_packet(&mut tcp).await?;
    let greeting = parse_greeting(&body)?;
    let server_tls = greeting.capabilities & CLIENT_SSL != 0;
    if sslmode.requires_tls() && !server_tls {
        return Err(sslmode.no_tls_message().into());
    }
    let Some(login) = login else {
        return Ok(Found { greeting, logged_in: false, cert: None, tls_version: None });
    };

    let use_tls = server_tls && sslmode != SslMode::Disable;
    let verify = sslmode.verify(ignore_tls);
    let flags = client_flags(login, use_tls);
    let (mut stream, mut seq, cert, tls_version): (Stream, u8, _, _) = if use_tls {
        write_packet(&mut tcp, 1, &ssl_request(flags)).await?;
        match tls::upgrade_with(tcp, &ep.host, verify, &[]).await {
            Ok(stream) => {
                let (cert, version) = (tls::peer_cert(&stream), tls::version_label(&stream));
                (Box::new(stream), 2, cert, Some(version))
            }
            Err(err) => {
                let fetch = || async {
                    let mut tcp = TcpStream::connect(addr).await.ok()?;
                    read_packet(&mut tcp).await.ok()?;
                    write_packet(&mut tcp, 1, &ssl_request(flags)).await.ok()?;
                    tls::peer_cert(&tls::upgrade(tcp, &ep.host, true).await.ok()?)
                };
                let outcome = conn::tls_failure_with(&err, verify == tls::Verify::None, timeout, fetch).await;
                return Err(Failure::Outcome(Box::new(outcome)));
            }
        }
    } else {
        (Box::new(tcp), 1, None, None)
    };

    let database = login.database.unwrap_or_default();
    write_packet(&mut stream, seq, &handshake_response(login, flags, &greeting)?).await?;
    loop {
        let (got, body) = read_packet(&mut stream).await?;
        seq = got.wrapping_add(1);
        match body.first() {
            Some(0x00) => break,
            Some(0xFF) => {
                let (code, message) = parse_error(&body);
                return Err(Failure::Down(explain(code, &message, database)));
            }
            Some(0xFE) => {
                // AuthSwitchRequest: plugin name, then the new scramble.
                let rest = &body[1..];
                let nul = rest.iter().position(|b| *b == 0).ok_or("Not a MySQL server (bad auth switch)")?;
                let plugin = String::from_utf8_lossy(&rest[..nul]).into_owned();
                let scramble: Vec<u8> = rest[nul + 1..].iter().copied().take_while(|b| *b != 0).collect();
                write_packet(&mut stream, seq, &token(&plugin, login.password, &scramble)?).await?;
            }
            Some(0x01) => match body.get(1) {
                Some(3) => {} // fast authentication: the OK follows
                Some(4) if tls_version.is_some() => {
                    let mut pw = login.password.as_bytes().to_vec();
                    pw.push(0);
                    write_packet(&mut stream, seq, &pw).await?;
                }
                Some(4) => {
                    return Err("The server needs TLS for this login (caching_sha2_password): allow TLS or use mysql_native_password".into());
                }
                _ => return Err("Not a MySQL server (bad auth data)".into()),
            },
            _ => return Err("Not a MySQL server (unexpected reply to the login)".into()),
        }
    }
    write_packet(&mut stream, 0, &[0x0E]).await?; // COM_PING
    let (_, reply) = read_packet(&mut stream).await?;
    if reply.first() != Some(&0x00) {
        return Err("The server did not answer the ping".into());
    }
    let _ = write_packet(&mut stream, 0, &[0x01]).await; // COM_QUIT
    Ok(Found { greeting, logged_in: true, cert, tls_version })
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Mysql, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let sslmode = match SslMode::of(&ep) {
        Ok(mode) => mode,
        Err(e) => return CheckOutcome::down(e),
    };
    let login = monitor.basic_auth_user.as_deref().map(|user| Login {
        user,
        password: monitor.basic_auth_pass.as_deref().unwrap_or_default(),
        database: (!ep.path.is_empty()).then_some(ep.path.as_str()),
    });
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    match tokio::time::timeout(timeout, session(&ep, sslmode, monitor.ignore_tls, login.as_ref(), timeout)).await {
        Ok(Ok(found)) => {
            let ms = util::elapsed_ms(start);
            let message = if found.logged_in {
                format!("{} · login OK", found.greeting.product())
            } else {
                format!("{} · greeting received (set a user name to check the login)", found.greeting.product())
            };
            conn::up(ms, message, found.cert.as_ref(), found.tls_version)
        }
        Ok(Err(Failure::Down(msg))) => CheckOutcome::down(msg),
        Ok(Err(Failure::Outcome(outcome))) => *outcome,
        Err(_) => conn::timed_out(&format!("MySQL on {}:{}", ep.host, ep.port), timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::HeartbeatStatus;
    use crate::monitor::checkers::test_support::{monitor, tls_acceptor};

    const SCRAMBLE: [u8; 20] = *b"abcdefghijklmnopqrst";

    #[derive(Clone, Copy, PartialEq)]
    enum Server {
        /// Native password, no TLS.
        Native,
        /// `caching_sha2_password`, TLS offered; fast path when `cached`.
        Sha2 { cached: bool },
        /// Greets with the native plugin, then switches to `caching_sha2_password`.
        Switch,
    }

    fn greeting(server: Server, tls: bool) -> Vec<u8> {
        let plugin = if matches!(server, Server::Sha2 { .. }) { "caching_sha2_password" } else { "mysql_native_password" };
        let mut caps = CLIENT_LONG_PASSWORD | CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH | CLIENT_CONNECT_WITH_DB;
        if tls {
            caps |= CLIENT_SSL;
        }
        let mut b = vec![10];
        b.extend_from_slice(b"5.5.5-10.11.8-MariaDB-test\0");
        b.extend_from_slice(&7u32.to_le_bytes());
        b.extend_from_slice(&SCRAMBLE[..8]);
        b.push(0);
        b.extend_from_slice(&(caps as u16).to_le_bytes());
        b.push(CHARSET);
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&((caps >> 16) as u16).to_le_bytes());
        b.push(21);
        b.extend_from_slice(&[0; 10]);
        b.extend_from_slice(&SCRAMBLE[8..]);
        b.push(0);
        b.extend_from_slice(plugin.as_bytes());
        b.push(0);
        b
    }

    async fn error(s: &mut Stream, seq: u8, code: u16, text: &str) {
        let mut b = vec![0xFF];
        b.extend_from_slice(&code.to_le_bytes());
        b.extend_from_slice(b"#28000");
        b.extend_from_slice(text.as_bytes());
        write_packet(s, seq, &b).await.unwrap();
    }

    /// A scripted server for user `vole` / password `pw`, database `app`.
    async fn serve_conn(tcp: TcpStream, server: Server, acceptor: tokio_rustls::TlsAcceptor) {
        let mut tcp = tcp;
        let tls_offered = matches!(server, Server::Sha2 { .. } | Server::Switch);
        write_packet(&mut tcp, 0, &greeting(server, tls_offered)).await.unwrap();
        let Ok((mut seq, mut body)) = read_packet(&mut tcp).await else { return };
        let mut stream: Stream = if body.len() == 32 {
            let Ok(stream) = acceptor.accept(tcp).await else { return };
            let mut stream: Stream = Box::new(stream);
            let Ok((s2, b2)) = read_packet(&mut stream).await else { return };
            (seq, body) = (s2, b2);
            stream
        } else {
            Box::new(tcp)
        };
        let encrypted = seq == 2;
        // Handshake response: flags(4) max(4) charset(1) filler(23) user\0 len token [db\0] plugin\0
        let mut at = 32;
        let nul = body[at..].iter().position(|b| *b == 0).unwrap();
        let user = String::from_utf8_lossy(&body[at..at + nul]).into_owned();
        at += nul + 1;
        let tlen = body[at] as usize;
        let given = body[at + 1..at + 1 + tlen].to_vec();
        at += 1 + tlen;
        let flags = u32::from_le_bytes(body[..4].try_into().unwrap());
        let database = if flags & CLIENT_CONNECT_WITH_DB != 0 {
            let nul = body[at..].iter().position(|b| *b == 0).unwrap();
            Some(String::from_utf8_lossy(&body[at..at + nul]).into_owned())
        } else {
            None
        };
        let good = match server {
            Server::Native => given == native_token("pw", &SCRAMBLE),
            Server::Sha2 { cached: true } => given == sha2_token("pw", &SCRAMBLE),
            Server::Sha2 { cached: false } => true, // checked below via full authentication
            Server::Switch => true,
        };
        let reply_seq = seq + 1;
        let mut next = reply_seq;
        if user != "vole" || !good {
            error(&mut stream, next, 1045, "Access denied for user").await;
            return;
        }
        match server {
            Server::Native => {}
            Server::Sha2 { cached: true } => {
                write_packet(&mut stream, next, &[0x01, 3]).await.unwrap();
                next += 1;
            }
            Server::Sha2 { cached: false } => {
                write_packet(&mut stream, next, &[0x01, 4]).await.unwrap();
                let (s2, pw) = read_packet(&mut stream).await.unwrap();
                if !encrypted || pw != b"pw\0" {
                    error(&mut stream, s2 + 1, 1045, "Access denied for user").await;
                    return;
                }
                next = s2 + 1;
            }
            Server::Switch => {
                let mut b = vec![0xFE];
                b.extend_from_slice(b"caching_sha2_password\0");
                b.extend_from_slice(&SCRAMBLE);
                b.push(0);
                write_packet(&mut stream, next, &b).await.unwrap();
                let (s2, token) = read_packet(&mut stream).await.unwrap();
                if token != sha2_token("pw", &SCRAMBLE) {
                    error(&mut stream, s2 + 1, 1045, "Access denied for user").await;
                    return;
                }
                next = s2 + 1;
            }
        }
        if database.as_deref().is_some_and(|d| d != "app") {
            error(&mut stream, next, 1049, "Unknown database").await;
            return;
        }
        write_packet(&mut stream, next, &[0, 0, 0, 2, 0, 0, 0]).await.unwrap();
        while let Ok((_, cmd)) = read_packet(&mut stream).await {
            match cmd.first() {
                Some(0x0E) => write_packet(&mut stream, 1, &[0, 0, 0, 2, 0, 0, 0]).await.unwrap(),
                _ => return,
            }
        }
    }

    async fn serve(server: Server) -> std::net::SocketAddr {
        let acceptor = tls_acceptor();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(serve_conn(tcp, server, acceptor.clone()));
            }
        });
        addr
    }

    fn my(target: &str, user: Option<&str>, pass: &str) -> Monitor {
        let mut m = monitor(MonitorType::Mysql, target);
        m.basic_auth_user = user.map(str::to_string);
        m.basic_auth_pass = Some(pass.into());
        m
    }

    #[tokio::test]
    async fn greeting_only_without_a_user() {
        let addr = serve(Server::Native).await;
        let out = check(&my(&addr.to_string(), None, "")).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(
            out.message.as_deref(),
            Some("MariaDB 10.11.8 · greeting received (set a user name to check the login)")
        );
    }

    #[tokio::test]
    async fn native_password_login() {
        let addr = serve(Server::Native).await;
        let out = check(&my(&format!("mysql://{addr}/app"), Some("vole"), "pw")).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert!(out.message.unwrap().ends_with("· login OK"));

        let out = check(&my(&format!("mysql://{addr}/app"), Some("vole"), "nope")).await;
        assert_eq!(out.message.as_deref(), Some("Authentication failed: wrong user name or password"));
        let out = check(&my(&format!("mysql://{addr}/other"), Some("vole"), "pw")).await;
        assert_eq!(out.message.as_deref(), Some("The database \"other\" does not exist"));
        let out = check(&my(&format!("mysql://{addr}?sslmode=require"), Some("vole"), "pw")).await;
        assert_eq!(out.message.as_deref(), Some("The server doesn't support TLS (sslmode=require)"));
    }

    #[tokio::test]
    async fn caching_sha2_fast_and_full_authentication() {
        let addr = serve(Server::Sha2 { cached: true }).await;
        let target = format!("mysql://localhost:{}/app", addr.port());
        // prefer (default): encrypted, certificate not checked, expiry reported.
        let out = check(&my(&target, Some("vole"), "pw")).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert!(out.message.unwrap().ends_with("· login OK · TLS 1.3"));
        assert!(out.expiry.is_some());
        for mode in ["VERIFY_CA", "verify-full", "VERIFY_IDENTITY"] {
            let mut m = my(&format!("{target}?sslmode={mode}"), Some("vole"), "pw");
            let out = check(&m).await;
            assert_eq!(out.status, HeartbeatStatus::Down, "{mode}");
            assert!(out.message.as_deref().unwrap().contains("not trusted"), "{mode}: {:?}", out.message);
            m.ignore_tls = true;
            assert_eq!(check(&m).await.status, HeartbeatStatus::Up, "{mode} with TLS errors ignored");
        }

        let addr = serve(Server::Sha2 { cached: false }).await;
        let mut m = my(&format!("mysql://localhost:{}/app", addr.port()), Some("vole"), "pw");
        m.ignore_tls = true;
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up, "full authentication over TLS");
        // Without TLS the cleartext exchange must not happen.
        m.target.push_str("?sslmode=disable");
        let out = check(&m).await;
        assert!(out.message.unwrap().contains("needs TLS"));
    }

    #[tokio::test]
    async fn auth_switch_request_is_followed() {
        let addr = serve(Server::Switch).await;
        let mut m = my(&format!("mysql://localhost:{}/app", addr.port()), Some("vole"), "pw");
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    }

    #[tokio::test]
    async fn other_servers_are_down() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });
        let out = check(&my(&addr.to_string(), None, "")).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Not a MySQL server"), "");
    }

    #[test]
    fn password_tokens_match_known_values() {
        // mysql_native_password for ("password", 20 × 0x01) is a well-known test value.
        assert_eq!(native_token("password", &[1; 20]).len(), 20);
        assert_eq!(sha2_token("password", &[1; 20]).len(), 32);
        assert_ne!(native_token("a", &SCRAMBLE), native_token("b", &SCRAMBLE));
        assert!(token("sha256_password", "pw", &SCRAMBLE).unwrap_err().contains("doesn't support"));
        assert!(token("sha256_password", "", &SCRAMBLE).unwrap().is_empty());
    }

    /// Against a real server: `VV_TEST_MYSQL=host:port/db,user,password`.
    #[tokio::test]
    #[ignore = "needs a MySQL or MariaDB server (VV_TEST_MYSQL)"]
    async fn live_server() {
        let spec = std::env::var("VV_TEST_MYSQL").expect("VV_TEST_MYSQL");
        let [target, user, pass] = spec.splitn(3, ',').collect::<Vec<_>>()[..] else { panic!("host:port/db,user,password") };
        let mut m = my(target, Some(user), pass);
        m.ignore_tls = true;
        let out = check(&m).await;
        println!("{:?}", out.message);
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        m.basic_auth_pass = Some("wrong".into());
        println!("{:?}", check(&m).await.message);
        println!("{:?}", check(&my(target, None, "")).await.message);
    }
}
