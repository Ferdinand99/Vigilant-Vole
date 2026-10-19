//! Probe implementations. Owned by the checkers agent.
//!
//! Contract: [`run_check`] performs exactly one probe for a monitor and
//! reports the raw outcome. It never panics, respects
//! `monitor.timeout_seconds`, and does not apply retries / upside-down /
//! maintenance logic (that is the engine's job). Used both by the server's
//! scheduler and by remote agents.
//!
//! Every checker returns a short, specific, human-friendly `message`
//! ("HTTP 503 Service Unavailable", "Keyword \"OK\" not found — body starts: …").
//! Checkers may return [`HeartbeatStatus::Pending`] for transient states
//! (Docker health "starting"); everything else is `Up` or `Down`.
//!
//! Public helpers for forms/validation live in the submodules and are
//! re-exported here: [`HTTP_METHODS`], [`BODY_ENCODINGS`], [`JSON_OPERATORS`],
//! [`DNS_RECORD_TYPES`], [`parse_status_codes`], [`parse_headers`],
//! [`parse_dns_resolver`].

pub mod dns;
pub mod docker;
pub mod domain;
pub mod http;
pub mod ntp;
pub mod ping;
pub mod steam;
pub mod tcp;
pub mod tls;
pub mod util;
pub mod websocket;

use std::time::Duration;

use crate::db::models::{CheckOutcome, HeartbeatStatus, Monitor, MonitorType};

pub use dns::{DNS_RECORD_TYPES, parse_dns_resolver};
pub use http::{
    BODY_ENCODINGS, HTTP_METHODS, JSON_OPERATORS, StatusCodes, parse_headers, parse_status_codes,
};

/// Run one check for `monitor`, dispatching on its type.
///
/// The probe runs in its own task under `tokio::time::timeout`, so a slow
/// target or a misbehaving library can neither exceed the monitor's timeout
/// nor take the caller down with a panic.
pub async fn run_check(monitor: &Monitor) -> CheckOutcome {
    if !monitor.monitor_type.is_active_check() {
        // Fed externally (push endpoint) / derived from children (engine).
        return CheckOutcome::down(format!(
            "{} monitors are not actively checked",
            monitor.monitor_type.label()
        ));
    }

    let timeout = timeout_of(monitor);
    let owned = monitor.clone();
    let mut handle = tokio::spawn(async move { dispatch(&owned).await });
    match tokio::time::timeout(timeout, &mut handle).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) if err.is_panic() => {
            tracing::error!(monitor_id = monitor.id, "checker panicked");
            CheckOutcome::down("Internal error: the checker crashed (see server log)")
        }
        Ok(Err(_)) => CheckOutcome::down("Check was cancelled"),
        Err(_) => {
            handle.abort();
            CheckOutcome::down(format!("Timed out after {}", util::format_secs(timeout)))
        }
    }
}

async fn dispatch(monitor: &Monitor) -> CheckOutcome {
    match monitor.monitor_type {
        MonitorType::Http | MonitorType::Keyword | MonitorType::JsonQuery => {
            http::check(monitor).await
        }
        MonitorType::Tcp => tcp::check(monitor).await,
        MonitorType::Ping => ping::check(monitor).await,
        MonitorType::Dns => dns::check(monitor).await,
        MonitorType::Docker => docker::check(monitor).await,
        MonitorType::Websocket => websocket::check(monitor).await,
        MonitorType::DomainExpiry => domain::check(monitor).await,
        MonitorType::Ntp => ntp::check(monitor).await,
        MonitorType::Steam => steam::check(monitor).await,
        MonitorType::Push | MonitorType::Group => CheckOutcome {
            status: HeartbeatStatus::Down,
            response_time_ms: None,
            message: Some("not actively checked".into()),
            expiry: None,
        },
    }
}

/// The monitor's configured timeout (at least one second).
pub fn timeout_of(monitor: &Monitor) -> Duration {
    Duration::from_secs(monitor.timeout_seconds.max(1).unsigned_abs())
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::db::models::{Monitor, MonitorType};

    /// A monitor with schema defaults, for checker tests.
    pub fn monitor(monitor_type: MonitorType, target: &str) -> Monitor {
        let n = crate::db::models::NewMonitor::default();
        Monitor {
            id: 1,
            name: "test".into(),
            description: String::new(),
            monitor_type,
            target: target.into(),
            parent_id: None,
            interval_seconds: n.interval_seconds,
            retry_interval_seconds: None,
            timeout_seconds: 5,
            retries: 0,
            active: true,
            upside_down: false,
            http_method: n.http_method,
            http_headers: None,
            http_body: None,
            http_body_encoding: n.http_body_encoding,
            accepted_status_codes: n.accepted_status_codes,
            max_redirects: n.max_redirects,
            ignore_tls: false,
            basic_auth_user: None,
            basic_auth_pass: None,
            proxy_url: None,
            keyword: None,
            keyword_invert: false,
            json_path: None,
            json_operator: n.json_operator,
            json_expected: None,
            dns_record_type: n.dns_record_type,
            dns_resolver: None,
            dns_expected: None,
            docker_host: None,
            push_token: None,
            expiry_notify: true,
            expiry_days: 14,
            resend_interval: 0,
            escalation_minutes: 0,
            flap_detection: true,
            run_on_agents: false,
            confirm_with_agents: false,
            source: "manual".into(),
            external_id: None,
            sort_order: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn push_and_group_are_not_checked() {
        let m = test_support::monitor(MonitorType::Push, "");
        let out = run_check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(
            out.message
                .unwrap_or_default()
                .contains("not actively checked")
        );
    }

    #[tokio::test]
    async fn run_check_enforces_timeout() {
        // A TCP listener that accepts nothing useful: Steam query never answers.
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let mut m = test_support::monitor(MonitorType::Steam, &addr.to_string());
        m.timeout_seconds = 1;
        let start = std::time::Instant::now();
        let out = run_check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(start.elapsed() < Duration::from_secs(3));
        drop(sock);
    }

    #[test]
    fn timeout_is_at_least_one_second() {
        let mut m = test_support::monitor(MonitorType::Tcp, "x:1");
        m.timeout_seconds = 0;
        assert_eq!(timeout_of(&m), Duration::from_secs(1));
    }
}
