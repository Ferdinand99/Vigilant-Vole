//! Trust roots for outbound TLS.
//!
//! Everything verifies certificates against the operating system's store. A
//! minimal container image (scratch, distroless without `ca-certificates`) has
//! none, and the TLS libraries then refuse to build a client at all ("No CA
//! certificates were loaded from the system"). In that case Vigilant Vole warns
//! once and uses the Mozilla root list bundled in the binary instead, so HTTPS
//! checks, notifications and agents keep working.

use std::sync::{Arc, OnceLock};

use rustls::RootCertStore;

/// Whether the system trust store holds at least one usable certificate
/// (loaded once; warns once when it doesn't).
pub fn system_store_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let loaded = rustls_native_certs::load_native_certs();
        let mut store = RootCertStore::empty();
        let (added, _) = store.add_parsable_certificates(loaded.certs);
        if added == 0 {
            tracing::warn!(
                "no CA certificates found on this system (is the `ca-certificates` package installed?); \
                 using the {} root certificates bundled with {} instead",
                webpki_root_certs::TLS_SERVER_ROOT_CERTS.len(),
                crate::kit::config::get().product
            );
        }
        added > 0
    })
}

/// The bundled Mozilla roots as a rustls store.
pub fn bundled_store() -> Arc<RootCertStore> {
    static STORE: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            let mut store = RootCertStore::empty();
            store.add_parsable_certificates(
                webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned(),
            );
            Arc::new(store)
        })
        .clone()
}

/// Call once at startup so a missing system store is reported up front.
pub fn init() {
    system_store_available();
}

/// `reqwest::Client::builder()` that also trusts the bundled roots when the
/// system store is empty. Use it for every outbound HTTP client.
pub fn http_client() -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    if system_store_available() {
        return builder;
    }
    trusting(
        builder,
        webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|der| der.as_ref()),
    )
}

/// `builder` that also trusts the DER certificates `roots`: how the bundled
/// roots are added when the system has none.
fn trusting<'a>(
    builder: reqwest::ClientBuilder,
    roots: impl IntoIterator<Item = &'a [u8]>,
) -> reqwest::ClientBuilder {
    builder.tls_certs_merge(
        roots
            .into_iter()
            .filter_map(|der| reqwest::Certificate::from_der(der).ok()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_list_is_usable() {
        let store = bundled_store();
        assert!(store.len() > 100, "only {} bundled roots", store.len());
        let converted = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .filter(|der| reqwest::Certificate::from_der(der).is_ok())
            .count();
        assert_eq!(converted, webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
    }

    #[test]
    fn the_client_builder_builds() {
        assert!(http_client().build().is_ok());
    }

    /// A CA and a server certificate for 127.0.0.1 signed by it, as DER.
    fn ca_and_server() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, SanType};
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(rcgen::DnType::CommonName, "Vole fallback test CA");
        let issuer = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut leaf = CertificateParams::new(Vec::<String>::new()).unwrap();
        leaf.distinguished_name
            .push(rcgen::DnType::CommonName, "127.0.0.1");
        leaf.subject_alt_names = vec![SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        ))];
        let cert = leaf.signed_by(&key, &issuer).unwrap();
        (
            issuer.der().to_vec(),
            cert.der().to_vec(),
            key.serialize_der(),
        )
    }

    /// Serves `HTTP/1.1 200 OK` over TLS with `cert`/`key`.
    async fn tls_server(cert: Vec<u8>, key: Vec<u8>) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.into()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(key.into()),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut buf = [0u8; 2048];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                        )
                        .await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        addr
    }

    /// The fallback adds roots with [`trusting`]; a real handshake shows a
    /// root added that way is what the client verifies against. (A server
    /// certificate chaining to one of the bundled public roots can't be made
    /// locally, so the test uses its own CA through the same code path.)
    #[tokio::test]
    async fn roots_added_for_the_fallback_are_trusted_in_a_real_handshake() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let (ca, cert, key) = ca_and_server();
        let addr = tls_server(cert, key).await;
        let url = format!("https://{addr}/");

        let untrusting = reqwest::Client::builder().no_proxy().build().unwrap();
        assert!(
            untrusting.get(&url).send().await.is_err(),
            "an unknown CA must be refused"
        );
        let client = trusting(reqwest::Client::builder().no_proxy(), [ca.as_slice()])
            .build()
            .unwrap();
        let resp = client.get(&url).send().await.expect("handshake");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "OK");
    }
}
