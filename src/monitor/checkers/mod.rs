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

pub mod conn;
pub mod dns;
pub mod docker;
pub mod domain;
pub mod endpoint;
pub mod game_server;
pub mod grpc;
pub mod grpc_keyword;
pub mod handshake;
pub mod http;
pub mod http_auth;
pub mod kafka;
pub mod mongodb;
pub mod mqtt;
pub mod mysql;
pub mod ntp;
pub mod ping;
pub mod postgres;
pub mod rabbitmq;
pub mod radius;
pub mod real_browser;
pub mod redis;
pub mod scram;
pub mod smtp;
pub mod snmp;
pub mod sqlserver;
pub mod steam;
pub mod tailscale;
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
pub use http_auth::{AUTH_METHODS, OAUTH_CLIENT_AUTHS};

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
        MonitorType::TailscalePing => tailscale::check(monitor).await,
        MonitorType::Dns => dns::check(monitor).await,
        MonitorType::Docker => docker::check(monitor).await,
        MonitorType::Websocket => websocket::check(monitor).await,
        MonitorType::DomainExpiry => domain::check(monitor).await,
        MonitorType::Ntp => ntp::check(monitor).await,
        MonitorType::Steam => steam::check(monitor).await,
        MonitorType::GameServer => game_server::check(monitor).await,
        MonitorType::Tls => handshake::check(monitor).await,
        MonitorType::Smtp => smtp::check(monitor).await,
        MonitorType::Redis => redis::check(monitor).await,
        MonitorType::Postgres => postgres::check(monitor).await,
        MonitorType::Mysql => mysql::check(monitor).await,
        MonitorType::Mqtt => mqtt::check(monitor).await,
        MonitorType::Rabbitmq => rabbitmq::check(monitor).await,
        MonitorType::Kafka => kafka::check(monitor).await,
        MonitorType::Snmp => snmp::check(monitor).await,
        MonitorType::Radius => radius::check(monitor).await,
        MonitorType::Grpc => grpc::check(monitor).await,
        MonitorType::GrpcKeyword => grpc_keyword::check(monitor).await,
        MonitorType::SqlServer => sqlserver::check(monitor).await,
        MonitorType::Mongodb => mongodb::check(monitor).await,
        MonitorType::RealBrowser => real_browser::check(monitor).await,
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
    use std::sync::Arc;

    use crate::db::models::{Monitor, MonitorType};

    /// A TLS acceptor with a fresh self-signed certificate for `localhost`
    /// (untrusted by the OS store, so verified checks fail and `ignore_tls`
    /// ones pass).
    pub fn tls_acceptor() -> tokio_rustls::TlsAcceptor {
        tls_acceptor_alpn(&[])
    }

    /// [`tls_acceptor`] that negotiates the given ALPN protocols.
    pub fn tls_acceptor_alpn(alpn: &[&[u8]]) -> tokio_rustls::TlsAcceptor {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![key.cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(key.signing_key.serialize_der().into()),
            )
            .unwrap();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }

    /// A server that speaks another protocol: for every connection it reads
    /// the client's first bytes, answers `reply`, closes its sending side and
    /// waits for the client to hang up. Reading first matters: a socket
    /// closed with unread data is reset, and on Windows a reset discards
    /// the reply before the client reads it.
    pub async fn foreign_server(reply: &'static [u8]) -> std::net::SocketAddr {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut buf)).await;
                    if tcp.write_all(reply).await.is_err() || tcp.shutdown().await.is_err() {
                        return;
                    }
                    let drain = async { while matches!(tcp.read(&mut buf).await, Ok(n) if n > 0) {} };
                    let _ = tokio::time::timeout(Duration::from_secs(10), drain).await;
                });
            }
        });
        addr
    }

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
            secret: None,
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
            auth_method: n.auth_method,
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_scope: None,
            oauth_audience: None,
            oauth_client_auth: n.oauth_client_auth,
            tls_client_cert: None,
            tls_client_key: None,
            tls_ca_cert: None,
            remote_browser_id: None,
            browser_selector: None,
            browser_wait: n.browser_wait,
            browser_viewport: n.browser_viewport,
            browser_js_errors: false,
            grpc_proto: None,
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
