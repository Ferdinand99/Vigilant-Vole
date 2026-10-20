//! SMTP checker: `target` = `host[:port]` (port defaults to 25).
//!
//! Reads the `220` greeting and says `EHLO`. Port 465 speaks TLS from the
//! first byte (implicit TLS); on any other port a server that offers
//! `STARTTLS` is upgraded, so the certificate gets checked (and reported as
//! expiry info) too. A server without `STARTTLS` is still up. No mail is
//! sent and nothing is authenticated. Response time = connect + greeting +
//! `EHLO` (+ TLS).

use std::net::SocketAddr;
use std::time::Instant;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use super::conn;
use super::tls::{self, CertSummary};
use super::util;
use crate::db::models::{CheckOutcome, Monitor};

/// Longest reply line / most reply lines read from a server.
const MAX_LINE: usize = 1024;
const MAX_LINES: usize = 64;

/// One SMTP reply: the code and its (possibly multi-line) text.
#[derive(Debug, PartialEq, Eq)]
struct Reply {
    code: u16,
    lines: Vec<String>,
}

impl Reply {
    fn first(&self) -> &str {
        self.lines.first().map(String::as_str).unwrap_or_default()
    }

    fn offers(&self, keyword: &str) -> bool {
        self.lines.iter().any(|l| l.trim().eq_ignore_ascii_case(keyword))
    }
}

/// Read one (multi-line) reply: `250-more` lines until a `250 last` line.
async fn read_reply<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Reply, String> {
    let mut lines = Vec::new();
    for _ in 0..MAX_LINES {
        let mut raw = Vec::new();
        let n = reader
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut raw)
            .await
            .map_err(|e| util::tidy_io_error(&e))?;
        if n == 0 {
            return Err("Connection closed by the server".into());
        }
        let line = String::from_utf8_lossy(&raw).trim_end().to_string();
        let (code, rest) = line.split_at_checked(3).ok_or_else(|| garbled(&line))?;
        let code: u16 = code.parse().map_err(|_| garbled(&line))?;
        let (more, text) = match rest.chars().next() {
            Some('-') => (true, &rest[1..]),
            Some(' ') => (false, &rest[1..]),
            None => (false, ""),
            Some(_) => return Err(garbled(&line)),
        };
        lines.push(text.to_string());
        if !more {
            return Ok(Reply { code, lines });
        }
    }
    Err("Reply from the server is too long".into())
}

fn garbled(line: &str) -> String {
    format!("Not an SMTP server (got \"{}\")", util::truncate(line, 60))
}

async fn command<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut BufReader<S>, line: &str) -> Result<Reply, String> {
    stream
        .get_mut()
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .map_err(|e| util::tidy_io_error(&e))?;
    read_reply(stream).await
}

/// Greeting + EHLO on an open stream. Returns the greeting text and the EHLO reply.
async fn converse<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut BufReader<S>) -> Result<(String, Reply), String> {
    let greeting = read_reply(stream).await?;
    if greeting.code != 220 {
        return Err(format!("Greeting refused: {} {}", greeting.code, util::truncate(greeting.first(), 80)));
    }
    let ehlo = ehlo(stream).await?;
    Ok((greeting.first().to_string(), ehlo))
}

