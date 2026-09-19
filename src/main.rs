mod api;
mod config;
mod db;

use config::Config;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config = Config::from_env();
    std::fs::create_dir_all(&config.data_dir).expect("failed to create data dir");

    let pool = db::build_pool(&config.db_path());
    db::run_migrations(&pool).await;

    let app = api::build_router(pool);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .expect("failed to bind listener");
    tracing::info!("vigilant-vole listening on {}", config.bind_addr);
    axum::serve(listener, app).await.expect("server error");
}
