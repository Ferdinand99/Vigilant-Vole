//! TLS helpers: leaf-certificate parsing into [`ExpiryInfo`], cached rustls
//! client configs (verified with native roots / verification disabled) and
//! a plain-TCP-to-TLS upgrade for the non-reqwest checkers (WebSocket, TLS,
//! SMTP, Redis, ...).

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::util;
use crate::db::models::{CheckOutcome, ExpiryInfo, ExpiryKind};

/// Parsed facts about a server's leaf certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertSummary {
    pub subject: Option<String>,
    pub issuer: Option<String>,
    pub not_after_unix: i64,
}

impl CertSummary {
    pub fn expiry_info(&self) -> Option<ExpiryInfo> {
        Some(ExpiryInfo {
            kind: ExpiryKind::Cert,
            subject: self.subject.clone(),
            issuer: self.issuer.clone(),
            expires_at: util::format_utc(self.not_after_unix)?,
        })
    }

    pub fn is_expired(&self, now_unix: i64) -> bool {
        self.not_after_unix < now_unix
    }

    /// "certificate expired 3 days ago (2026-10-16)".
    pub fn expired_message(&self, now_unix: i64) -> String {
        let date = util::format_utc(self.not_after_unix)
            .map(|s| s[..10].to_string())
            .unwrap_or_default();
        format!(
            "TLS: certificate expired {} ({date})",
            util::relative_days(self.not_after_unix, now_unix)
        )
    }
}

/// Parse a DER-encoded certificate. Subject = CN (falls back to O);
/// issuer = O (falls back to CN).
pub fn parse_cert(der: &[u8]) -> Option<CertSummary> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    fn first<'a>(
        mut it: impl Iterator<Item = &'a x509_parser::x509::AttributeTypeAndValue<'a>>,
    ) -> Option<String> {
        it.find_map(|a| a.as_str().ok().map(str::to_string))
    }
    let subject = first(cert.subject().iter_common_name())
        .or_else(|| first(cert.subject().iter_organization()));
    let issuer = first(cert.issuer().iter_organization())
        .or_else(|| first(cert.issuer().iter_common_name()));
    Some(CertSummary {
        subject,
        issuer,
        not_after_unix: cert.validity().not_after.timestamp(),
    })
}

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .clone()
}

/// The OS trust store, loaded once. `None` if it holds no certificates.
fn native_roots() -> Option<Arc<RootCertStore>> {
    static ROOTS: OnceLock<Option<Arc<RootCertStore>>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let loaded = rustls_native_certs::load_native_certs();
            let mut roots = RootCertStore::empty();
            let (added, _) = roots.add_parsable_certificates(loaded.certs);
            if added == 0 {
                tracing::warn!("no native root certificates found for TLS checks");
                return None;
            }
            Some(Arc::new(roots))
        })
        .clone()
}

/// rustls config that verifies certificates against the OS trust store.
/// `None` if the store could not be loaded (callers fall back to the
/// library default).
pub fn verified_config() -> Option<Arc<ClientConfig>> {
    static CONFIG: OnceLock<Option<Arc<ClientConfig>>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = native_roots()?;
            ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .ok()
                .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
        })
        .clone()
}

/// rustls config that verifies the certificate chain against the OS trust
/// store but not the host name (`sslmode=verify-ca`).
fn ca_only_config() -> Option<Arc<ClientConfig>> {
    static CONFIG: OnceLock<Option<Arc<ClientConfig>>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let inner = WebPkiServerVerifier::builder_with_provider(native_roots()?, provider()).build().ok()?;
            ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .ok()
                .map(|b| Arc::new(b.dangerous().with_custom_certificate_verifier(Arc::new(CaOnly(inner))).with_no_client_auth()))
        })
        .clone()
}

/// How much of the server's certificate a handshake checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    /// Accept any certificate ("ignore TLS errors", `sslmode=prefer|require`).
    None,
    /// A trusted chain, any host name (`sslmode=verify-ca`).
    Ca,
    /// A trusted chain issued for the host name.
    Full,
}

impl Verify {
    /// The monitor option "ignore TLS errors" turns verification off.
    pub fn unless_ignored(self, ignore_tls: bool) -> Self {
        if ignore_tls { Verify::None } else { self }
    }
}

/// The chain check of [`WebPkiServerVerifier`] without its host name check
/// (which runs after a successful chain check, so only that error is waived).
#[derive(Debug)]
struct CaOnly(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for CaOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Err(rustls::Error::InvalidCertificate(CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. })) => {
                Ok(ServerCertVerified::assertion())
            }
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// rustls config that accepts any certificate (monitor option "ignore TLS
/// errors"). Signatures are still checked so the handshake is well-formed.
pub fn insecure_config() -> Option<Arc<ClientConfig>> {
    static CONFIG: OnceLock<Option<Arc<ClientConfig>>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let verifier = Arc::new(AcceptAnyCert(provider()));
            ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .ok()
                .map(|b| {
                    Arc::new(
                        b.dangerous()
                            .with_custom_certificate_verifier(verifier)
                            .with_no_client_auth(),
                    )
                })
        })
        .clone()
}

#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Run the TLS handshake over an established TCP connection. `host` is the
/// name to verify (and send as SNI); `ignore_tls` accepts any certificate.
pub async fn upgrade(tcp: TcpStream, host: &str, ignore_tls: bool) -> std::io::Result<TlsStream<TcpStream>> {
    upgrade_alpn(tcp, host, ignore_tls, &[]).await
}