async fn ehlo<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut BufReader<S>) -> Result<Reply, String> {
    let ehlo = command(stream, "EHLO vigilant-vole").await?;
    if ehlo.code != 250 {
        return Err(format!("EHLO refused: {} {}", ehlo.code, util::truncate(ehlo.first(), 80)));
    }
    Ok(ehlo)
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let (host, port) = match util::split_host_port(&monitor.target, Some(25)) {
        Ok(hp) => hp,
        Err(e) => return CheckOutcome::down(e),
    };
    let addrs = match util::resolve_all(&host, port).await {
        Ok(addrs) => addrs,
        Err(e) => return CheckOutcome::down(e),
    };
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    match tokio::time::timeout(timeout, probe(&host, &addrs, port == 465, monitor.ignore_tls)).await {
        Ok(Ok(found)) => {
            let ms = util::elapsed_ms(start);
            let mut outcome = CheckOutcome::up(ms).with_message(found.message());
            outcome.expiry = found.cert.as_ref().and_then(CertSummary::expiry_info);
            outcome
        }
        Ok(Err(Failure::Tls(err, addr))) => {
            let implicit = port == 465;
            conn::tls_failure_with(&err, monitor.ignore_tls, timeout, || fetch_cert(&host, addr, implicit)).await
        }
        Ok(Err(Failure::Smtp(msg))) => CheckOutcome::down(msg),
        Err(_) => CheckOutcome::down(format!(
            "SMTP on {}:{port}: no answer within {}",
            addrs[addrs.len() - 1].ip(),
            util::format_secs(timeout)
        )),
    }
}

/// The certificate the server presents, fetched without verification (the
/// STARTTLS dance is replayed on a fresh connection).
async fn fetch_cert(host: &str, addr: SocketAddr, implicit_tls: bool) -> Option<CertSummary> {
    let tcp = TcpStream::connect(addr).await.ok()?;
    if implicit_tls {
        return tls::peer_cert(&tls::upgrade(tcp, host, true).await.ok()?);
    }
    let mut reader = BufReader::new(tcp);
    let (_, ehlo) = converse(&mut reader).await.ok()?;
    if !ehlo.offers("STARTTLS") || command(&mut reader, "STARTTLS").await.ok()?.code != 220 || !reader.buffer().is_empty() {
        return None;
    }
    tls::peer_cert(&tls::upgrade(reader.into_inner(), host, true).await.ok()?)
}

enum Failure {
    Smtp(String),
    /// The handshake failed with the server at this address.
    Tls(std::io::Error, SocketAddr),
}

impl From<String> for Failure {
    fn from(msg: String) -> Self {
        Failure::Smtp(msg)
    }
}

struct Found {
    greeting: String,
    tls: Option<&'static str>,
    starttls_offered: bool,
    cert: Option<CertSummary>,
}

impl Found {
    fn message(&self) -> String {
        let tls = match (self.tls, self.starttls_offered) {
            (Some(v), _) => format!(" · {v}"),
            (None, true) => " · STARTTLS offered".to_string(),
            (None, false) => " · no TLS offered".to_string(),
        };
        format!("{}{tls}", util::truncate(&self.greeting, 80))
    }
}

async fn probe(host: &str, addrs: &[SocketAddr], implicit_tls: bool, ignore_tls: bool) -> Result<Found, Failure> {
    let (tcp, addr) = util::connect_any(addrs)
        .await
        .map_err(|(_, e)| Failure::Smtp(format!("Connection failed: {}", util::tidy_io_error(&e))))?;
    let tls_err = |e| Failure::Tls(e, addr);
    if implicit_tls {
        let stream = tls::upgrade(tcp, host, ignore_tls).await.map_err(tls_err)?;
        let (version, cert) = (tls::version_label(&stream), tls::peer_cert(&stream));
        let mut reader = BufReader::new(stream);
        let (greeting, ehlo) = converse(&mut reader).await?;
        let _ = command(&mut reader, "QUIT").await;
        return Ok(Found { greeting, tls: Some(version), starttls_offered: ehlo.offers("STARTTLS"), cert });
    }
    let mut reader = BufReader::new(tcp);
    let (greeting, ehlo) = converse(&mut reader).await?;
    if !ehlo.offers("STARTTLS") {
        let _ = command(&mut reader, "QUIT").await;
        return Ok(Found { greeting, tls: None, starttls_offered: false, cert: None });
    }
    let reply = command(&mut reader, "STARTTLS").await?;
    if reply.code != 220 {
        return Err(format!("STARTTLS refused: {} {}", reply.code, util::truncate(reply.first(), 80)).into());
    }
    // Bytes the server sent after its 220 would be read as part of the TLS
    // session (STARTTLS injection); there must be none.
    if !reader.buffer().is_empty() {
        return Err("Server sent data before the TLS handshake".to_string().into());
    }
    let stream = tls::upgrade(reader.into_inner(), host, ignore_tls).await.map_err(tls_err)?;
    let (version, cert) = (tls::version_label(&stream), tls::peer_cert(&stream));
    let mut reader = BufReader::new(stream);
    // The server forgets the session state after STARTTLS: say EHLO again.
    ehlo_again(&mut reader).await?;
    let _ = command(&mut reader, "QUIT").await;
    Ok(Found { greeting, tls: Some(version), starttls_offered: true, cert })
}

