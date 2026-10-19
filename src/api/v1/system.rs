//! `/api/v1/me`, `/api/v1/health`, `/api/v1/agents`, `/api/v1/settings`.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};
use serde_json::{Value, json};

use super::{Body, V1Error, V1Result, json_ok};
use crate::api::AppState;
use crate::auth::{AuthMethod, CurrentUser};
use crate::config_io::{self, Document, Mode, SettingsDoc};
use crate::db::agents as db_agents;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/me", get(me))
        .route("/health", get(health))
        .route("/agents", get(agents))
        .route("/settings", get(get_settings).patch(patch_settings).put(patch_settings))
}

async fn me(user: CurrentUser, method: Option<Extension<AuthMethod>>) -> Response {
    let (auth, scope) = match method.map(|m| m.0) {
        Some(AuthMethod::Token { scope, .. }) => ("token", Some(scope.as_str())),
        _ => ("session", None),
    };
    json_ok(
        &HeaderMap::new(),
        &json!({
            "id": user.id,
            "username": user.username,
            "display_name": user.display_name,
            "role": user.role.as_str(),
            "auth": auth,
            "token_scope": scope,
            "can_edit": user.can_edit(),
            "is_admin": user.is_admin(),
        }),
    )
}

async fn health(State(state): State<AppState>, _user: CurrentUser, headers: HeaderMap) -> V1Result<Response> {
    let counts = state
        .with_conn(|c| {
            let monitors = crate::db::monitors::list_monitors_with_status(c)?;
            let mut by_status = std::collections::BTreeMap::<&str, i64>::new();
            for row in &monitors {
                let key = if !row.monitor.active {
                    "paused"
                } else {
                    row.status.map_or("unknown", |s| s.as_str())
                };
                *by_status.entry(key).or_default() += 1;
            }
            let agents: i64 = c.query_row("SELECT COUNT(*) FROM agents", [], |r| r.get(0))?;
            let pages: i64 = c.query_row("SELECT COUNT(*) FROM status_pages", [], |r| r.get(0))?;
            Ok(json!({
                "monitors": { "total": monitors.len(), "by_status": by_status },
                "status_pages": pages,
                "agents": agents,
            }))
        })
        .await?;
    let mut v = json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": super::STARTED.elapsed().as_secs(),
    });
    if let (Some(obj), Value::Object(extra)) = (v.as_object_mut(), counts) {
        obj.extend(extra);
    }
    Ok(json_ok(&headers, &v))
}

async fn agents(State(state): State<AppState>, user: CurrentUser, headers: HeaderMap) -> V1Result<Response> {
    user.require_admin()?;
    let data = state.with_conn(|c| db_agents::list_agents(c)).await?;
    Ok(json_ok(&headers, &json!({ "data": data, "next_cursor": null })))
}

async fn get_settings(State(state): State<AppState>, user: CurrentUser, headers: HeaderMap) -> V1Result<Response> {
    user.require_admin()?;
    let s = state.settings().await?;
    Ok(json_ok(&headers, &s))
}

/// Partial update; same rules as the general settings page (via the config importer).
async fn patch_settings(State(state): State<AppState>, user: CurrentUser, Body(input): Body<SettingsDoc>) -> V1Result<Response> {
    user.require_admin()?;
    let doc = Document { settings: Some(input), ..Document::default() };
    match config_io::import(&state, doc, Mode::Merge, false).await {
        Ok(_) => {}
        Err(config_io::ImportError::Invalid(plan)) => {
            return Err(V1Error::fields(plan.errors.into_iter().map(|e| (e.key, e.message))));
        }
        Err(config_io::ImportError::Api(e)) => return Err(e.into()),
    }
    tracing::info!(user = %user.username, "settings updated via API");
    let s = state.settings().await?;
    Ok(json_ok(&HeaderMap::new(), &s))
}