/// [`upgrade`] offering ALPN protocols (`b"h2"` for gRPC).
pub async fn upgrade_alpn(tcp: TcpStream, host: &str, ignore_tls: bool, alpn: &[&[u8]]) -> std::io::Result<TlsStream<TcpStream>> {
    upgrade_with(tcp, host, Verify::Full.unless_ignored(ignore_tls), alpn).await
}

/// [`upgrade`] with an explicit verification level.
pub async fn upgrade_with(tcp: TcpStream, host: &str, verify: Verify, alpn: &[&[u8]]) -> std::io::Result<TlsStream<TcpStream>> {
    let config = match verify {
        Verify::None => insecure_config(),
        Verify::Ca => ca_only_config(),
        Verify::Full => verified_config(),
    };
    let mut config = config.ok_or_else(|| std::io::Error::other("TLS is unavailable (no trusted root certificates)"))?;
    if !alpn.is_empty() {
        let mut own = (*config).clone();
        own.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        config = Arc::new(own);
    }
    let name = ServerName::try_from(host.to_string())
        .map_err(|_| std::io::Error::other(format!("Invalid server name \"{host}\"")))?;
    TlsConnector::from(config).connect(name, tcp).await
}

/// The leaf certificate the peer presented.
pub fn peer_cert(stream: &TlsStream<TcpStream>) -> Option<CertSummary> {
    parse_cert(stream.get_ref().1.peer_certificates()?.first()?)
}

/// "TLS 1.3" for the negotiated protocol version.
pub fn version_label(stream: &TlsStream<TcpStream>) -> &'static str {
    match stream.get_ref().1.protocol_version() {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLS 1.3",
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLS 1.2",
        _ => "TLS",
    }
}

/// Fetch the certificate of a server that speaks TLS from the first byte,
/// without verifying it.
pub async fn fetch_cert(host: &str, addr: SocketAddr) -> Option<CertSummary> {
    let tcp = TcpStream::connect(addr).await.ok()?;
    peer_cert(&upgrade(tcp, host, true).await.ok()?)
}

/// Explain a failed verified handshake. Certificate problems are classified;
/// `fetch` gets the certificate again without verification (services that
/// upgrade mid-protocol replay their prelude first) so the outcome can carry
/// its expiry ("expired 3 days ago"). `None` if the error is not a
/// certificate problem (the caller reports it as a connection failure).
pub async fn explain_failure<F, Fut>(err: &std::io::Error, timeout: Duration, fetch: F) -> Option<CheckOutcome>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<CertSummary>>,
{
    let reason = classify_tls_error(&util::full_chain(err))?;
    let cert = tokio::time::timeout(timeout.min(Duration::from_secs(5)), fetch()).await.ok().flatten();
    let now = util::now_unix();
    let mut outcome = match &cert {
        Some(c) if c.is_expired(now) => CheckOutcome::down(c.expired_message(now)),
        _ => CheckOutcome::down(reason),
    };
    outcome.expiry = cert.as_ref().and_then(CertSummary::expiry_info);
    Some(outcome)
}

/// Map a TLS verification failure (from any layer's error text) to a clear
/// message. `None` if the text does not look like a certificate problem.
pub fn classify_tls_error(chain: &str) -> Option<&'static str> {
    let c = chain.to_ascii_lowercase();
    if c.contains("expired") {
        Some("TLS: certificate has expired")
    } else if c.contains("notvalidforname")
        || c.contains("not valid for name")
        || c.contains("hostname mismatch")
    {
        Some("TLS: certificate is not valid for this hostname")
    } else if c.contains("notvalidyet") || c.contains("not valid yet") {
        Some("TLS: certificate is not valid yet")
    } else if c.contains("revoked") {
        Some("TLS: certificate has been revoked")
    } else if c.contains("unknownissuer")
        || c.contains("unknown issuer")
        || c.contains("self-signed")
        || c.contains("self signed")
    {
        Some("TLS: certificate is not trusted (unknown issuer or self-signed)")
    } else if c.contains("invalid peer certificate")
        || c.contains("invalidcertificate")
        || c.contains("certificate verify")
    {
        Some("TLS: certificate verification failed")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_tls_errors() {
        assert_eq!(
            classify_tls_error("error sending request: invalid peer certificate: Expired"),
            Some("TLS: certificate has expired")
        );
        assert_eq!(
            classify_tls_error("invalid peer certificate: UnknownIssuer"),
            Some("TLS: certificate is not trusted (unknown issuer or self-signed)")
        );
        assert_eq!(
            classify_tls_error("invalid peer certificate: NotValidForName"),
            Some("TLS: certificate is not valid for this hostname")
        );
        assert_eq!(classify_tls_error("connection refused"), None);
    }

    #[test]
    fn configs_build() {
        assert!(insecure_config().is_some());
        assert_eq!(ca_only_config().is_some(), verified_config().is_some());
    }

    #[test]
    fn ignoring_tls_errors_turns_verification_off() {
        assert_eq!(Verify::Full.unless_ignored(true), Verify::None);
        assert_eq!(Verify::Ca.unless_ignored(false), Verify::Ca);
    }

    #[test]
    fn expired_message_mentions_age() {
        let s = CertSummary {
            subject: None,
            issuer: None,
            not_after_unix: 0,
        };
        assert_eq!(
            s.expired_message(3 * 86_400 + 1),
            "TLS: certificate expired 3 days ago (1970-01-01)"
        );
        assert!(s.is_expired(1));
    }

    #[test]
    fn parse_cert_rejects_garbage() {
        assert!(parse_cert(b"not a certificate").is_none());
    }
}
