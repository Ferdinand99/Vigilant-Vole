//! WebSocket checker (tokio-tungstenite).
//!
//! `target`: `ws://` or `wss://` URL (`http(s)://` is accepted and mapped).
//! Up when the HTTP upgrade succeeds (101); the connection is then closed
//! with a close handshake (best effort, max 2 s, not counted in the response
//! time). Response time = TCP + TLS + upgrade handshake. `ignore_tls` is
//! honoured; on `wss://` the leaf certificate is reported as expiry info.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Error as WsError};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use url::Url;

use super::tls::{self, CertSummary};
use super::util;
use crate::db::models::{CheckOutcome, Monitor};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Normalise the target to a ws/wss URL.
pub fn parse_ws_url(target: &str) -> Result<Url, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("No URL configured".into());
    }
    let mut url = Url::parse(target)
        .map_err(|e| format!("Invalid URL \"{}\": {e}", util::truncate(target, 80)))?;
    let scheme = match url.scheme() {
        "ws" | "http" => "ws",
        "wss" | "https" => "wss",
        other => {
            return Err(format!(
                "Unsupported URL scheme \"{other}\" (use ws:// or wss://)"
            ));
        }
    };
    if url.scheme() != scheme {
        url.set_scheme(scheme)
            .map_err(|()| "Invalid URL".to_string())?;
    }
    Ok(url)
}

async fn connect(
    url: &Url,
    ignore_tls: bool,
) -> Result<(Ws, tungstenite::handshake::client::Response), WsError> {
    let mut request = url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "user-agent",
        HeaderValue::from_static(crate::server::USER_AGENT),
    );
    let connector = if url.scheme() == "wss" {
        let config = if ignore_tls {
            tls::insecure_config()
        } else {
            tls::verified_config()
        };
        config.map(Connector::Rustls)
    } else {
        None
    };
    tokio_tungstenite::connect_async_tls_with_config(request, Some(ws_config()), true, connector)
        .await
}

/// Largest message/frame accepted from a monitored server (default is 64 MiB):
/// a check only needs the handshake and maybe a small reply.
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

fn ws_config() -> tungstenite::protocol::WebSocketConfig {
    tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
}

fn peer_cert(ws: &Ws) -> Option<CertSummary> {
    match ws.get_ref() {
        MaybeTlsStream::Rustls(stream) => {
            let certs = stream.get_ref().1.peer_certificates()?;
            tls::parse_cert(certs.first()?)
        }
        _ => None,
    }
}

async fn close(mut ws: Ws) {
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        if ws.close(None).await.is_ok() {
            while let Some(Ok(_)) = ws.next().await {}
        }
    })
    .await;
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let url = match parse_ws_url(&monitor.target) {
        Ok(u) => u,
        Err(e) => return CheckOutcome::down(e),
    };
    let start = Instant::now();
    match connect(&url, monitor.ignore_tls).await {
        Ok((ws, response)) => {
            let ms = util::elapsed_ms(start);
            let cert = peer_cert(&ws);
            close(ws).await;
            let mut outcome = CheckOutcome::up(ms).with_message(format!(
                "Handshake OK (HTTP {} Switching Protocols)",
                response.status().as_u16()
            ));
            outcome.expiry = cert.as_ref().and_then(CertSummary::expiry_info);
            outcome
        }
        Err(err) => error_outcome(&err, &url, monitor.ignore_tls).await,
    }
}

async fn error_outcome(err: &WsError, url: &Url, ignore_tls: bool) -> CheckOutcome {
    match err {
        WsError::Http(resp) => {
            let status = resp.status();
            let reason = status
                .canonical_reason()
                .map(|r| format!(" {r}"))
                .unwrap_or_default();
            CheckOutcome::down(format!("Upgrade refused: HTTP {}{reason}", status.as_u16()))
        }
        WsError::Url(e) => CheckOutcome::down(format!("Invalid WebSocket URL: {e}")),
        other => {
            let chain = util::full_chain(other);
            if url.scheme() == "wss"
                && !ignore_tls
                && let Some(reason) = tls::classify_tls_error(&chain)
            {
                // Fetch the certificate again without verification to explain why.
                let mut https = url.clone();
                let cert = match https.set_scheme("https") {
                    Ok(()) => {
                        super::http::probe_cert_unverified(&https, Duration::from_secs(5)).await
                    }
                    Err(()) => None,
                };
                let now = util::now_unix();
                let mut outcome = match &cert {
                    Some(c) if c.is_expired(now) => CheckOutcome::down(c.expired_message(now)),
                    _ => CheckOutcome::down(reason),
                };
                outcome.expiry = cert.as_ref().and_then(CertSummary::expiry_info);
                return outcome;
            }
            match other {
                WsError::Io(io) => {
                    CheckOutcome::down(format!("Connection failed: {}", util::tidy_io_error(io)))
                }
                _ => CheckOutcome::down(util::root_cause(other)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{HeartbeatStatus, MonitorType};
    use crate::monitor::checkers::test_support::monitor;

    #[test]
    fn messages_from_monitored_servers_are_capped() {
        let cfg = ws_config();
        assert_eq!(cfg.max_message_size, Some(MAX_MESSAGE_BYTES));
        assert_eq!(cfg.max_frame_size, Some(MAX_MESSAGE_BYTES));
    }

    #[test]
    fn url_normalisation() {
        assert_eq!(
            parse_ws_url("https://x/socket").unwrap().as_str(),
            "wss://x/socket"
        );
        assert_eq!(
            parse_ws_url("ws://x:8080/").unwrap().as_str(),
            "ws://x:8080/"
        );
        assert!(parse_ws_url("ftp://x").is_err());
        assert!(parse_ws_url("").is_err());
    }

    /// Accepts WebSocket upgrades on any path except `/plain`, which gets a 200.
    async fn serve() -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut peek = [0u8; 16];
                    let n = stream.peek(&mut peek).await.unwrap_or(0);
                    if peek[..n].starts_with(b"GET /plain") {
                        let reply = "HTTP/1.1 200 OK|content-length: 2|connection: close||ok"
                            .replace('|', "\r\n");
                        // Drain the request first so closing doesn't send a TCP reset.
                        let mut req = [0u8; 2048];
                        let _ = stream.read(&mut req).await;
                        let _ = stream.write_all(reply.as_bytes()).await;
                        let _ = stream.shutdown().await;
                        return;
                    }
                    if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                        while let Some(Ok(_)) = ws.next().await {}
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn handshake_ok_and_refused() {
        let addr = serve().await;
        let out = check(&monitor(MonitorType::Websocket, &format!("ws://{addr}/ws"))).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(
            out.message.as_deref(),
            Some("Handshake OK (HTTP 101 Switching Protocols)")
        );

        let out = check(&monitor(
            MonitorType::Websocket,
            &format!("ws://{addr}/plain"),
        ))
        .await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        let msg = out.message.unwrap();
        assert!(msg.starts_with("Upgrade refused: HTTP 200"), "{msg}");
    }

    #[tokio::test]
    async fn connection_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let out = check(&monitor(MonitorType::Websocket, &format!("ws://{addr}/"))).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Connection failed"));
    }

    #[tokio::test]
    #[ignore = "needs internet"]
    async fn live_wss() {
        let out = check(&monitor(
            MonitorType::Websocket,
            "wss://ws.postman-echo.com/raw",
        ))
        .await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert!(out.expiry.is_some());
        let out = check(&monitor(
            MonitorType::Websocket,
            "wss://expired.badssl.com/",
        ))
        .await;
        let msg = out.message.unwrap();
        assert!(msg.starts_with("TLS: certificate expired"), "{msg}");
    }
}
