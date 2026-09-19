mod monitors;

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

use crate::db::models::MonitorWithStatus;
use crate::db::monitors as db_monitors;
use crate::monitor::Scheduler;

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

#[derive(Clone)]
pub struct AppState {
    pub db: Pool,
    pub scheduler: Scheduler,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/healthz", get(healthz))
        .route(
            "/monitors/new",
            get(monitors::new_form).post(monitors::create),
        )
        .route(
            "/monitors/{id}/edit",
            get(monitors::edit_form).post(monitors::update),
        )
        .route("/monitors/{id}", axum::routing::delete(monitors::delete))
        .route("/static/{*path}", get(static_asset))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    monitors: Vec<MonitorWithStatus>,
}

pub(crate) struct HtmlTemplate<T>(pub T);

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
    let monitors = conn
        .interact(|conn| db_monitors::list_monitors_with_status(conn))
        .await
        .expect("db task panicked")
        .expect("query failed");

    HtmlTemplate(DashboardTemplate { monitors })
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
