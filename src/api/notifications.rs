use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use serde::Deserialize;
use serde_json::json;

use crate::db::models::{ChannelKind, NotificationChannel};
use crate::db::notifications as db_notifications;

use super::{AppState, HtmlTemplate};

#[derive(Template)]
#[template(path = "notifications.html")]
struct NotificationsTemplate {
    channels: Vec<NotificationChannel>,
}

#[derive(Deserialize)]
pub struct ChannelForm {
    name: String,
    kind: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    bot_token: String,
    #[serde(default)]
    chat_id: String,
}

pub async fn list(State(state): State<AppState>) -> impl IntoResponse {
    let conn = state.db.get().await.expect("failed to get db connection");
    let channels = conn
        .interact(|conn| db_notifications::list_channels(conn))
        .await
        .expect("db task panicked")
        .expect("query failed");

    HtmlTemplate(NotificationsTemplate { channels })
}

pub async fn create(State(state): State<AppState>, Form(form): Form<ChannelForm>) -> impl IntoResponse {
    let kind = ChannelKind::parse(&form.kind).unwrap_or(ChannelKind::Webhook);
    let config = match kind {
        ChannelKind::Telegram => json!({ "bot_token": form.bot_token, "chat_id": form.chat_id }),
        _ => json!({ "url": form.url }),
    };
    let config_str = config.to_string();
    let name = form.name;

    let conn = state.db.get().await.expect("failed to get db connection");
    conn.interact(move |conn| db_notifications::insert_channel(conn, &name, kind, &config_str))
        .await
        .expect("db task panicked")
        .expect("insert failed");

    Redirect::to("/settings/notifications")
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let conn = state.db.get().await.expect("failed to get db connection");
    conn.interact(move |conn| db_notifications::delete_channel(conn, id))
        .await
        .expect("db task panicked")
        .expect("delete failed");

    StatusCode::OK
}
