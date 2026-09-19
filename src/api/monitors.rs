use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use serde::Deserialize;

use crate::db::models::{MonitorType, NewMonitor};
use crate::db::monitors as db_monitors;

use super::{AppState, HtmlTemplate};

#[derive(Deserialize)]
pub struct MonitorForm {
    pub name: String,
    pub monitor_type: String,
    pub target: String,
    pub interval_seconds: i64,
    pub timeout_seconds: i64,
    pub retries: i64,
}

#[derive(Template)]
#[template(path = "monitor_form.html")]
struct MonitorFormTemplate {
    heading: String,
    action: String,
    name: String,
    monitor_type: String,
    target: String,
    interval_seconds: i64,
    timeout_seconds: i64,
    retries: i64,
}

pub async fn new_form() -> impl IntoResponse {
    HtmlTemplate(MonitorFormTemplate {
        heading: "New Monitor".into(),
        action: "/monitors/new".into(),
        name: String::new(),
        monitor_type: "http".into(),
        target: String::new(),
        interval_seconds: 60,
        timeout_seconds: 10,
        retries: 0,
    })
}

pub async fn edit_form(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let conn = state.db.get().await.expect("failed to get db connection");
    let monitor = conn
        .interact(move |conn| db_monitors::get_monitor(conn, id))
        .await
        .expect("db task panicked")
        .expect("query failed");

    match monitor {
        Some(m) => HtmlTemplate(MonitorFormTemplate {
            heading: "Edit Monitor".into(),
            action: format!("/monitors/{}/edit", m.id),
            name: m.name,
            monitor_type: m.monitor_type.as_str().to_string(),
            target: m.target,
            interval_seconds: m.interval_seconds,
            timeout_seconds: m.timeout_seconds,
            retries: m.retries,
        })
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn form_to_new_monitor(form: MonitorForm) -> NewMonitor {
    NewMonitor {
        name: form.name,
        monitor_type: MonitorType::parse(&form.monitor_type).unwrap_or(MonitorType::Http),
        target: form.target,
        interval_seconds: form.interval_seconds.max(5),
        timeout_seconds: form.timeout_seconds.max(1),
        retries: form.retries.max(0),
    }
}

pub async fn create(State(state): State<AppState>, Form(form): Form<MonitorForm>) -> impl IntoResponse {
    let new_monitor = form_to_new_monitor(form);
    let conn = state.db.get().await.expect("failed to get db connection");
    let inserted = conn
        .interact(move |conn| {
            let id = db_monitors::insert_monitor(conn, &new_monitor)?;
            db_monitors::get_monitor(conn, id)
        })
        .await
        .expect("db task panicked")
        .expect("insert failed");

    if let Some(monitor) = inserted {
        state.scheduler.spawn(monitor).await;
    }

    Redirect::to("/")
}

pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<MonitorForm>,
) -> impl IntoResponse {
    let new_monitor = form_to_new_monitor(form);
    let conn = state.db.get().await.expect("failed to get db connection");
    let updated = conn
        .interact(move |conn| {
            db_monitors::update_monitor(conn, id, &new_monitor)?;
            db_monitors::get_monitor(conn, id)
        })
        .await
        .expect("db task panicked")
        .expect("update failed");

    if let Some(monitor) = updated {
        state.scheduler.respawn(monitor).await;
    }

    Redirect::to("/")
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let conn = state.db.get().await.expect("failed to get db connection");
    conn.interact(move |conn| db_monitors::delete_monitor(conn, id))
        .await
        .expect("db task panicked")
        .expect("delete failed");

    state.scheduler.stop(id).await;

    StatusCode::OK
}
