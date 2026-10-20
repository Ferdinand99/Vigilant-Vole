//! PostgreSQL checker: `target` = `postgres://host[:port][/database][?sslmode=…]`.
//!
//! Speaks the frontend/backend protocol 3.0 directly: optional TLS
//! (`SSLRequest`), startup, cleartext / MD5 / SCRAM-SHA-256 authentication,
//! then `SELECT 1`. The user name and password come from the monitor's
//! credentials. `sslmode` works like libpq's ([`conn::SslMode`]): `disable`,
//! `prefer` (default: TLS when the server offers it), `require`, `verify-ca`
//! and `verify-full`; only the `verify-*` modes check the certificate (and not
//! when the monitor ignores TLS errors). The certificate's expiry is reported
//! whenever TLS is used. The database defaults to the user name. Response
//! time = connect + login + query.

use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac as _};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::conn::{self, Io, SslMode};
use super::endpoint;
use super::tls::{self, CertSummary};
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

const MAX_MESSAGE: usize = 1024 * 1024;
/// Refuse absurd SCRAM iteration counts from a hostile server (PostgreSQL's default is 4096).
const MAX_SCRAM_ITERATIONS: u32 = 200_000;

/// `SSLRequest`: length 8, code 80877103.
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xD2, 0x16, 0x2F];

type Stream = Box<dyn Io>;

/// A backend message: its tag and body.
struct Message {
    tag: u8,
    body: Vec<u8>,
}

async fn read_message(s: &mut Stream) -> Result<Message, String> {
    let tag = s.read_u8().await.map_err(io_err)?;
    let len = s.read_i32().await.map_err(io_err)?;
    let len = usize::try_from(len).ok().filter(|l| (4..=MAX_MESSAGE).contains(l)).ok_or("Not a PostgreSQL server (bad message)")?;
    let mut body = vec![0u8; len - 4];
    s.read_exact(&mut body).await.map_err(io_err)?;
    Ok(Message { tag, body })
}

fn io_err(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        "Connection closed by the server".into()
    } else {
        util::tidy_io_error(&e)
    }
}

async fn send(s: &mut Stream, tag: Option<u8>, body: &[u8]) -> Result<(), String> {
    let mut out = Vec::with_capacity(body.len() + 5);
    if let Some(tag) = tag {
        out.push(tag);
    }
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(body);
    s.write_all(&out).await.map_err(io_err)
}

fn cstr(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

/// The fields of an `ErrorResponse`: (SQLSTATE, message).
fn error_fields(body: &[u8]) -> (String, String) {
    let (mut code, mut message) = (String::new(), String::new());
    for field in body.split(|b| *b == 0) {
        let Some((kind, text)) = field.split_first() else { continue };
        let text = String::from_utf8_lossy(text).into_owned();
        match kind {
            b'C' => code = text,
            b'M' => message = text,
            _ => {}
        }
    }
    (code, message)
}

fn explain(code: &str, message: &str, database: &str) -> String {
    match code {
        "28P01" => "Authentication failed: wrong user name or password".into(),
        "3D000" => format!("The database \"{}\" does not exist", util::truncate(database, 40)),
        "57P03" => "The server is starting up".into(),
        "53300" => "The server has too many connections".into(),
        _ => format!("Server error {code}: {}", util::truncate(message, 100)),
    }
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut first = salt.to_vec();
    first.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac(password, &first);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac(password, &u);
        for (o, b) in out.iter_mut().zip(u) {
            *o ^= b;
        }
    }
    out
}

fn md5_hex(data: &[u8]) -> String {
    hex::encode(md5::Md5::digest(data))
}

/// The client side of SCRAM-SHA-256 (RFC 5802 / 7677) after the server's first message.
#[derive(Debug)]
struct ScramFinal {
    message: String,
    server_signature: [u8; 32],
}

