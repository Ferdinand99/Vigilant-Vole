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
                 using the {} root certificates bundled with Vigilant Vole instead",
                webpki_root_certs::TLS_SERVER_ROOT_CERTS.len()
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
    let certs = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| reqwest::Certificate::from_der(der).ok());
    builder.tls_certs_merge(certs)
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
}
