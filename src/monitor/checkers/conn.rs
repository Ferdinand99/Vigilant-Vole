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

/// A failed [`connect`]: the complete Down outcome (boxed, it is large).
/// Converts into [`CheckOutcome`], so `?` works in probes that fail with one.
#[derive(Debug)]
pub struct Failed(pub Box<CheckOutcome>);

impl From<Failed> for CheckOutcome {
    fn from(failed: Failed) -> Self {
        *failed.0
    }
}

impl From<CheckOutcome> for Failed {
    fn from(outcome: CheckOutcome) -> Self {
        Failed(Box::new(outcome))
    }
}

/// Resolve and connect to `endpoint`; wrap in TLS when its scheme asks for it.
/// The caller applies the monitor's timeout around the whole check.
pub async fn connect(endpoint: &Endpoint, ignore_tls: bool, timeout: std::time::Duration) -> Result<Connected, Failed> {
    connect_alpn(endpoint, ignore_tls, timeout, &[]).await
}

/// Resolve `host:port` and open a TCP connection to the first address that
/// accepts (see [`util::connect_any`]). `Err` is the Down message.
pub async fn tcp(host: &str, port: u16) -> Result<(TcpStream, SocketAddr), String> {
    let addrs = util::resolve_all(host, port).await?;
    let (tcp, addr) = util::connect_any(&addrs)
        .await
        .map_err(|(addr, e)| format!("Connection to {}:{port} failed: {}", addr.ip(), util::tidy_io_error(&e)))?;
    let _ = tcp.set_nodelay(true);
    Ok((tcp, addr))
}

/// [`connect`] offering ALPN protocols during the TLS handshake.
pub async fn connect_alpn(
    endpoint: &Endpoint,
    ignore_tls: bool,
    timeout: std::time::Duration,
    alpn: &[&[u8]],
) -> Result<Connected, Failed> {
    let (tcp, addr) = tcp(&endpoint.host, endpoint.port).await.map_err(CheckOutcome::down)?;
    if !endpoint.tls {
        return Ok(Connected { io: Box::new(tcp), addr, cert: None, tls_version: None });
    }
    match tls::upgrade_alpn(tcp, &endpoint.host, ignore_tls, alpn).await {
        Ok(stream) => {
            let (cert, version) = (tls::peer_cert(&stream), tls::version_label(&stream));
            Ok(Connected { io: Box::new(stream), addr, cert, tls_version: Some(version) })
        }
        Err(err) => Err(tls_failure(&err, &endpoint.host, addr, ignore_tls, timeout).await.into()),
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

/// `?sslmode=` of the database targets, with libpq's meaning (MySQL's
/// `--ssl-mode` names are accepted too). Only the `verify-*` modes check the
/// certificate; the others encrypt when they can, like the clients do.
/// Ordered from least to most protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SslMode {
    /// No TLS.
    Disable,
    /// TLS when the server offers it, certificate not checked (the default).
    Prefer,
    /// TLS or fail, certificate not checked.
    Require,
    /// TLS or fail, trusted chain (any host name).
    VerifyCa,
    /// TLS or fail, trusted chain issued for the host name.
    VerifyFull,
}

impl SslMode {
    pub fn of(endpoint: &Endpoint) -> Result<Self, String> {
        let raw = endpoint.query("sslmode").unwrap_or("prefer");
        Ok(match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "disable" | "disabled" => SslMode::Disable,
            "prefer" | "preferred" => SslMode::Prefer,
            "require" | "required" => SslMode::Require,
            "verify-ca" => SslMode::VerifyCa,
            "verify-full" | "verify-identity" => SslMode::VerifyFull,
            _ => {
                return Err(format!(
                    "Unsupported sslmode \"{}\" (use disable, prefer, require, verify-ca or verify-full)",
                    util::truncate(raw, 20)
                ));
            }
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        }
    }

    /// The connection fails when the server can't do TLS.
    pub fn requires_tls(self) -> bool {
        matches!(self, SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull)
    }

    /// How the certificate is checked; "ignore TLS errors" turns it off.
    pub fn verify(self, ignore_tls: bool) -> tls::Verify {
        match self {
            SslMode::VerifyCa => tls::Verify::Ca,
            SslMode::VerifyFull => tls::Verify::Full,
            _ => tls::Verify::None,
        }
        .unless_ignored(ignore_tls)
    }

    /// "The server doesn't support TLS (sslmode=require)".
    pub fn no_tls_message(self) -> String {
        format!("The server doesn't support TLS (sslmode={})", self.as_str())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::MonitorType;
    use crate::monitor::checkers::endpoint;
    use std::time::Duration;

    #[tokio::test]
    async fn localhost_targets_reach_ipv4_only_services() {
        // `localhost` is `::1` first on Windows and in Alpine containers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ep = endpoint::parse(MonitorType::Redis, &format!("redis://localhost:{port}")).unwrap();
        let connected = connect(&ep, false, Duration::from_secs(5)).await.map_err(CheckOutcome::from).unwrap();
        assert_eq!(connected.addr, SocketAddr::from(([127, 0, 0, 1], port)));
    }

    #[test]
    fn sslmode_follows_libpq_and_accepts_mysql_names() {
        let mode = |t: &str| SslMode::of(&endpoint::parse(MonitorType::Postgres, t).unwrap());
        assert_eq!(mode("db"), Ok(SslMode::Prefer));
        assert_eq!(mode("db?sslmode=VERIFY_IDENTITY"), Ok(SslMode::VerifyFull));
        assert_eq!(mode("db?sslmode=required"), Ok(SslMode::Require));
        assert!(mode("db?sslmode=allow").unwrap_err().starts_with("Unsupported sslmode"));
        // Only the verify modes check the certificate, and never when TLS errors are ignored.
        assert_eq!(SslMode::Prefer.verify(false), tls::Verify::None);
        assert_eq!(SslMode::Require.verify(false), tls::Verify::None);
        assert_eq!(SslMode::VerifyCa.verify(false), tls::Verify::Ca);
        assert_eq!(SslMode::VerifyFull.verify(true), tls::Verify::None);
        assert!(!SslMode::Prefer.requires_tls() && SslMode::VerifyCa.requires_tls());
    }

    #[tokio::test]
    async fn the_last_failure_is_reported() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let err = tcp("127.0.0.1", port).await.unwrap_err();
        assert_eq!(err, format!("Connection to 127.0.0.1:{port} failed: Connection refused"));
    }
}