fn scram_final(password: &str, client_first_bare: &str, server_first: &str, client_nonce: &str) -> Result<ScramFinal, String> {
    let bad = || "The server sent a malformed SCRAM message".to_string();
    let (mut nonce, mut salt, mut iterations) = (None, None, None);
    for part in server_first.split(',') {
        match part.split_once('=') {
            Some(("r", v)) => nonce = Some(v),
            Some(("s", v)) => salt = Some(B64.decode(v).map_err(|_| bad())?),
            Some(("i", v)) => iterations = Some(v.parse::<u32>().map_err(|_| bad())?),
            _ => {}
        }
    }
    let (nonce, salt, iterations) = (nonce.ok_or_else(bad)?, salt.ok_or_else(bad)?, iterations.ok_or_else(bad)?);
    if !nonce.starts_with(client_nonce) {
        return Err("The server's SCRAM nonce is wrong".into());
    }
    if iterations == 0 || iterations > MAX_SCRAM_ITERATIONS {
        return Err("The server asked for an unreasonable SCRAM iteration count".into());
    }
    let salted = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
    let client_key = hmac(&salted, b"Client Key");
    let stored_key: [u8; 32] = Sha256::digest(client_key).into();
    let without_proof = format!("c=biws,r={nonce}");
    let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
    let signature = hmac(&stored_key, auth_message.as_bytes());
    let proof: Vec<u8> = client_key.iter().zip(signature).map(|(a, b)| a ^ b).collect();
    let server_key = hmac(&salted, b"Server Key");
    Ok(ScramFinal {
        message: format!("{without_proof},p={}", B64.encode(proof)),
        server_signature: hmac(&server_key, auth_message.as_bytes()),
    })
}

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

struct Session {
    version: Option<String>,
    cert: Option<CertSummary>,
    tls_version: Option<&'static str>,
}

async fn connect(
    ep: &endpoint::Endpoint,
    sslmode: SslMode,
    ignore_tls: bool,
    timeout: std::time::Duration,
) -> Result<(Stream, Option<CertSummary>, Option<&'static str>), Failure> {
    let (mut tcp, addr) = conn::tcp(&ep.host, ep.port).await.map_err(Failure::Down)?;
    if sslmode == SslMode::Disable {
        return Ok((Box::new(tcp), None, None));
    }
    let verify = sslmode.verify(ignore_tls);
    tcp.write_all(&SSL_REQUEST).await.map_err(|e| Failure::Down(io_err(e)))?;
    match tcp.read_u8().await.map_err(|e| Failure::Down(io_err(e)))? {
        b'S' => match tls::upgrade_with(tcp, &ep.host, verify, &[]).await {
            Ok(stream) => {
                let (cert, version) = (tls::peer_cert(&stream), tls::version_label(&stream));
                Ok((Box::new(stream), cert, Some(version)))
            }
            Err(err) => {
                let fetch = || async {
                    let mut tcp = TcpStream::connect(addr).await.ok()?;
                    tcp.write_all(&SSL_REQUEST).await.ok()?;
                    (tcp.read_u8().await.ok()? == b'S').then_some(())?;
                    tls::peer_cert(&tls::upgrade(tcp, &ep.host, true).await.ok()?)
                };
                let outcome = conn::tls_failure_with(&err, verify == tls::Verify::None, timeout, fetch).await;
                Err(Failure::Outcome(Box::new(outcome)))
            }
        },
        b'N' if sslmode.requires_tls() => Err(Failure::Down(sslmode.no_tls_message())),
        b'N' => Ok((Box::new(tcp), None, None)),
        _ => Err(Failure::Down("Not a PostgreSQL server (bad reply to the TLS request)".into())),
    }
}

