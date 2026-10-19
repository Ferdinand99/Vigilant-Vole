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
    let addr = match util::resolve(&host, port).await {
        Ok(addr) => addr,
        Err(e) => return CheckOutcome::down(e),
    };
    let timeout = super::timeout_of(monitor);
    let start = Instant::now();
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_stream)) => CheckOutcome::up(util::elapsed_ms(start))
            .with_message(format!("Port {port} open on {}", addr.ip())),
        Ok(Err(err)) => CheckOutcome::down(format!(
            "Port {port} on {}: {}",
            addr.ip(),
            util::tidy_io_error(&err)
        )),
        Err(_) => CheckOutcome::down(format!(
            "Port {port} on {}: no answer within {}",
            addr.ip(),
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
    async fn closed_port_is_down() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let out = check(&monitor(MonitorType::Tcp, &addr.to_string())).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().contains("Connection refused") || cfg!(windows));
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
