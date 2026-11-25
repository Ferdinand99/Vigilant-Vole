//! `cargo run -p kit-demo`: Kit Demo on http://127.0.0.1:3101 (`KD_PORT`,
//! `KD_HOST`), data in `./kit-demo-data` (`KD_DATA_DIR`). `KD_SHELL=sidebar`
//! shows it in the side-menu shell instead of the top bar.

use std::net::{IpAddr, SocketAddr};

#[tokio::main]
async fn main() {
    let dir = std::env::var("KD_DATA_DIR").unwrap_or_else(|_| "kit-demo-data".into());
    let port: u16 = std::env::var("KD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3101);
    let host: IpAddr = std::env::var("KD_HOST")
        .ok()
        .and_then(|h| h.parse().ok())
        .unwrap_or_else(|| IpAddr::from([127, 0, 0, 1]));
    let config = match std::env::var("KD_SHELL").as_deref() {
        Ok("sidebar") => &kit_demo::KIT_SIDEBAR,
        _ => &kit_demo::KIT,
    };
    let demo = kit_demo::start_with(std::path::Path::new(&dir), config)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let app = kit_demo::router(&demo);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((host, port)))
        .await
        .expect("bind");
    println!("Kit Demo on http://{host}:{port}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("serve");
}