async fn ehlo_again<S: AsyncRead + AsyncWrite + Unpin>(reader: &mut BufReader<S>) -> Result<(), String> {
    ehlo(reader).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{HeartbeatStatus, MonitorType};
    use crate::monitor::checkers::test_support::{monitor, tls_acceptor};

    /// A scripted SMTP server. `starttls` advertises and performs STARTTLS.
    async fn serve(greeting: &'static str, starttls: bool) -> std::net::SocketAddr {
        let acceptor = tls_acceptor();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let mut r = BufReader::new(tcp);
                    r.get_mut().write_all(greeting.as_bytes()).await.unwrap();
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let cmd = line.trim().to_ascii_uppercase();
                        let reply: &[u8] = if cmd.starts_with("EHLO") && starttls {
                            b"250-mail.test\r\n250-SIZE 100\r\n250 STARTTLS\r\n"
                        } else if cmd.starts_with("EHLO") {
                            b"250-mail.test\r\n250 SIZE 100\r\n"
                        } else if cmd == "STARTTLS" {
                            r.get_mut().write_all(b"220 Ready\r\n").await.unwrap();
                            let tls = acceptor.accept(r.into_inner()).await.unwrap();
                            let mut r = BufReader::new(tls);
                            loop {
                                let mut line = String::new();
                                if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                                    return;
                                }
                                let reply: &[u8] = if line.trim().eq_ignore_ascii_case("QUIT") {
                                    b"221 Bye\r\n"
                                } else {
                                    b"250-mail.test\r\n250 SIZE 100\r\n"
                                };
                                r.get_mut().write_all(reply).await.unwrap();
                            }
                        } else {
                            b"221 Bye\r\n"
                        };
                        r.get_mut().write_all(reply).await.unwrap();
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn plain_server_without_starttls_is_up() {
        let addr = serve("220 mail.test ESMTP ready\r\n", false).await;
        let out = check(&monitor(MonitorType::Smtp, &addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("mail.test ESMTP ready · no TLS offered"));
        assert!(out.expiry.is_none());
    }

    #[tokio::test]
    async fn multiline_greeting_is_read_to_the_end() {
        let addr = serve("220-welcome\r\n220 mail.test ready\r\n", false).await;
        let out = check(&monitor(MonitorType::Smtp, &addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    }

    #[tokio::test]
    async fn starttls_is_upgraded_and_the_certificate_reported() {
        let addr = serve("220 mail.test ESMTP\r\n", true).await;
        let target = format!("localhost:{}", addr.port());
        let mut m = monitor(MonitorType::Smtp, &target);
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("mail.test ESMTP · TLS 1.3"));
        assert!(out.expiry.is_some());

        let out = check(&monitor(MonitorType::Smtp, &target)).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().contains("not trusted"));
    }

    #[tokio::test]
    async fn refused_greeting_and_non_smtp_servers_are_down() {
        let addr = serve("554 No SMTP service here\r\n", false).await;
        let out = check(&monitor(MonitorType::Smtp, &addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert_eq!(out.message.as_deref(), Some("Greeting refused: 554 No SMTP service here"));

        let addr = serve("SSH-2.0-OpenSSH_9.6\r\n", false).await;
        let out = check(&monitor(MonitorType::Smtp, &addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Not an SMTP server"));
    }

    #[tokio::test]
    async fn closed_port_is_down() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let out = check(&monitor(MonitorType::Smtp, &addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Connection failed"));
    }
}
