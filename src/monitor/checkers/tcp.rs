//! TCP port checker: `target` = `host:port` (`[v6]:port` for IPv6 literals).
//! Up when a TCP connection is established; response time = connect time
//! (excluding DNS resolution).

use std::time::Instant;

use super::util;
use crate::db::models::{CheckOutcome, Monitor};

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let (host, port) = match util::split_host_port(&monitor.target, None) {
        Ok(hp) => hp,
        Err(e) => return CheckOutcome::down(e),
    };
    let addrs = match util::resolve_all(&host, port).await {
        Ok(addrs) => addrs,
        Err(e) => return CheckOutcome::down(e),
    };
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    match tokio::time::timeout(timeout, util::connect_any(&addrs)).await {
        Ok(Ok((_stream, addr))) => CheckOutcome::up(util::elapsed_ms(start))
            .with_message(format!("Port {port} open on {}", addr.ip())),
        Ok(Err((addr, err))) => CheckOutcome::down(format!(
            "Port {port} on {}: {}",
            addr.ip(),
            util::tidy_io_error(&err)
        )),
        Err(_) => CheckOutcome::down(format!(
            "Port {port} on {}: no answer within {}",
            addrs[addrs.len() - 1].ip(),
            util::format_secs(timeout)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{HeartbeatStatus, MonitorType};
    use crate::monitor::checkers::test_support::monitor;

    #[tokio::test]
    async fn open_port_is_up() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let out = check(&monitor(MonitorType::Tcp, &addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Up);
        assert_eq!(
            out.message,
            Some(format!("Port {} open on 127.0.0.1", addr.port()))
        );
    }

    #[tokio::test]
    async fn a_metadata_address_is_refused() {
        let out = check(&monitor(MonitorType::Tcp, "169.254.169.254:80")).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        let msg = out.message.unwrap();
        assert!(
            msg.starts_with("Refused: the address is not allowed"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn closed_port_is_down() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let out = check(&monitor(MonitorType::Tcp, &addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().contains("Connection refused") || cfg!(windows));
    }

    #[tokio::test]
    async fn localhost_reaches_an_ipv4_only_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let out = check(&monitor(MonitorType::Tcp, &format!("localhost:{port}"))).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert_eq!(out.message, Some(format!("Port {port} open on 127.0.0.1")));
    }

    #[tokio::test]
    async fn missing_port_is_down() {
        let out = check(&monitor(MonitorType::Tcp, "localhost")).await;
        assert_eq!(
            out.message.as_deref(),
            Some("Target \"localhost\" needs a port (host:port)")
        );
    }
}
