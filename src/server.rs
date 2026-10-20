//! Server startup: database, scheduler, background jobs, HTTP listener.

use std::sync::Arc;
use std::time::Duration;

use crate::api::{self, AppState};
use crate::config::Config;
use crate::monitor::{self, Scheduler};
use crate::{auth, db, discovery, maintenance};

/// User agent for outbound requests (notifications, RDAP, ...).
pub const USER_AGENT: &str = concat!("VigilantVole/", env!("CARGO_PKG_VERSION"));

/// Build the shared outbound HTTP client.
pub fn build_http_client() -> reqwest::Client {
    crate::tls_roots::http_client()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client")
}

/// Run the web server until it is stopped (Ctrl-C / SIGTERM).
pub async fn run(config: Config) {
    crate::tls_roots::init();
    std::fs::create_dir_all(&config.data_dir).expect("failed to create data dir");
    // Swap in a backup uploaded via Settings → Backup before the DB is opened.
    crate::config_io::apply_pending_restore(&config.data_dir);

    let pool = db::build_pool(&config.db_path());
    db::run_migrations(&pool, &config.data_dir.join("backups")).await;
    // Real-browser checks read their remote browser and store screenshots.
    crate::monitor::checkers::real_browser::install(pool.clone());

    let (update_tx, _) = tokio::sync::broadcast::channel(16);
    let http = build_http_client();
    let scheduler = Scheduler::new(pool.clone(), update_tx.clone(), http.clone());
    scheduler.start_all().await;

    let session_key = auth::load_or_create_key(&config.data_dir);
    let bind_addr = config.bind_addr.clone();

    let state = AppState {
        db: pool,
        scheduler,
        session_key,
        update_tx,
        http,
        config: Arc::new(config),
    };

    // Notifications sent by background jobs use the site language.
    crate::i18n::site_language(&state).await;

    // Background jobs; each owner implements its own `spawn`.
    monitor::retention::spawn(state.clone());
    discovery::spawn(state.clone());
    maintenance::spawn(state.clone());
    // GitOps: merge `VV_CONFIG_FILE` (if set) into the database on startup.
    crate::config_io::spawn_startup_import(state.clone());

    let scheduler = state.scheduler.clone();
    let app = api::build_router(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .expect("failed to bind listener");
    tracing::info!("vigilant-vole listening on {bind_addr}");
    // Connect info lets the login rate limiter key on the peer address.
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
    scheduler.shutdown().await;
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(%err, "failed to listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
