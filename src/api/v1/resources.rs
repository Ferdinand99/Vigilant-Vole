//! `/api/v1/tags/**`, `/api/v1/notification-channels/**` and
//! `/api/v1/notification-providers`.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Body, Id, V1Error, V1Result, created, json_ok, no_content};
use crate::api::AppState;
use crate::auth::{CurrentUser, Role};
use crate::db::models::NotificationChannel;
use crate::db::notifications as db_notifications;
use crate::db::tags as db_tags;
use crate::notification::{self, FieldKind};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tags", get(list_tags).post(create_tag))
        .route(
            "/tags/{id}",
            get(show_tag)
                .put(update_tag)
                .patch(update_tag)
                .delete(delete_tag),
        )
        .route("/notification-providers", get(providers))
        .route(
            "/notification-channels",
            get(list_channels).post(create_channel),
        )
        .route(
            "/notification-channels/{id}",
            get(show_channel)
                .put(update_channel)
                .patch(update_channel)
                .delete(delete_channel),
        )
        .route("/notification-channels/{id}/test", post(test_channel))
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

fn tag_json(t: &db_tags::Tag, monitors: Option<i64>) -> Value {
    let mut v = json!({ "id": t.id, "name": t.name, "color": t.color });
    if let Some(n) = monitors {
        v["monitors"] = json!(n);
    }
    v
}

async fn list_tags(
    State(state): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
) -> V1Result<Response> {
    let tags = state
        .with_conn(|c| db_tags::list_tags_with_counts(c))
        .await?;
    let data: Vec<Value> = tags
        .iter()
        .map(|t| tag_json(&t.tag, Some(t.monitors)))
        .collect();
    Ok(json_ok(
        &headers,
        &json!({ "data": data, "next_cursor": null }),
    ))
}