async fn login(s: &mut Stream, user: &str, password: Option<&str>, database: &str) -> Result<Option<String>, Failure> {
    let mut startup = 196_608i32.to_be_bytes().to_vec();
    for (k, v) in [("user", user), ("database", database), ("application_name", "vigilant-vole"), ("client_encoding", "UTF8")] {
        startup.extend(cstr(k));
        startup.extend(cstr(v));
    }
    startup.push(0);
    send(s, None, &startup).await?;

    let need_password = || Failure::Down("The server wants a password: set one on the monitor".into());
    let mut scram: Option<(String, String, [u8; 32])> = None; // (client_first_bare, nonce, expected server signature)
    let mut version = None;
    loop {
        let msg = read_message(s).await?;
        match msg.tag {
            b'R' => {
                let kind = i32::from_be_bytes(msg.body.get(..4).and_then(|b| b.try_into().ok()).ok_or("Not a PostgreSQL server (bad auth message)")?);
                match kind {
                    0 => {}
                    3 => send(s, Some(b'p'), &cstr(password.ok_or_else(need_password)?)).await?,
                    5 => {
                        let salt = msg.body.get(4..8).ok_or("Not a PostgreSQL server (bad MD5 salt)")?;
                        let inner = md5_hex(format!("{}{user}", password.ok_or_else(need_password)?).as_bytes());
                        let mut data = inner.into_bytes();
                        data.extend_from_slice(salt);
                        send(s, Some(b'p'), &cstr(&format!("md5{}", md5_hex(&data)))).await?;
                    }
                    10 => {
                        password.ok_or_else(need_password)?;
                        let offered = String::from_utf8_lossy(&msg.body[4..]).into_owned();
                        if !offered.split('\0').any(|m| m == "SCRAM-SHA-256") {
                            return Err("The server only offers authentication methods this monitor doesn't support".to_string().into());
                        }
                        let nonce = B64.encode(rand::random::<[u8; 18]>());
                        let bare = format!("n=,r={nonce}");
                        let first = format!("n,,{bare}");
                        let mut body = cstr("SCRAM-SHA-256");
                        body.extend_from_slice(&(first.len() as i32).to_be_bytes());
                        body.extend_from_slice(first.as_bytes());
                        send(s, Some(b'p'), &body).await?;
                        scram = Some((bare, nonce, [0; 32]));
                    }
                    11 => {
                        let (bare, nonce, _) = scram.take().ok_or("Unexpected SCRAM message from the server")?;
                        let server_first = String::from_utf8_lossy(&msg.body[4..]).into_owned();
                        let password = password.ok_or_else(need_password)?.to_string();
                        let (b, f, n) = (bare.clone(), server_first.clone(), nonce.clone());
                        let fin = tokio::task::spawn_blocking(move || scram_final(&password, &b, &f, &n))
                            .await
                            .map_err(|_| "SCRAM computation failed".to_string())??;
                        send(s, Some(b'p'), fin.message.as_bytes()).await?;
                        scram = Some((bare, nonce, fin.server_signature));
                    }
                    12 => {
                        let (_, _, expected) = scram.take().ok_or("Unexpected SCRAM message from the server")?;
                        let text = String::from_utf8_lossy(&msg.body[4..]).into_owned();
                        let ok = text.strip_prefix("v=").and_then(|v| B64.decode(v).ok()).is_some_and(|v| v == expected);
                        if !ok {
                            return Err("The server failed to prove it knows the password".to_string().into());
                        }
                    }
                    other => return Err(format!("Unsupported authentication method (code {other})").into()),
                }
            }
            b'E' => {
                let (code, message) = error_fields(&msg.body);
                return Err(Failure::Down(explain(&code, &message, database)));
            }
            b'S' => {
                let mut parts = msg.body.split(|b| *b == 0);
                if parts.next() == Some(b"server_version") {
                    version = parts.next().map(|v| String::from_utf8_lossy(v).into_owned());
                }
            }
            b'Z' => return Ok(version),
            b'K' | b'N' => {}
            _ => return Err("Not a PostgreSQL server (unexpected message)".to_string().into()),
        }
    }
}

