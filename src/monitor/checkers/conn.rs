//! Shared connection setup for the service checkers: resolve, connect, and
//! (for `rediss://`-style targets) run the TLS handshake, so every checker
//! starts from a byte stream and reports certificate problems the same way.

use std::net::SocketAddr;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use super::endpoint::Endpoint;
use super::tls::{self, CertSummary};
use super::util;
use crate::db::models::CheckOutcome;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// An open connection plus what the TLS handshake revealed.
pub struct Connected {
    pub io: Box<dyn Io>,
    pub addr: SocketAddr,
    pub cert: Option<CertSummary>,
    /// "TLS 1.3" when the connection is encrypted.
    pub tls_version: Option<&'static str>,
}

/// Resolve and connect to `endpoint`; wrap in TLS when its scheme asks for it.
/// The caller applies the monitor's timeout around the whole check.
pub async fn connect(endpoint: &Endpoint, ignore_tls: bool, timeout: std::time::Duration) -> Result<Connected, CheckOutcome> {
    connect_alpn(endpoint, ignore_tls, timeout, &[]).await
}

/// [`connect`] offering ALPN protocols during the TLS handshake.
pub async fn connect_alpn(
    endpoint: &Endpoint,
    ignore_tls: bool,
    timeout: std::time::Duration,
    alpn: &[&[u8]],
) -> Result<Connected, CheckOutcome> {
    let addr = util::resolve(&endpoint.host, endpoint.port).await.map_err(CheckOutcome::down)?;
    let tcp = TcpStream::connect(addr).await.map_err(|e| {
        CheckOutcome::down(format!("Connection to {}:{} failed: {}", addr.ip(), endpoint.port, util::tidy_io_error(&e)))
    })?;
    let _ = tcp.set_nodelay(true);
    if !endpoint.tls {
        return Ok(Connected { io: Box::new(tcp), addr, cert: None, tls_version: None });
    }
    match tls::upgrade_alpn(tcp, &endpoint.host, ignore_tls, alpn).await {
        Ok(stream) => {
            let (cert, version) = (tls::peer_cert(&stream), tls::version_label(&stream));
            Ok(Connected { io: Box::new(stream), addr, cert, tls_version: Some(version) })
        }
        Err(err) => Err(tls_failure(&err, &endpoint.host, addr, ignore_tls, timeout).await),
    }
}

/// The outcome for a failed handshake: certificate problems get their own
/// message (and the certificate's expiry), anything else is a plain failure.
pub async fn tls_failure(
    err: &std::io::Error,
    host: &str,
    addr: SocketAddr,
    ignore_tls: bool,
    timeout: std::time::Duration,
) -> CheckOutcome {
    tls_failure_with(err, ignore_tls, timeout, || tls::fetch_cert(host, addr)).await
}

/// [`tls_failure`] for services that upgrade mid-protocol: `fetch` replays
/// the prelude and returns the unverified certificate.
pub async fn tls_failure_with<F, Fut>(err: &std::io::Error, ignore_tls: bool, timeout: std::time::Duration, fetch: F) -> CheckOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<CertSummary>>,
{
    if !ignore_tls
        && let Some(outcome) = tls::explain_failure(err, timeout, fetch).await
    {
        return outcome;
    }
    CheckOutcome::down(format!("TLS handshake failed: {}", util::tidy_io_error(err)))
}

/// An `Up` outcome that also carries the certificate (if the connection was
/// encrypted) and appends the TLS version to the message.
pub fn up(ms: i64, message: String, cert: Option<&CertSummary>, tls_version: Option<&str>) -> CheckOutcome {
    let message = match tls_version {
        Some(v) => format!("{message} · {v}"),
        None => message,
    };
    let mut outcome = CheckOutcome::up(ms).with_message(message);
    outcome.expiry = cert.and_then(CertSummary::expiry_info);
    outcome
}

/// "Timed out after 10 s" for a check that ran past its limit.
pub fn timed_out(what: &str, timeout: std::time::Duration) -> CheckOutcome {
    CheckOutcome::down(format!("{what}: no answer within {}", util::format_secs(timeout)))
}
