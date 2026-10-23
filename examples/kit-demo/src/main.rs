//! `cargo run -p kit-demo`: Kit Demo on http://127.0.0.1:3101 (`KD_PORT`),
//! data in `./kit-demo-data` (`KD_DATA_DIR`).

use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    let dir = std::env::var("KD_DATA_DIR").unwrap_or_else(|_| "kit-demo-data".into());
    let port: u16 = std::env::var("KD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3101);
    let demo = kit_demo::start(std::path::Path::new(&dir))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let app = kit_demo::router(&demo);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .expect("bind");
    println!("Kit Demo on http://127.0.0.1:{port}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("serve");
}
