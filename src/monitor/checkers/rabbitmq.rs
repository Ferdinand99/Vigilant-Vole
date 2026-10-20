//! RabbitMQ / AMQP 0-9-1 checker: `target` = `[amqp|amqps]://host[:port][/vhost]`.
//!
//! Sends the protocol header and reads `Connection.Start` (which names the
//! broker and its version). With a user name it also logs in (`PLAIN`) and
//! opens the virtual host (default `/`; `%2F` is decoded), then closes
//! politely. Without a user name only the greeting is checked. Response time
//! = connect + login.

use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::conn::{self, Io};
use super::endpoint;
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

const MAX_FRAME: usize = 1024 * 1024;
const FRAME_END: u8 = 0xCE;
const HEADER: &[u8; 8] = b"AMQP\x00\x00\x09\x01";

type Stream = Box<dyn Io>;

struct Frame {
    kind: u8,
    payload: Vec<u8>,
}

impl Frame {
    /// `(class, method, arguments)` of a method frame.
    fn method(&self) -> Option<(u16, u16, &[u8])> {
        if self.kind != 1 || self.payload.len() < 4 {
            return None;
        }
        let p = &self.payload;
        Some((u16::from_be_bytes([p[0], p[1]]), u16::from_be_bytes([p[2], p[3]]), &p[4..]))
    }
}

fn io_err(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        "Connection closed by the broker".into()
    } else {
        util::tidy_io_error(&e)
    }
}

async fn read_frame(s: &mut Stream) -> Result<Frame, String> {
    let mut head = [0u8; 7];
    s.read_exact(&mut head).await.map_err(io_err)?;
    if &head[..4] == b"AMQP" {
        return Err("The broker doesn't speak AMQP 0-9-1".into());
    }
    let size = u32::from_be_bytes([head[3], head[4], head[5], head[6]]) as usize;
    if size > MAX_FRAME {
        return Err("Not an AMQP broker (frame too large)".into());
    }
    let mut payload = vec![0u8; size + 1];
    s.read_exact(&mut payload).await.map_err(io_err)?;
    if payload.pop() != Some(FRAME_END) {
        return Err("Not an AMQP broker (bad frame)".into());
    }
    Ok(Frame { kind: head[0], payload })
}

fn method_frame(class: u16, method: u16, args: &[u8]) -> Vec<u8> {
    let mut payload = class.to_be_bytes().to_vec();
    payload.extend_from_slice(&method.to_be_bytes());
    payload.extend_from_slice(args);
    let mut out = vec![1, 0, 0];
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out.push(FRAME_END);
    out
}

fn short_str(out: &mut Vec<u8>, s: &str) {
    out.push(s.len().min(255) as u8);
    out.extend_from_slice(&s.as_bytes()[..s.len().min(255)]);
}

fn long_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// A field table entry value we may want.
fn skip_value(b: &[u8], at: &mut usize) -> Option<()> {
    let kind = *b.get(*at)?;
    *at += 1;
    let n = match kind {
        b't' | b'b' | b'B' => 1,
        b's' | b'u' => 2,
        b'I' | b'i' | b'f' => 4,
        b'l' | b'L' | b'd' | b'T' => 8,
        b'D' => 5,
        b'V' => 0,
        b'S' | b'x' | b'F' | b'A' => {
            let len = u32::from_be_bytes(b.get(*at..*at + 4)?.try_into().ok()?) as usize;
            *at += 4;
            len
        }
        _ => return None,
    };
    *at = at.checked_add(n).filter(|end| *end <= b.len())?;
    Some(())
}

/// `product` and `version` from the server-properties table of `Connection.Start`.
fn server_properties(args: &[u8]) -> (Option<String>, Option<String>) {
    let mut at = 2; // version major / minor
    let Some(len) = args.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize) else {
        return (None, None);
    };
    at += 4;
    let end = (at + len).min(args.len());
    let (mut product, mut version) = (None, None);
    while at < end {
        let Some(&name_len) = args.get(at) else { break };
        let name_end = at + 1 + usize::from(name_len);
        let Some(name) = args.get(at + 1..name_end) else { break };
        at = name_end;
        if args.get(at) == Some(&b'S')
            && let Some(len) = args.get(at + 1..at + 5).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
            && let Some(text) = args.get(at + 5..at + 5 + len)
        {
            match name {
                b"product" => product = Some(String::from_utf8_lossy(text).into_owned()),
                b"version" => version = Some(String::from_utf8_lossy(text).into_owned()),
                _ => {}
            }
        }
        if skip_value(args, &mut at).is_none() {
            break;
        }
    }
    (product, version)
}

