//! TLS helpers: leaf-certificate parsing into [`ExpiryInfo`] and cached rustls
//! client configs (verified with native roots / verification disabled) for
//! the non-reqwest checkers (WebSocket).

use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use super::util;
use crate::db::models::{ExpiryInfo, ExpiryKind};

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

/// rustls config that verifies certificates against the OS trust store.
/// `None` if the store could not be loaded (callers fall back to the
/// library default).
pub fn verified_config() -> Option<Arc<ClientConfig>> {
    static CONFIG: OnceLock<Option<Arc<ClientConfig>>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let loaded = rustls_native_certs::load_native_certs();
            let mut roots = RootCertStore::empty();
            let (added, _) = roots.add_parsable_certificates(loaded.certs);
            if added == 0 {
                tracing::warn!("no native root certificates found for WebSocket TLS");
                return None;
            }
            ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .ok()
                .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
        })
        .clone()
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
