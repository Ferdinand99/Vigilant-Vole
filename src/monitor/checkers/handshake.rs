//! TLS handshake checker: `target` = `host[:port]` (port defaults to 443).
//! Up when a TLS handshake completes and the certificate is acceptable
//! (verified against the OS trust store unless `ignore_tls`). Works for any
//! implicit-TLS service (HTTPS, IMAPS, LDAPS, ...). The leaf certificate is
//! reported as expiry info, so the usual expiry warnings apply. Response time
//! = TCP connect + handshake.

use std::time::Instant;

use super::tls::{self, CertSummary};
use super::util;
use crate::db::models::{CheckOutcome, Monitor};

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let (host, port) = match util::split_host_port(&monitor.target, Some(443)) {
        Ok(hp) => hp,
        Err(e) => return CheckOutcome::down(e),
    };
    let addrs = match util::resolve_all(&host, port).await {
        Ok(addrs) => addrs,
        Err(e) => return CheckOutcome::down(e),
    };
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    // The address that answered (or the last one tried), for the messages.
    let mut addr = addrs[0];
    let handshake = async {
        let (tcp, used) = util::connect_any(&addrs).await.map_err(|(tried, e)| {
            addr = tried;
            e
        })?;
        addr = used;
        tls::upgrade(tcp, &host, monitor.ignore_tls).await
    };
    let result = tokio::time::timeout(timeout, handshake).await;
    match result {
        Ok(Ok(stream)) => {
            let ms = util::elapsed_ms(start);
            let cert = tls::peer_cert(&stream);
            let now = util::now_unix();
            if let Some(c) = cert.as_ref().filter(|c| !monitor.ignore_tls && c.is_expired(now)) {
                let mut outcome = CheckOutcome::down(c.expired_message(now));
                outcome.expiry = c.expiry_info();
                return outcome;
            }
            let mut message = format!("{} handshake OK", tls::version_label(&stream));
            if let Some(c) = &cert {
                if let Some(subject) = &c.subject {
                    message.push_str(&format!(" · certificate for {subject}"));
                }
                message.push_str(&format!(" · {}", util::relative_days(c.not_after_unix, now)));
            }
            let mut outcome = CheckOutcome::up(ms).with_message(message);
            outcome.expiry = cert.as_ref().and_then(CertSummary::expiry_info);
            outcome
        }
        Ok(Err(err)) => match tls::explain_failure(&err, timeout, || tls::fetch_cert(&host, addr)).await {
            Some(outcome) if !monitor.ignore_tls => outcome,
            _ => CheckOutcome::down(format!("TLS handshake with {}:{port} failed: {}", addr.ip(), util::tidy_io_error(&err))),
        },
        Err(_) => CheckOutcome::down(format!(
            "TLS handshake with {}:{port}: no answer within {}",
            addr.ip(),
            util::format_secs(timeout)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{HeartbeatStatus, MonitorType};
    use crate::monitor::checkers::test_support::{monitor, tls_acceptor};

    async fn tls_server() -> std::net::SocketAddr {
        let acceptor = tls_acceptor();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = acceptor.accept(tcp).await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn untrusted_certificate_is_down_unless_ignored() {
        let addr = tls_server().await;
        let target = format!("localhost:{}", addr.port());
        let out = check(&monitor(MonitorType::Tls, &target)).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.as_deref().unwrap().contains("not trusted"), "{:?}", out.message);
        assert!(out.expiry.is_some(), "the certificate is still reported");

        let mut m = monitor(MonitorType::Tls, &target);
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        let message = out.message.unwrap();
        assert!(message.starts_with("TLS 1.3 handshake OK"), "{message}");
        assert!(out.expiry.is_some());
    }

    #[tokio::test]
    async fn plain_tcp_server_is_down() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Ok((mut tcp, _)) = listener.accept().await {
                let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });
        let mut m = monitor(MonitorType::Tls, &addr.to_string());
        m.ignore_tls = true;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("TLS handshake with 127.0.0.1"));
    }

    #[tokio::test]
    async fn closed_port_is_down() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let out = check(&monitor(MonitorType::Tls, &addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
    }
}