/// The text of a `Connection.Close`: `(code, text)`.
fn close_reason(args: &[u8]) -> (u16, String) {
    let code = args.get(..2).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]));
    let text = args.get(2).and_then(|n| args.get(3..3 + usize::from(*n))).map(|t| String::from_utf8_lossy(t).into_owned()).unwrap_or_default();
    (code, text)
}

fn explain(code: u16, text: &str) -> String {
    match code {
        403 => "Authentication failed: wrong user name or password".into(),
        530 => format!("Not allowed: {}", util::truncate(text, 100)),
        320 => "The broker is shutting down".into(),
        _ => format!("The broker closed the connection ({code}): {}", util::truncate(text, 100)),
    }
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Rabbitmq, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let vhost = if ep.path.is_empty() { "/" } else { ep.path.as_str() };
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    let probe = async {
        let connected = conn::connect(&ep, monitor.ignore_tls, timeout).await?;
        let mut s: Stream = connected.io;
        let fail = CheckOutcome::down;
        s.write_all(HEADER).await.map_err(|e| fail(io_err(e)))?;
        let start_frame = read_frame(&mut s).await.map_err(fail)?;
        let Some((10, 10, args)) = start_frame.method() else {
            return Err(fail("Not an AMQP broker (no Connection.Start)".into()));
        };
        let (product, version) = server_properties(args);
        let name = match (product, version) {
            (Some(p), Some(v)) => format!("{p} {v}"),
            (Some(p), None) => p,
            _ => "AMQP broker".into(),
        };
        let Some(user) = monitor.basic_auth_user.as_deref() else {
            let ms = util::elapsed_ms(start);
            let message = format!("{name} · greeting received (set a user name to check the login)");
            return Ok(conn::up(ms, message, connected.cert.as_ref(), connected.tls_version));
        };

        // Start-Ok: client properties, mechanism, response, locale.
        let mut ok = Vec::new();
        long_str(&mut ok, &[]);
        short_str(&mut ok, "PLAIN");
        let mut plain = vec![0];
        plain.extend_from_slice(user.as_bytes());
        plain.push(0);
        plain.extend_from_slice(monitor.basic_auth_pass.as_deref().unwrap_or_default().as_bytes());
        long_str(&mut ok, &plain);
        short_str(&mut ok, "en_US");
        s.write_all(&method_frame(10, 11, &ok)).await.map_err(|e| fail(io_err(e)))?;

        let tune = loop {
            // RabbitMQ drops the connection without a word when the login is refused.
            let f = read_frame(&mut s)
                .await
                .map_err(|_| fail("Login refused: the broker closed the connection (wrong user name or password?)".into()))?;
            match f.method() {
                Some((10, 30, args)) => break args.to_vec(),
                Some((10, 50, args)) => {
                    let (code, text) = close_reason(args);
                    return Err(fail(explain(code, &text)));
                }
                _ if f.kind == 8 => {}
                _ => return Err(fail("Not an AMQP broker (unexpected frame during login)".into())),
            }
        };
        // Tune-Ok: accept the broker's channel and frame limits, no heartbeat.
        let mut tune_ok = tune.get(..6).unwrap_or(&[0; 6]).to_vec();
        tune_ok.extend_from_slice(&[0, 0]);
        s.write_all(&method_frame(10, 31, &tune_ok)).await.map_err(|e| fail(io_err(e)))?;
        let mut open = Vec::new();
        short_str(&mut open, vhost);
        short_str(&mut open, "");
        open.push(0);
        s.write_all(&method_frame(10, 40, &open)).await.map_err(|e| fail(io_err(e)))?;
        loop {
            let f = read_frame(&mut s).await.map_err(fail)?;
            match f.method() {
                Some((10, 41, _)) => break,
                Some((10, 50, args)) => {
                    let (code, text) = close_reason(args);
                    return Err(fail(explain(code, &text)));
                }
                _ if f.kind == 8 => {}
                _ => return Err(fail("Not an AMQP broker (unexpected frame opening the virtual host)".into())),
            }
        }
        let ms = util::elapsed_ms(start);
        let mut close = 200u16.to_be_bytes().to_vec();
        short_str(&mut close, "OK");
        close.extend_from_slice(&[0, 0, 0, 0]);
        let _ = s.write_all(&method_frame(10, 50, &close)).await;
        let message = format!("{name} · vhost {} opened", util::truncate(vhost, 40));
        Ok(conn::up(ms, message, connected.cert.as_ref(), connected.tls_version))
    };
    match tokio::time::timeout(timeout, probe).await {
        Ok(Ok(outcome)) | Ok(Err(outcome)) => outcome,
        Err(_) => conn::timed_out(&format!("AMQP broker {}:{}", ep.host, ep.port), timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::HeartbeatStatus;
    use crate::monitor::checkers::test_support::{monitor, tls_acceptor};

    fn table_entry(out: &mut Vec<u8>, name: &str, value: &str) {
        short_str(out, name);
        out.push(b'S');
        long_str(out, value.as_bytes());
    }

    /// A scripted broker: user `guest` / `guest`, vhosts `/` and `prod`.
    async fn handle(mut s: Stream) {
        let mut header = [0u8; 8];
        if s.read_exact(&mut header).await.is_err() {
            return;
        }
        if &header != HEADER {
            let _ = s.write_all(HEADER).await;
            return;
        }
        let mut props = Vec::new();
        table_entry(&mut props, "product", "RabbitMQ");
        table_entry(&mut props, "version", "3.12.1");
        short_str(&mut props, "capabilities");
        props.push(b'F');
        let mut caps = Vec::new();
        short_str(&mut caps, "publisher_confirms");
        caps.extend_from_slice(&[b't', 1]);
        props.extend_from_slice(&(caps.len() as u32).to_be_bytes());
        props.extend_from_slice(&caps);
        let mut start = vec![0, 9];
        start.extend_from_slice(&(props.len() as u32).to_be_bytes());
        start.extend_from_slice(&props);
        long_str(&mut start, b"PLAIN AMQPLAIN");
        long_str(&mut start, b"en_US");
        s.write_all(&method_frame(10, 10, &start)).await.unwrap();

        let Ok(f) = read_frame(&mut s).await else { return };
        let (_, _, args) = f.method().unwrap();
        // client props (longstr), mechanism, response, locale
        let mut at = 4 + u32::from_be_bytes(args[..4].try_into().unwrap()) as usize;
        let mech_len = usize::from(args[at]);
        at += 1 + mech_len;
        let resp_len = u32::from_be_bytes(args[at..at + 4].try_into().unwrap()) as usize;
        let response = args[at + 4..at + 4 + resp_len].to_vec();
        if response != b"\0guest\0guest" {
            let mut close = 403u16.to_be_bytes().to_vec();
            short_str(&mut close, "ACCESS_REFUSED - Login was refused using authentication mechanism PLAIN");
            close.extend_from_slice(&[0, 10, 0, 11]);
            s.write_all(&method_frame(10, 50, &close)).await.unwrap();
            return;
        }
        let mut tune = 2047u16.to_be_bytes().to_vec();
        tune.extend_from_slice(&131_072u32.to_be_bytes());
        tune.extend_from_slice(&60u16.to_be_bytes());
        s.write_all(&method_frame(10, 30, &tune)).await.unwrap();
        let _tune_ok = read_frame(&mut s).await.unwrap();
        let open = read_frame(&mut s).await.unwrap();
        let (_, _, args) = open.method().unwrap();
        let vhost = String::from_utf8_lossy(&args[1..1 + usize::from(args[0])]).into_owned();
        if vhost != "/" && vhost != "prod" {
            let mut close = 530u16.to_be_bytes().to_vec();
            short_str(&mut close, &format!("NOT_ALLOWED - vhost {vhost} not found"));
            close.extend_from_slice(&[0, 10, 0, 40]);
            s.write_all(&method_frame(10, 50, &close)).await.unwrap();
            return;
        }
        let mut ok = Vec::new();
        short_str(&mut ok, "");
        s.write_all(&method_frame(10, 41, &ok)).await.unwrap();
        if let Ok(f) = read_frame(&mut s).await
            && f.method().map(|m| (m.0, m.1)) == Some((10, 50))
        {
            let _ = s.write_all(&method_frame(10, 51, &[])).await;
        }
    }

    async fn serve(tls: bool) -> std::net::SocketAddr {
        let acceptor = tls_acceptor();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if tls {
                        if let Ok(stream) = acceptor.accept(tcp).await {
                            handle(Box::new(stream)).await;
                        }
                    } else {
                        handle(Box::new(tcp)).await;
                    }
                });
            }
        });
        addr
    }

    fn amqp(target: &str, user: Option<&str>, pass: &str) -> Monitor {
        let mut m = monitor(MonitorType::Rabbitmq, target);
        m.basic_auth_user = user.map(str::to_string);
        m.basic_auth_pass = Some(pass.into());
        m
    }

    #[tokio::test]
    async fn greeting_names_the_broker() {
        let addr = serve(false).await;
        let out = check(&amqp(&addr.to_string(), None, "")).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("RabbitMQ 3.12.1 · greeting received (set a user name to check the login)"));
    }

    #[tokio::test]
    async fn login_and_vhost() {
        let addr = serve(false).await;
        let out = check(&amqp(&addr.to_string(), Some("guest"), "guest")).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("RabbitMQ 3.12.1 · vhost / opened"));
        let out = check(&amqp(&format!("amqp://{addr}/prod"), Some("guest"), "guest")).await;
        assert_eq!(out.message.as_deref(), Some("RabbitMQ 3.12.1 · vhost prod opened"));

        let out = check(&amqp(&addr.to_string(), Some("guest"), "wrong")).await;
        assert_eq!(out.message.as_deref(), Some("Authentication failed: wrong user name or password"));
        let out = check(&amqp(&format!("amqp://{addr}/nope"), Some("guest"), "guest")).await;
        assert_eq!(out.message.as_deref(), Some("Not allowed: NOT_ALLOWED - vhost nope not found"));
    }

    #[tokio::test]
    async fn amqps_checks_the_certificate() {
        let addr = serve(true).await;
        let target = format!("amqps://localhost:{}", addr.port());
        let out = check(&amqp(&target, Some("guest"), "guest")).await;
        assert!(out.message.as_deref().unwrap().contains("not trusted"), "{:?}", out.message);
        let mut m = amqp(&target, Some("guest"), "guest");
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert!(out.message.unwrap().ends_with("· TLS 1.3"));
        assert!(out.expiry.is_some());
    }

    #[tokio::test]
    async fn other_servers_are_down() {
        let addr = crate::monitor::checkers::test_support::foreign_server(b"HTTP/1.1 400 Bad Request\r\n\r\nxxxxxxxxxxxx").await;
        let out = check(&amqp(&addr.to_string(), None, "")).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Not an AMQP broker"), "");
    }

    #[test]
    fn server_properties_survive_nested_tables() {
        let mut props = Vec::new();
        table_entry(&mut props, "product", "RabbitMQ");
        short_str(&mut props, "capabilities");
        props.push(b'F');
        props.extend_from_slice(&0u32.to_be_bytes());
        table_entry(&mut props, "version", "4.0.2");
        let mut args = vec![0, 9];
        args.extend_from_slice(&(props.len() as u32).to_be_bytes());
        args.extend_from_slice(&props);
        assert_eq!(server_properties(&args), (Some("RabbitMQ".into()), Some("4.0.2".into())));
        assert_eq!(server_properties(&[0, 9, 0, 0, 0, 5, 1]), (None, None));
    }

    /// Against a real broker: `VV_TEST_RABBITMQ=amqp://host:port/vhost,user,password`.
    #[tokio::test]
    #[ignore = "needs a RabbitMQ broker (VV_TEST_RABBITMQ)"]
    async fn live_broker() {
        let spec = std::env::var("VV_TEST_RABBITMQ").expect("VV_TEST_RABBITMQ");
        let [target, user, pass] = spec.splitn(3, ',').collect::<Vec<_>>()[..] else { panic!("target,user,password") };
        let out = check(&amqp(target, Some(user), pass)).await;
        println!("{:?}", out.message);
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        println!("{:?}", check(&amqp(target, Some(user), "wrong")).await.message);
        println!("{:?}", check(&amqp(target, None, "")).await.message);
    }
}
