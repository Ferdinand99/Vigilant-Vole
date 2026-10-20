//! gRPC checker: `target` = `[grpc|grpcs]://host[:port][/service]`.
//!
//! Calls the standard health service (`grpc.health.v1.Health/Check`) over
//! HTTP/2: `grpcs://` uses TLS with ALPN `h2`, `grpc://` is plain HTTP/2
//! ("prior knowledge"). The path names the service to ask about; empty means
//! the whole server. Up only when the answer is `SERVING`. Response time =
//! connect + call.

use std::time::Instant;

use bytes::Bytes;
use http::{Request, StatusCode};

use super::conn;
use super::endpoint;
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

const METHOD: &str = "/grpc.health.v1.Health/Check";
const MAX_BODY: usize = 64 * 1024;

/// A `HealthCheckRequest`, framed for gRPC (no compression).
fn request_message(service: &str) -> Vec<u8> {
    let mut proto = Vec::new();
    if !service.is_empty() {
        proto.push(0x0A); // field 1, length-delimited
        let mut len = service.len();
        loop {
            let byte = (len & 0x7F) as u8;
            len >>= 7;
            if len == 0 {
                proto.push(byte);
                break;
            }
            proto.push(byte | 0x80);
        }
        proto.extend_from_slice(service.as_bytes());
    }
    let mut framed = vec![0];
    framed.extend_from_slice(&(proto.len() as u32).to_be_bytes());
    framed.extend_from_slice(&proto);
    framed
}

/// `ServingStatus` from a framed `HealthCheckResponse` (UNKNOWN when absent).
fn parse_status(body: &[u8]) -> Result<u64, String> {
    let bad = || "Not a gRPC health answer".to_string();
    let (flag, rest) = body.split_first().ok_or_else(bad)?;
    if *flag != 0 {
        return Err("The server compressed the answer, which this monitor doesn't support".into());
    }
    let len = u32::from_be_bytes(rest.get(..4).ok_or_else(bad)?.try_into().map_err(|_| bad())?) as usize;
    let msg = rest.get(4..4 + len).ok_or_else(bad)?;
    let mut at = 0;
    while at < msg.len() {
        let tag = msg[at];
        at += 1;
        if tag == 0x08 {
            // varint
            let (mut value, mut shift) = (0u64, 0);
            loop {
                let b = *msg.get(at).ok_or_else(bad)?;
                at += 1;
                value |= u64::from(b & 0x7F) << shift;
                shift += 7;
                if b & 0x80 == 0 || shift > 63 {
                    break;
                }
            }
            return Ok(value);
        }
        // Skip a length-delimited unknown field; anything else ends the scan.
        if tag & 7 != 2 {
            break;
        }
        let n = usize::from(*msg.get(at).ok_or_else(bad)?);
        at += 1 + n;
    }
    Ok(0)
}

fn grpc_status_name(code: u32) -> &'static str {
    match code {
        1 => "CANCELLED",
        2 => "UNKNOWN",
        3 => "INVALID_ARGUMENT",
        4 => "DEADLINE_EXCEEDED",
        5 => "NOT_FOUND",
        7 => "PERMISSION_DENIED",
        8 => "RESOURCE_EXHAUSTED",
        12 => "UNIMPLEMENTED",
        13 => "INTERNAL",
        14 => "UNAVAILABLE",
        16 => "UNAUTHENTICATED",
        _ => "error",
    }
}

fn explain(code: u32, message: &str, service: &str) -> String {
    match code {
        5 => format!("The service \"{}\" isn't registered for health checks", util::truncate(service, 60)),
        12 => "The server doesn't implement the gRPC health service (grpc.health.v1.Health)".into(),
        14 => "The server is unavailable".into(),
        16 | 7 => format!("Not allowed: gRPC {} {}", grpc_status_name(code), util::truncate(message, 80)).trim_end().to_string(),
        _ => format!("gRPC status {code} ({}) {}", grpc_status_name(code), util::truncate(message, 80)).trim_end().to_string(),
    }
}

