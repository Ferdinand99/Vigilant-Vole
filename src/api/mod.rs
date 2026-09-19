use askama::Template;
use axum::{
    Router,
    extract::{Path as AxumPath, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use deadpool_sqlite::Pool;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

#[derive(Clone)]
pub struct AppState {
    pub db: Pool,
}

pub fn build_router(db: Pool) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/healthz", get(healthz))
        .route("/static/{*path}", get(static_asset))
        .with_state(AppState { db })
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    monitor_count: i64,
}

struct HtmlTemplate<T>(T);

impl<T: Template> IntoResponse for HtmlTemplate<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {err}"),
            )
                .into_response(),
        }
    }
}

async fn dashboard(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.get().await.expect("failed to get db connection");
    let monitor_count: i64 = conn
        .interact(|conn| conn.query_row("SELECT COUNT(*) FROM monitors", [], |row| row.get(0)))
        .await
        .expect("db task panicked")
        .expect("count query failed");

    HtmlTemplate(DashboardTemplate { monitor_count })
}

async fn static_asset(AxumPath(path): AxumPath<String>) -> impl IntoResponse {
    match StaticAssets::get(&path) {
        Some(file) => {
            let content_type = guess_content_type(&path);
            ([(header::CONTENT_TYPE, content_type)], file.data).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn guess_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}
