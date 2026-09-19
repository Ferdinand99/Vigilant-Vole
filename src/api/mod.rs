mod auth;
mod events;
mod monitors;
mod notifications;

use askama::Template;
use axum::{
    Router,
    extract::{FromRef, Path as AxumPath, State},
    http::{StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use axum_extra::extract::cookie::Key;
use deadpool_sqlite::Pool;
use rust_embed::Embed;
use tokio::sync::broadcast;

use crate::db::models::{HeartbeatStatus, MonitorWithStatus};
use crate::db::monitors as db_monitors;
use crate::monitor::Scheduler;

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

#[derive(Clone)]
pub struct AppState {
    pub db: Pool,
    pub scheduler: Scheduler,
    pub session_key: Key,
    pub update_tx: broadcast::Sender<()>,
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Key {
        state.session_key.clone()
    }
}

pub fn build_router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/", get(dashboard))
        .route(
            "/monitors/new",
            get(monitors::new_form).post(monitors::create),
        )
        .route(
            "/monitors/{id}/edit",
            get(monitors::edit_form).post(monitors::update),
        )
        .route("/monitors/{id}", axum::routing::delete(monitors::delete))
        .route(
            "/settings/notifications",
            get(notifications::list).post(notifications::create),
        )
        .route(
            "/settings/notifications/{id}",
            axum::routing::delete(notifications::delete),
        )
        .route("/logout", axum::routing::post(auth::logout))
        .route("/events", get(events::stream))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    let public = Router::new()
        .route("/healthz", get(healthz))
        .route("/setup", get(auth::setup_form).post(auth::setup_submit))
        .route("/login", get(auth::login_form).post(auth::login_submit))
        .route("/static/{*path}", get(static_asset));

    protected.merge(public).with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    data: DashboardData,
}

/// Shared by the full dashboard page and the SSE stream so both render the
/// exact same `monitor_list.html` fragment from the exact same query.
pub(crate) struct DashboardData {
    pub monitors: Vec<MonitorWithStatus>,
    pub total: usize,
    pub up: usize,
    pub down: usize,
    pub pending: usize,
}

pub(crate) async fn load_dashboard_data(db: &Pool) -> DashboardData {
    let conn = db.get().await.expect("failed to get db connection");
    let monitors = conn
        .interact(|conn| db_monitors::list_monitors_with_status(conn))
        .await
        .expect("db task panicked")
        .expect("query failed");

    let total = monitors.len();
    let up = monitors
        .iter()
        .filter(|m| m.status == Some(HeartbeatStatus::Up))
        .count();
    let down = monitors
        .iter()
        .filter(|m| m.status == Some(HeartbeatStatus::Down))
        .count();
    let pending = total - up - down;

    DashboardData {
        monitors,
        total,
        up,
        down,
        pending,
    }
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
    let data = load_dashboard_data(&state.db).await;
    HtmlTemplate(DashboardTemplate { data })
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
