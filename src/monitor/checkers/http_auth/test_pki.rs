//! Throwaway certificates for tests (rcgen), and a TLS server that demands a
//! client certificate (tokio-rustls). Dev-dependencies only.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, SanType,
};
use rustls::RootCertStore;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;

/// A CA plus a server certificate (for 127.0.0.1) and a client certificate
/// signed by it. All PEM except the server's DER material. The PEM text uses
/// LF line endings on every OS (rcgen writes CRLF on Windows), the way the
/// form stores it, so tests can compare stored values with these.
pub struct Pki {
    pub ca_pem: String,
    pub client_cert_pem: String,
    pub client_key_pem: String,
    server_cert: rustls::pki_types::CertificateDer<'static>,
    server_key: Vec<u8>,
    ca_der: rustls::pki_types::CertificateDer<'static>,
}

fn ca(name: &str) -> (CertifiedIssuer<'static, KeyPair>, String) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    let issuer = CertifiedIssuer::self_signed(params, key).unwrap();
    let pem = lf(issuer.pem());
    (issuer, pem)
}

/// PEM text with LF line endings.
fn lf(pem: String) -> String {
    pem.replace("\r\n", "\n")
}

fn leaf(
    issuer: &CertifiedIssuer<'static, KeyPair>,
    cn: &str,
    usage: ExtendedKeyUsagePurpose,
    ip: bool,
) -> (rcgen::Certificate, KeyPair) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, cn);
    params.extended_key_usages = vec![usage];
    if ip {
        params.subject_alt_names = vec![SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST))];
    }
    let cert = params.signed_by(&key, issuer).unwrap();
    (cert, key)
}

impl Pki {
    pub fn new() -> Self {
        let (issuer, ca_pem) = ca("Vole test CA");
        let (server, server_key) = leaf(
            &issuer,
            "localhost",
            ExtendedKeyUsagePurpose::ServerAuth,
            true,
        );
        let (client, client_key) = leaf(
            &issuer,
            "vole-client",
            ExtendedKeyUsagePurpose::ClientAuth,
            false,
        );
        Self {
            ca_pem,
            client_cert_pem: lf(client.pem()),
            client_key_pem: lf(client_key.serialize_pem()),
            server_cert: server.der().clone(),
            server_key: server_key.serialize_der(),
            ca_der: issuer.der().clone(),
        }
    }
}

/// A client certificate and key issued by some other CA (the server must refuse it).
pub fn stranger_client() -> (String, String) {
    let (issuer, _) = ca("Other CA");
    let (cert, key) = leaf(
        &issuer,
        "stranger",
        ExtendedKeyUsagePurpose::ClientAuth,
        false,
    );
    (cert.pem(), key.serialize_pem())
}

/// A fresh private key unrelated to any certificate above.
pub fn unrelated_key_pem() -> String {
    lf(KeyPair::generate().unwrap().serialize_pem())
}

/// Serves `HTTP/1.1 200 OK` over TLS, but only to clients presenting a
/// certificate issued by the test CA. Returns the listening address.
pub async fn serve_requiring_client_cert(pki: &Pki) -> SocketAddr {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = RootCertStore::empty();
    roots.add(pki.ca_der.clone()).unwrap();
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![pki.server_cert.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pki.server_key.clone())),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
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

#[test]
fn pem_text_uses_lf_on_every_os() {
    let pki = Pki::new();
    for pem in [
        &pki.ca_pem,
        &pki.client_cert_pem,
        &pki.client_key_pem,
        &unrelated_key_pem(),
    ] {
        assert!(!pem.contains('\r'), "{pem:?}");
    }
}