async fn show_tag(
    State(state): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    let tag = state
        .with_conn(move |c| db_tags::get_tag(c, id))
        .await?
        .ok_or_else(V1Error::not_found)?;
    Ok(json_ok(&headers, &tag_json(&tag, None)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TagIn {
    name: Option<String>,
    color: Option<String>,
    #[serde(default, rename = "id")]
    _id: Option<i64>,
    #[serde(default, rename = "monitors")]
    _monitors: Option<i64>,
}

fn clean_tag_name(name: &str) -> V1Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(V1Error::field("name", "Give the tag a name."));
    }
    if name.chars().count() > db_tags::MAX_NAME_LEN {
        return Err(V1Error::field(
            "name",
            format!("Keep tag names under {} characters.", db_tags::MAX_NAME_LEN),
        ));
    }
    Ok(name.to_string())
}

fn check_color(color: &str) -> V1Result<String> {
    let c = color.trim().to_ascii_lowercase();
    if db_tags::PALETTE.contains(&c.as_str()) || c.starts_with('#') {
        Ok(db_tags::normalize_color(&c).to_string())
    } else {
        Err(V1Error::field(
            "color",
            format!("Use one of: {}.", db_tags::PALETTE.join(", ")),
        ))
    }
}

async fn create_tag(
    State(state): State<AppState>,
    user: CurrentUser,
    Body(input): Body<TagIn>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let name = clean_tag_name(input.name.as_deref().unwrap_or_default())?;
    let color = check_color(input.color.as_deref().unwrap_or(db_tags::DEFAULT_COLOR))?;
    let tag = state
        .with_conn(move |c| {
            if db_tags::name_taken(c, &name, None)? {
                return Ok(None);
            }
            let id = db_tags::insert_tag(c, &name, &color)?;
            db_tags::get_tag(c, id)
        })
        .await?
        .ok_or_else(|| V1Error::conflict("A tag with this name already exists."))?;
    state.notify_update();
    Ok(created(
        format!("/api/v1/tags/{}", tag.id),
        &tag_json(&tag, None),
    ))
}

async fn update_tag(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
    Body(input): Body<TagIn>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let current = state
        .with_conn(move |c| db_tags::get_tag(c, id))
        .await?
        .ok_or_else(V1Error::not_found)?;
    let name = match &input.name {
        Some(n) => clean_tag_name(n)?,
        None => current.name.clone(),
    };
    let color = match &input.color {
        Some(c) => check_color(c)?,
        None => current.color.clone(),
    };
    let tag = state
        .with_conn(move |c| {
            if db_tags::name_taken(c, &name, Some(id))? {
                return Ok(None);
            }
            db_tags::update_tag(c, id, &name, &color)?;
            db_tags::get_tag(c, id)
        })
        .await?
        .ok_or_else(|| V1Error::conflict("A tag with this name already exists."))?;
    state.notify_update();
    Ok(json_ok(&HeaderMap::new(), &tag_json(&tag, None)))
}

async fn delete_tag(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    if !state.with_conn(move |c| db_tags::delete_tag(c, id)).await? {
        return Err(V1Error::not_found());
    }
    state.notify_update();
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// Notification channels
// ---------------------------------------------------------------------------

/// A channel as JSON — config always redacted.
fn channel_json(c: &NotificationChannel, links: Option<i64>) -> Value {
    let mut v = json!({
        "id": c.id,
        "name": c.name,
        "kind": c.kind,
        "active": c.active,
        "is_default": c.is_default,
        "created_at": c.created_at,
        "config": notification::redacted_config(c),
    });
    if let Some(n) = links {
        v["monitors"] = json!(n);
    }
    v
}

fn field_kind(kind: FieldKind) -> Value {
    match kind {
        FieldKind::Text => json!("text"),
        FieldKind::Url => json!("url"),
        FieldKind::Secret => json!("secret"),
        FieldKind::Number => json!("number"),
        FieldKind::Bool => json!("bool"),
        FieldKind::Textarea => json!("textarea"),
        FieldKind::KeyValue => json!("key_value"),
        FieldKind::Select(options) => {
            json!({ "select": options.iter().map(|(v, _)| *v).collect::<Vec<_>>() })
        }
    }
}

async fn providers(_user: CurrentUser, headers: HeaderMap) -> Response {
    let data: Vec<Value> = notification::PROVIDERS
        .iter()
        .map(|p| {
            json!({
                "kind": p.kind,
                "label": p.label,
                "description": p.description,
                "fields": p.fields.iter().map(|f| json!({
                    "key": f.key,
                    "label": f.label,
                    "type": field_kind(f.kind),
                    "required": f.required,
                    "secret": f.secret,
                    "default": if f.default.is_empty() { Value::Null } else { json!(f.default_value()) },
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json_ok(&headers, &json!({ "data": data, "next_cursor": null }))
}

async fn list_channels(
    State(state): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
) -> V1Result<Response> {
    let rows = state
        .with_conn(|c| db_notifications::list_channels_with_counts(c))
        .await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|(c, n)| channel_json(c, Some(*n)))
        .collect();
    Ok(json_ok(
        &headers,
        &json!({ "data": data, "next_cursor": null }),
    ))
}

async fn load_channel(state: &AppState, id: i64) -> V1Result<NotificationChannel> {
    state
        .with_conn(move |c| db_notifications::get_channel(c, id))
        .await?
        .ok_or_else(V1Error::not_found)
}

async fn show_channel(
    State(state): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    let channel = load_channel(&state, id).await?;
    let links = state
        .with_conn(move |c| db_notifications::count_links(c, id))
        .await?;
    Ok(json_ok(&headers, &channel_json(&channel, Some(links))))
}

#[derive(Debug, Deserialize)]
struct ChannelIn {
    name: Option<String>,
    kind: Option<String>,
    config: Option<Value>,
    active: Option<bool>,
    is_default: Option<bool>,
    /// Also link every existing monitor.
    #[serde(default)]
    apply_existing: bool,
}

fn channel_errors(errors: Vec<(String, String)>) -> V1Error {
    V1Error::fields(errors)
}

async fn create_channel(
    State(state): State<AppState>,
    user: CurrentUser,
    Body(input): Body<ChannelIn>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let name = input.name.unwrap_or_default().trim().to_string();
    let kind = input.kind.unwrap_or_default();
    let config = input.config.unwrap_or_else(|| json!({}));
    let config =
        crate::config_io::validate_channel(&name, &kind, &config, None).map_err(channel_errors)?;
    let is_default = input.is_default.unwrap_or(false);
    let active = input.active.unwrap_or(true);
    let apply = input.apply_existing;
    let saved_kind = kind.clone();
    let channel = state
        .with_conn(move |c| {
            let tx = c.transaction()?;
            let id = db_notifications::insert_channel(
                &tx,
                &name,
                &saved_kind,
                &config.to_string(),
                is_default,
            )?;
            if !active {
                db_notifications::set_active(&tx, id, false)?;
            }
            if apply && !is_default {
                db_notifications::apply_to_all(&tx, id)?;
            }
            let ch = db_notifications::get_channel(&tx, id)?;
            tx.commit()?;
            Ok(ch)
        })
        .await?
        .ok_or_else(|| V1Error::internal("channel vanished after insert"))?;
    tracing::info!(user = %user.username, kind = %kind, "notification channel created via API");
    Ok(created(
        format!("/api/v1/notification-channels/{}", channel.id),
        &channel_json(&channel, None),
    ))
}

async fn update_channel(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
    Body(input): Body<ChannelIn>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let channel = load_channel(&state, id).await?;
    if let Some(kind) = &input.kind
        && *kind != channel.kind
    {
        return Err(V1Error::field(
            "kind",
            "A channel’s kind can’t change. Create a new channel instead.",
        ));
    }
    let stored: Value = serde_json::from_str(&channel.config).unwrap_or_else(|_| json!({}));
    let name = input
        .name
        .clone()
        .unwrap_or_else(|| channel.name.clone())
        .trim()
        .to_string();
    // Secrets left out or masked keep the stored value (`schema::merge_secrets`).
    let config = input.config.clone().unwrap_or_else(|| stored.clone());
    let config = crate::config_io::validate_channel(&name, &channel.kind, &config, Some(&stored))
        .map_err(channel_errors)?;
    let active = input.active.unwrap_or(channel.active);
    let is_default = input.is_default.unwrap_or(channel.is_default);
    let apply = input.apply_existing;
    let updated = state
        .with_conn(move |c| {
            let tx = c.transaction()?;
            db_notifications::update_channel(&tx, id, &name, &config.to_string(), is_default)?;
            db_notifications::set_active(&tx, id, active)?;
            if apply {
                db_notifications::apply_to_all(&tx, id)?;
            }
            let ch = db_notifications::get_channel(&tx, id)?;
            tx.commit()?;
            Ok(ch)
        })
        .await?
        .ok_or_else(V1Error::not_found)?;
    tracing::info!(user = %user.username, channel_id = id, "notification channel updated via API");
    Ok(json_ok(&HeaderMap::new(), &channel_json(&updated, None)))
}

async fn delete_channel(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    load_channel(&state, id).await?;
    state
        .with_conn(move |c| db_notifications::delete_channel(c, id))
        .await?;
    tracing::info!(user = %user.username, channel_id = id, "notification channel deleted via API");
    Ok(no_content())
}

async fn test_channel(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let channel = load_channel(&state, id).await?;
    let settings = state.settings().await?;
    let event = notification::test_event(&settings.site_name, settings.base_url.as_deref());
    match notification::send(&state.http, &channel, &event).await {
        Ok(()) => Ok(json_ok(&HeaderMap::new(), &json!({ "ok": true }))),
        Err(err) => Ok(V1Error::new(
            StatusCode::BAD_GATEWAY,
            "delivery_failed",
            format!("Couldn’t send the test: {err}"),
        )
        .into_response()),
    }
}
