//! Webhooks of status page subscribers go straight to the address that was
//! checked, never through a proxy from the environment (which could reach
//! internal services the address check keeps out).
//!
//! Its own test binary: it sets proxy variables for the whole process.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Accepts one connection, answers 200 and reports what it was sent.
async fn listener() -> (SocketAddr, tokio::task::JoinHandle<String>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut sock, _) = l.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await;
        String::from_utf8_lossy(&buf[..n]).to_string()
    });
    (addr, task)
}

#[tokio::test]
async fn subscriber_webhooks_ignore_proxy_variables() {
    let (proxy, proxy_got) = listener().await;
    let (target, target_got) = listener().await;
    let proxy_url = format!("http://{proxy}");
    // SAFETY: this test binary has a single test, so nothing else reads the
    // environment concurrently.
    unsafe {
        for var in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::set_var(var, &proxy_url);
        }
        for var in ["NO_PROXY", "no_proxy"] {
            std::env::remove_var(var);
        }
    }
    let url = url::Url::parse(&format!("http://hooks.example.test:{}/in", target.port())).unwrap();
    vigilant_vole::subscribers::post_pinned(
        url,
        &[target],
        &serde_json::json!({"type": "incident"}),
        Duration::from_secs(5),
    )
    .await
    .expect("delivered");
    let request = target_got.await.unwrap();
    assert!(request.starts_with("POST /in HTTP/1.1"), "{request}");
    assert!(!proxy_got.is_finished(), "the proxy was used");
    proxy_got.abort();
}

/// Test binaries have no `main`: install the kit's configuration before any
/// test runs, so no test can forget it.
#[ctor::ctor(unsafe)]
fn install_kit_config() {
    vigilant_vole::product::install();
}