async fn select_one(s: &mut Stream, database: &str) -> Result<(), Failure> {
    send(s, Some(b'Q'), &cstr("SELECT 1")).await?;
    loop {
        let msg = read_message(s).await?;
        match msg.tag {
            b'E' => {
                let (code, message) = error_fields(&msg.body);
                return Err(Failure::Down(explain(&code, &message, database)));
            }
            b'Z' => return Ok(()),
            _ => {}
        }
    }
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Postgres, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let Some(user) = monitor.basic_auth_user.as_deref() else {
        return CheckOutcome::down("Enter a user name");
    };
    let sslmode = match SslMode::of(&ep) {
        Ok(mode) => mode,
        Err(e) => return CheckOutcome::down(e),
    };
    let database = if ep.path.is_empty() { user } else { ep.path.as_str() };
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    let probe = async {
        let (mut stream, cert, tls_version) = connect(&ep, sslmode, monitor.ignore_tls, timeout).await?;
        let version = login(&mut stream, user, monitor.basic_auth_pass.as_deref(), database).await?;
        select_one(&mut stream, database).await?;
        let ms = util::elapsed_ms(start);
        let _ = send(&mut stream, Some(b'X'), &[]).await;
        Ok::<_, Failure>((ms, Session { version, cert, tls_version }))
    };
    match tokio::time::timeout(timeout, probe).await {
        Ok(Ok((ms, session))) => {
            let message = match &session.version {
                Some(v) => format!("PostgreSQL {v} · SELECT 1 OK"),
                None => "SELECT 1 OK".to_string(),
            };
            conn::up(ms, message, session.cert.as_ref(), session.tls_version)
        }
        Ok(Err(Failure::Down(msg))) => CheckOutcome::down(msg),
        Ok(Err(Failure::Outcome(outcome))) => *outcome,
        Err(_) => conn::timed_out(&format!("PostgreSQL on {}:{}", ep.host, ep.port), timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::HeartbeatStatus;
    use crate::monitor::checkers::test_support::{monitor, tls_acceptor};

    #[derive(Clone, Copy, PartialEq)]
    enum Auth {
        Trust,
        Cleartext,
        Md5,
        Scram,
    }

    const SALT: [u8; 4] = [1, 2, 3, 4];

    async fn msg(s: &mut Stream) -> (u8, Vec<u8>) {
        let m = read_message(s).await.unwrap();
        (m.tag, m.body)
    }

    async fn auth_request(s: &mut Stream, kind: i32, extra: &[u8]) {
        let mut body = kind.to_be_bytes().to_vec();
        body.extend_from_slice(extra);
        send(s, Some(b'R'), &body).await.unwrap();
    }

    async fn reject(s: &mut Stream, code: &str, text: &str) {
        let mut body = b"SFATAL\0C".to_vec();
        body.extend(cstr(code));
        body.push(b'M');
        body.extend(cstr(text));
        body.push(0);
        send(s, Some(b'E'), &body).await.unwrap();
    }

    /// A scripted PostgreSQL backend for user `vole` / password `pw`, database `app`.
    async fn session(mut s: Stream, auth: Auth) {
        // Startup (the optional TLS request was answered before).
        let Ok(len) = s.read_i32().await else { return };
        let mut body = vec![0u8; len as usize - 4];
        if s.read_exact(&mut body).await.is_err() {
            return;
        }
        let text = String::from_utf8_lossy(&body[4..]).into_owned();
        let param = |name: &str| {
            let mut parts = text.split('\0');
            while let Some(k) = parts.next() {
                if k == name {
                    return parts.next().unwrap_or_default().to_string();
                }
                parts.next();
            }
            String::new()
        };
        let (user, database) = (param("user"), param("database"));
        let ok = match auth {
            Auth::Trust => true,
            Auth::Cleartext => {
                auth_request(&mut s, 3, &[]).await;
                let (_, b) = msg(&mut s).await;
                b == cstr("pw")
            }
            Auth::Md5 => {
                auth_request(&mut s, 5, &SALT).await;
                let (_, b) = msg(&mut s).await;
                let mut data = md5_hex(format!("pw{user}").as_bytes()).into_bytes();
                data.extend_from_slice(&SALT);
                b == cstr(&format!("md5{}", md5_hex(&data)))
            }
            Auth::Scram => {
                auth_request(&mut s, 10, b"SCRAM-SHA-256\0\0").await;
                let (_, b) = msg(&mut s).await;
                let at = b.iter().position(|c| *c == 0).unwrap() + 1;
                let first = String::from_utf8_lossy(&b[at + 4..]).into_owned();
                let bare = first.strip_prefix("n,,").unwrap().to_string();
                let client_nonce = bare.split("r=").nth(1).unwrap().to_string();
                let nonce = format!("{client_nonce}srv");
                let server_first = format!("r={nonce},s={},i=4096", B64.encode(b"saltsalt"));
                auth_request(&mut s, 11, server_first.as_bytes()).await;
                let (_, b) = msg(&mut s).await;
                let fin = String::from_utf8_lossy(&b).into_owned();
                let proof = B64.decode(fin.rsplit(",p=").next().unwrap()).unwrap();
                let salted = pbkdf2_sha256(b"pw", b"saltsalt", 4096);
                let client_key = hmac(&salted, b"Client Key");
                let stored: [u8; 32] = Sha256::digest(client_key).into();
                let without_proof = fin.rsplit_once(",p=").unwrap().0.to_string();
                let auth_message = format!("{bare},{server_first},{without_proof}");
                let signature = hmac(&stored, auth_message.as_bytes());
                let recovered: Vec<u8> = proof.iter().zip(signature).map(|(a, b)| a ^ b).collect();
                let good = recovered == client_key;
                if good {
                    let server_key = hmac(&salted, b"Server Key");
                    let v = format!("v={}", B64.encode(hmac(&server_key, auth_message.as_bytes())));
                    auth_request(&mut s, 12, v.as_bytes()).await;
                }
                good
            }
        };
        if !ok {
            reject(&mut s, "28P01", "password authentication failed for user").await;
            return;
        }
        if database != "app" {
            reject(&mut s, "3D000", "database does not exist").await;
            return;
        }
        auth_request(&mut s, 0, &[]).await;
        let mut body = cstr("server_version");
        body.extend(cstr("16.2"));
        send(&mut s, Some(b'S'), &body).await.unwrap();
        send(&mut s, Some(b'Z'), b"I").await.unwrap();
        loop {
            let m = match read_message(&mut s).await {
                Ok(m) => m,
                Err(_) => return,
            };
            match m.tag {
                b'Q' => {
                    send(&mut s, Some(b'C'), &cstr("SELECT 1")).await.unwrap();
                    send(&mut s, Some(b'Z'), b"I").await.unwrap();
                }
                _ => return,
            }
        }
    }

    async fn serve(auth: Auth, tls: bool) -> std::net::SocketAddr {
        let acceptor = tls_acceptor();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // The TLS request comes first unless sslmode=disable.
                    let mut peek = [0u8; 8];
                    tcp.peek(&mut peek).await.unwrap();
                    let stream: Stream = if peek[4..] == [0x04, 0xD2, 0x16, 0x2F] {
                        tcp.read_exact(&mut peek).await.unwrap();
                        if tls {
                            tcp.write_all(b"S").await.unwrap();
                            let Ok(stream) = acceptor.accept(tcp).await else { return };
                            Box::new(stream)
                        } else {
                            tcp.write_all(b"N").await.unwrap();
                            Box::new(tcp)
                        }
                    } else if tls {
                        // hostssl only: plain connections are refused.
                        let mut stream: Stream = Box::new(tcp);
                        reject(&mut stream, "28000", "no pg_hba.conf entry (no encryption)").await;
                        return;
                    } else {
                        Box::new(tcp)
                    };
                    session(stream, auth).await;
                });
            }
        });
        addr
    }

    fn pg(target: &str, pass: Option<&str>) -> Monitor {
        let mut m = monitor(MonitorType::Postgres, target);
        m.basic_auth_user = Some("vole".into());
        m.basic_auth_pass = pass.map(str::to_string);
        m
    }

    #[tokio::test]
    async fn every_authentication_method_logs_in() {
        for auth in [Auth::Trust, Auth::Cleartext, Auth::Md5, Auth::Scram] {
            let addr = serve(auth, false).await;
            let out = check(&pg(&format!("postgres://{addr}/app"), Some("pw"))).await;
            assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
            assert_eq!(out.message.as_deref(), Some("PostgreSQL 16.2 · SELECT 1 OK"));
        }
    }

    #[tokio::test]
    async fn wrong_password_and_missing_database_are_explained() {
        for auth in [Auth::Cleartext, Auth::Md5, Auth::Scram] {
            let addr = serve(auth, false).await;
            let out = check(&pg(&format!("postgres://{addr}/app"), Some("nope"))).await;
            assert_eq!(out.status, HeartbeatStatus::Down);
            assert_eq!(out.message.as_deref(), Some("Authentication failed: wrong user name or password"));
        }
        let addr = serve(Auth::Md5, false).await;
        let out = check(&pg(&format!("postgres://{addr}/other"), Some("pw"))).await;
        assert_eq!(out.message.as_deref(), Some("The database \"other\" does not exist"));
        let out = check(&pg(&format!("postgres://{addr}/app"), None)).await;
        assert_eq!(out.message.as_deref(), Some("The server wants a password: set one on the monitor"));
    }

    #[tokio::test]
    async fn tls_is_used_when_offered_and_verified_only_when_asked() {
        let addr = serve(Auth::Scram, true).await;
        let target = format!("postgres://localhost:{}/app", addr.port());
        // prefer (default) and require encrypt without checking the certificate, like libpq.
        for t in [target.clone(), format!("{target}?sslmode=require")] {
            let out = check(&pg(&t, Some("pw"))).await;
            assert_eq!(out.status, HeartbeatStatus::Up, "{t}: {:?}", out.message);
            assert!(out.message.unwrap().ends_with("· TLS 1.3"));
            assert!(out.expiry.is_some(), "the certificate's expiry is still reported");
        }
        for mode in ["verify-ca", "verify-full"] {
            let out = check(&pg(&format!("{target}?sslmode={mode}"), Some("pw"))).await;
            assert_eq!(out.status, HeartbeatStatus::Down, "{mode}");
            assert!(out.message.as_deref().unwrap().contains("not trusted"), "{mode}: {:?}", out.message);
            assert!(out.expiry.is_some());
        }

        let mut m = pg(&format!("{target}?sslmode=verify-full"), Some("pw"));
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);

        let out = check(&pg(&format!("{target}?sslmode=disable"), Some("pw"))).await;
        assert_eq!(out.status, HeartbeatStatus::Down, "the server speaks TLS only");
    }

    #[tokio::test]
    async fn sslmode_require_refuses_a_plain_server() {
        let addr = serve(Auth::Md5, false).await;
        let out = check(&pg(&format!("postgres://{addr}/app?sslmode=require"), Some("pw"))).await;
        assert_eq!(out.message.as_deref(), Some("The server doesn't support TLS (sslmode=require)"));
        let out = check(&pg(&format!("postgres://{addr}/app?sslmode=verify-full"), Some("pw"))).await;
        assert_eq!(out.message.as_deref(), Some("The server doesn't support TLS (sslmode=verify-full)"));
        let out = check(&pg(&format!("postgres://{addr}/app?sslmode=allow"), Some("pw"))).await;
        assert!(out.message.unwrap().starts_with("Unsupported sslmode"));
    }

    #[tokio::test]
    async fn the_database_defaults_to_the_user() {
        let addr = serve(Auth::Trust, false).await;
        let mut m = pg(&addr.to_string(), None);
        m.basic_auth_user = Some("app".into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
        m.basic_auth_user = None;
        assert_eq!(check(&m).await.message.as_deref(), Some("Enter a user name"));
    }

    #[tokio::test]
    async fn other_servers_are_down() {
        let addr = crate::monitor::checkers::test_support::foreign_server(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        let out = check(&pg(&format!("{addr}?sslmode=disable"), Some("pw"))).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Not a PostgreSQL server"));
    }

    /// Against a real server: `VV_TEST_POSTGRES=host:port/db,user,password`.
    #[tokio::test]
    #[ignore = "needs a PostgreSQL server (VV_TEST_POSTGRES)"]
    async fn live_server() {
        let spec = std::env::var("VV_TEST_POSTGRES").expect("VV_TEST_POSTGRES");
        let [target, user, pass] = spec.splitn(3, ',').collect::<Vec<_>>()[..] else { panic!("host:port/db,user,password") };
        let mut m = pg(target, Some(pass));
        m.basic_auth_user = Some(user.into());
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        println!("{:?} expiry={:?}", out.message, out.expiry.is_some());
        let mut plain = pg(&format!("{target}?sslmode=disable"), Some(pass));
        plain.basic_auth_user = Some(user.into());
        let out = check(&plain).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        println!("{:?}", out.message);
        m.basic_auth_pass = Some("wrong".into());
        println!("{:?}", check(&m).await.message);
    }

    #[test]
    fn scram_matches_the_rfc_7677_vector() {
        // RFC 7677 §3 (user "user", password "pencil").
        let fin = scram_final(
            "pencil",
            "n=user,r=rOprNGfwEbeRWgbNEkqO",
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
            "rOprNGfwEbeRWgbNEkqO",
        )
        .unwrap();
        assert_eq!(
            fin.message,
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        assert_eq!(B64.encode(fin.server_signature), "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=");
    }

    #[test]
    fn hostile_scram_parameters_are_refused() {
        let err = scram_final("pw", "n=,r=abc", "r=abcdef,s=c2FsdA==,i=999999999", "abc").unwrap_err();
        assert!(err.contains("iteration"));
        let err = scram_final("pw", "n=,r=abc", "r=zzz,s=c2FsdA==,i=4096", "abc").unwrap_err();
        assert!(err.contains("nonce"));
    }
}
