use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::db::models::HeartbeatStatus;

pub struct CheckOutcome {
    pub status: HeartbeatStatus,
    pub response_time_ms: Option<i64>,
    pub message: Option<String>,
}

impl CheckOutcome {
    fn down(message: impl Into<String>) -> Self {
        Self {
            status: HeartbeatStatus::Down,
            response_time_ms: None,
            message: Some(message.into()),
        }
    }

    fn up(response_time_ms: i64) -> Self {
        Self {
            status: HeartbeatStatus::Up,
            response_time_ms: Some(response_time_ms),
            message: None,
        }
    }
}

pub async fn check_http(client: &reqwest::Client, target: &str, timeout: Duration) -> CheckOutcome {
    let start = Instant::now();
    match tokio::time::timeout(timeout, client.get(target).send()).await {
        Ok(Ok(resp)) => {
            let elapsed = start.elapsed().as_millis() as i64;
            if resp.status().is_success() || resp.status().is_redirection() {
                CheckOutcome::up(elapsed)
            } else {
                CheckOutcome {
                    status: HeartbeatStatus::Down,
                    response_time_ms: Some(elapsed),
                    message: Some(format!("HTTP {}", resp.status())),
                }
            }
        }
        Ok(Err(err)) => CheckOutcome::down(err.to_string()),
        Err(_) => CheckOutcome::down("timed out"),
    }
}

pub async fn check_tcp(target: &str, timeout: Duration) -> CheckOutcome {
    let start = Instant::now();
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(target)).await {
        Ok(Ok(_stream)) => CheckOutcome::up(start.elapsed().as_millis() as i64),
        Ok(Err(err)) => CheckOutcome::down(err.to_string()),
        Err(_) => CheckOutcome::down("timed out"),
    }
}

pub async fn check_ping(target: &str, timeout: Duration) -> CheckOutcome {
    let ip = match resolve_ip(target).await {
        Some(ip) => ip,
        None => return CheckOutcome::down("could not resolve host"),
    };

    let config = match ip {
        IpAddr::V4(_) => surge_ping::Config::default(),
        IpAddr::V6(_) => surge_ping::Config::builder().kind(surge_ping::ICMP::V6).build(),
    };

    let client = match surge_ping::Client::new(&config) {
        Ok(c) => c,
        Err(err) => return CheckOutcome::down(err.to_string()),
    };

    let mut pinger = client
        .pinger(ip, surge_ping::PingIdentifier(rand_identifier()))
        .await;
    pinger.timeout(timeout);

    let payload = [0u8; 8];
    match pinger.ping(surge_ping::PingSequence(0), &payload).await {
        Ok((_packet, dur)) => CheckOutcome::up(dur.as_millis() as i64),
        Err(err) => CheckOutcome::down(err.to_string()),
    }
}

async fn resolve_ip(target: &str) -> Option<IpAddr> {
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Some(ip);
    }
    tokio::net::lookup_host((target, 0))
        .await
        .ok()?
        .next()
        .map(|addr| addr.ip())
}

fn rand_identifier() -> u16 {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)) as u16
}