/// Aborts a spawned task when dropped. The HTTP/2 connection task belongs to
/// the probe: when the monitor's timeout drops the probe mid-call, the task
/// and its socket go with it instead of depending on the peer to finish the
/// shutdown (a peer that stops reading would keep it alive).
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn h2_err(e: &h2::Error) -> String {
    if e.is_io() {
        "Connection closed by the server".into()
    } else if let Some(reason) = e.reason() {
        format!("HTTP/2 error: {reason}")
    } else {
        "The server doesn't speak HTTP/2 (is this a gRPC port?)".into()
    }
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let ep = match endpoint::parse(MonitorType::Grpc, &monitor.target) {
        Ok(ep) => ep,
        Err(e) => return CheckOutcome::down(e),
    };
    let service = ep.path.as_str();
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    let probe = async {
        let connected = conn::connect_alpn(&ep, monitor.ignore_tls, timeout, &[b"h2"]).await?;
        let fail = CheckOutcome::down;
        let (mut client, connection) = h2::client::handshake(connected.io).await.map_err(|e| fail(h2_err(&e)))?;
        let driver = AbortOnDrop(tokio::spawn(connection));
        let result = async {
            client = client.ready().await.map_err(|e| fail(h2_err(&e)))?;
            let host = if ep.host.contains(':') { format!("[{}]", ep.host) } else { ep.host.clone() };
            let request = Request::builder()
                .method("POST")
                .uri(format!("{}://{host}:{}{METHOD}", if ep.tls { "https" } else { "http" }, ep.port))
                .header("content-type", "application/grpc")
                .header("te", "trailers")
                .header("user-agent", crate::server::USER_AGENT)
                .body(())
                .map_err(|_| fail("Invalid address".into()))?;
            let (response, mut send) = client.send_request(request, false).map_err(|e| fail(h2_err(&e)))?;
            send.send_data(Bytes::from(request_message(service)), true).map_err(|e| fail(h2_err(&e)))?;
            let response = response.await.map_err(|e| fail(h2_err(&e)))?;
            let (parts, mut body) = response.into_parts();
            if parts.status != StatusCode::OK {
                return Err(fail(format!("The server answered HTTP {} (is this a gRPC endpoint?)", parts.status.as_u16())));
            }
            let mut data = Vec::new();
            while let Some(chunk) = body.data().await {
                let chunk = chunk.map_err(|e| fail(h2_err(&e)))?;
                let _ = body.flow_control().release_capacity(chunk.len());
                data.extend_from_slice(&chunk);
                if data.len() > MAX_BODY {
                    return Err(fail("The answer is unexpectedly large".into()));
                }
            }
            let trailers = body.trailers().await.map_err(|e| fail(h2_err(&e)))?;
            // A "trailers-only" error puts grpc-status in the headers.
            let status_of = |h: &http::HeaderMap| {
                let code = h.get("grpc-status")?.to_str().ok()?.parse::<u32>().ok()?;
                let msg = h.get("grpc-message").and_then(|m| m.to_str().ok()).unwrap_or_default().to_string();
                Some((code, msg))
            };
            let (code, message) = trailers.as_ref().and_then(status_of).or_else(|| status_of(&parts.headers)).unwrap_or((0, String::new()));
            if code != 0 {
                return Err(fail(explain(code, &message, service)));
            }
            parse_status(&data).map_err(fail)
        }
        .await;
        drop(driver);
        let status = result?;
        let ms = util::elapsed_ms(start);
        let what = if service.is_empty() { "server".to_string() } else { format!("service {}", util::truncate(service, 60)) };
        match status {
            1 => Ok(conn::up(ms, format!("SERVING ({what})"), connected.cert.as_ref(), connected.tls_version)),
            2 => Err(fail(format!("NOT_SERVING ({what})"))),
            3 => Err(fail(format!("SERVICE_UNKNOWN: the server doesn't know {what}"))),
            _ => Err(fail(format!("The health status is UNKNOWN ({what})"))),
        }
    };
    match tokio::time::timeout(timeout, probe).await {
        Ok(Ok(outcome)) | Ok(Err(outcome)) => outcome,
        Err(_) => conn::timed_out(&format!("gRPC server {}:{}", ep.host, ep.port), timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::HeartbeatStatus;
    use crate::monitor::checkers::test_support::{monitor, tls_acceptor_alpn};
    use tokio::io::{AsyncRead, AsyncWrite};

    /// A health server: `` and `ok.Service` are SERVING, `bad.Service` is
    /// NOT_SERVING, anything else is NOT_FOUND; the method must be the health check.
    async fn serve_h2<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(io: S) {
        let Ok(mut conn) = h2::server::handshake(io).await else { return };
        while let Some(Ok((request, mut respond))) = conn.accept().await {
            tokio::spawn(async move {
                let (parts, mut body) = request.into_parts();
                let mut data = Vec::new();
                while let Some(Ok(chunk)) = body.data().await {
                    let _ = body.flow_control().release_capacity(chunk.len());
                    data.extend_from_slice(&chunk);
                }
                let json = |status: &str| http::Response::builder().status(200).header("content-type", "application/grpc").body(()).map(|r| (r, status.to_string())).unwrap();
                if parts.uri.path() != METHOD {
                    let (resp, _) = json("");
                    let mut send = respond.send_response(resp, false).unwrap();
                    let mut t = http::HeaderMap::new();
                    t.insert("grpc-status", "12".parse().unwrap());
                    send.send_trailers(t).unwrap();
                    return;
                }
                let service = if data.len() > 7 { String::from_utf8_lossy(&data[7..]).into_owned() } else { String::new() };
                let status: Option<u8> = match service.as_str() {
                    "" | "ok.Service" => Some(1),
                    "bad.Service" => Some(2),
                    _ => None,
                };
                let (resp, _) = json("");
                match status {
                    Some(s) => {
                        let mut send = respond.send_response(resp, false).unwrap();
                        send.send_data(Bytes::from(vec![0, 0, 0, 0, 2, 0x08, s]), false).unwrap();
                        let mut t = http::HeaderMap::new();
                        t.insert("grpc-status", "0".parse().unwrap());
                        send.send_trailers(t).unwrap();
                    }
                    None => {
                        // Trailers-only error response.
                        let resp = http::Response::builder()
                            .status(200)
                            .header("content-type", "application/grpc")
                            .header("grpc-status", "5")
                            .header("grpc-message", "unknown service")
                            .body(())
                            .unwrap();
                        respond.send_response(resp, true).unwrap();
                    }
                }
            });
        }
    }

    async fn serve(tls: bool) -> std::net::SocketAddr {
        let acceptor = tls_acceptor_alpn(&[b"h2"]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if tls {
                        if let Ok(stream) = acceptor.accept(tcp).await {
                            serve_h2(stream).await;
                        }
                    } else {
                        serve_h2(tcp).await;
                    }
                });
            }
        });
        addr
    }

    fn grpc(target: &str) -> Monitor {
        let mut m = monitor(MonitorType::Grpc, target);
        m.timeout_seconds = 3;
        m
    }

    #[test]
    fn health_request_is_framed() {
        assert_eq!(request_message(""), [0, 0, 0, 0, 0]);
        assert_eq!(request_message("ab"), [0, 0, 0, 0, 4, 0x0A, 2, b'a', b'b']);
        assert_eq!(parse_status(&[0, 0, 0, 0, 2, 0x08, 1]), Ok(1));
        assert_eq!(parse_status(&[0, 0, 0, 0, 0]), Ok(0));
        assert!(parse_status(&[1, 0, 0, 0, 0]).is_err());
        assert!(parse_status(&[0, 0, 0, 0, 9, 1]).is_err());
    }

    #[tokio::test]
    async fn serving_and_not_serving() {
        let addr = serve(false).await;
        let out = check(&grpc(&format!("grpc://{addr}"))).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("SERVING (server)"));
        let out = check(&grpc(&format!("grpc://{addr}/ok.Service"))).await;
        assert_eq!(out.message.as_deref(), Some("SERVING (service ok.Service)"));
        let out = check(&grpc(&format!("grpc://{addr}/bad.Service"))).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert_eq!(out.message.as_deref(), Some("NOT_SERVING (service bad.Service)"));
        let out = check(&grpc(&format!("grpc://{addr}/nope"))).await;
        assert_eq!(out.message.as_deref(), Some("The service \"nope\" isn't registered for health checks"));
    }

    #[tokio::test]
    async fn grpcs_negotiates_h2_and_checks_the_certificate() {
        let addr = serve(true).await;
        let target = format!("grpcs://localhost:{}", addr.port());
        let out = check(&grpc(&target)).await;
        assert!(out.message.as_deref().unwrap().contains("not trusted"), "{:?}", out.message);
        let mut m = grpc(&target);
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message.as_deref(), Some("SERVING (server) · TLS 1.3"));
        assert!(out.expiry.is_some());
    }

    /// A server that takes the call and never answers: once the check times
    /// out, the client must hang up (no connection task left running).
    #[tokio::test]
    async fn a_timed_out_call_closes_the_connection() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            while matches!(tcp.read(&mut buf).await, Ok(n) if n > 0) {}
            let _ = closed_tx.send(());
        });
        let mut m = grpc(&format!("grpc://{addr}"));
        m.timeout_seconds = 1;
        let out = check(&m).await;
        assert!(out.message.as_deref().unwrap().contains("no answer within"), "{:?}", out.message);
        let closed = tokio::time::timeout(std::time::Duration::from_secs(3), closed_rx).await;
        assert!(closed.is_ok(), "the connection outlived the check");
    }

    #[tokio::test]
    async fn dropping_the_guard_aborts_the_task() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let guard = AbortOnDrop(tokio::spawn(async move {
            let _tx = tx; // dropped when the task ends
            std::future::pending::<()>().await;
        }));
        drop(guard);
        let ended = tokio::time::timeout(std::time::Duration::from_secs(1), rx).await;
        assert!(matches!(ended, Ok(Err(_))), "the task kept running");
    }

    #[tokio::test]
    async fn other_servers_are_down() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });
        let out = check(&grpc(&format!("grpc://{addr}"))).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(!out.message.unwrap().is_empty());
    }

    /// Against a real server: `VV_TEST_GRPC=grpc://host:port` (services `ok.Service`
    /// SERVING and `bad.Service` NOT_SERVING registered).
    #[tokio::test]
    #[ignore = "needs a gRPC server with the health service (VV_TEST_GRPC)"]
    async fn live_server() {
        let base = std::env::var("VV_TEST_GRPC").expect("VV_TEST_GRPC");
        for suffix in ["", "/ok.Service", "/bad.Service", "/unregistered"] {
            let out = check(&grpc(&format!("{base}{suffix}"))).await;
            println!("{suffix:?} -> {:?} {:?}", out.status, out.message);
        }
        let out = check(&grpc(&base)).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    }
}
