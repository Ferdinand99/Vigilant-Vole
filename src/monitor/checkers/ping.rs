//! ICMP ping checker: `target` = hostname or IPv4/IPv6 address. One echo
//! request per check; response time = RTT.
//!
//! Needs ICMP sockets: unprivileged ICMP (`net.ipv4.ping_group_range`) or
//! raw sockets (root / `CAP_NET_RAW`, e.g. Docker `--cap-add NET_RAW`).

use std::net::IpAddr;

use super::util;
use crate::db::models::{CheckOutcome, Monitor};

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let target = monitor
        .target
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if target.is_empty() {
        return CheckOutcome::down("No host configured");
    }
    let ip = match resolve_ip(target).await {
        Ok(ip) => ip,
        Err(e) => return CheckOutcome::down(e),
    };

    let config = match ip {
        IpAddr::V4(_) => surge_ping::Config::default(),
        IpAddr::V6(_) => surge_ping::Config::builder()
            .kind(surge_ping::ICMP::V6)
            .build(),
    };
    let client = match surge_ping::Client::new(&config) {
        Ok(c) => c,
        Err(err) => return CheckOutcome::down(socket_error(&err)),
    };

    let mut pinger = client
        .pinger(ip, surge_ping::PingIdentifier(rand::random::<u16>()))
        .await;
    pinger.timeout(super::timeout_of(monitor));

    let payload = [0u8; 56];
    match pinger.ping(surge_ping::PingSequence(0), &payload).await {
        Ok((_packet, rtt)) => {
            let ms = i64::try_from(rtt.as_millis()).unwrap_or(i64::MAX);
            CheckOutcome::up(ms).with_message(format!(
                "Reply from {ip} in {:.1} ms",
                rtt.as_secs_f64() * 1000.0
            ))
        }
        Err(surge_ping::SurgeError::Timeout { .. }) => CheckOutcome::down(format!(
            "No reply from {ip} within {}",
            util::format_secs(super::timeout_of(monitor))
        )),
        Err(surge_ping::SurgeError::IOError(err)) => CheckOutcome::down(socket_error(&err)),
        Err(err) => CheckOutcome::down(format!("Ping {ip} failed: {err}")),
    }
}

fn socket_error(err: &std::io::Error) -> String {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        "ICMP not permitted: run as root or grant CAP_NET_RAW (Docker: --cap-add NET_RAW)".into()
    } else {
        format!("Could not open ICMP socket: {}", util::tidy_io_error(err))
    }
}

async fn resolve_ip(target: &str) -> Result<IpAddr, String> {
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Ok(ip);
    }
    let addrs: Vec<IpAddr> = tokio::net::lookup_host((target, 0))
        .await
        .map_err(|e| {
            format!(
                "Could not resolve \"{target}\": {}",
                util::tidy_io_error(&e)
            )
        })?
        .map(|a| a.ip())
        .collect();
    // Prefer IPv4: it works on more networks/containers.
    addrs
        .iter()
        .find(|ip| ip.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| format!("Could not resolve \"{target}\": no addresses"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_literals() {
        assert_eq!(
            resolve_ip("127.0.0.1").await,
            Ok("127.0.0.1".parse().unwrap())
        );
        assert_eq!(resolve_ip("::1").await, Ok("::1".parse().unwrap()));
    }

    #[test]
    fn permission_error_mentions_net_raw() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(socket_error(&err).contains("NET_RAW"));
    }
}
