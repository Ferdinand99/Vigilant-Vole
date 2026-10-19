//! `/api/v1/maintenance/**`. Windows are exchanged in their stored form:
//! `start_at` / `end_at` in UTC (`YYYY-MM-DD HH:MM:SS`), `time_from` /
//! `time_to` as local `HH:MM` in `timezone`, `weekdays` as ISO days (`"1,3,5"`).

use std::collections::HashMap;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Body, Id, V1Error, V1Result, created, json_ok, no_content};
use crate::api::AppState;
use crate::auth::{CurrentUser, Role};
use crate::config_io::{self, MaintenanceDoc};
use crate::db::maintenance::{self as db_maint, MaintenanceWindow, NewWindow};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/maintenance", get(list).post(create))
        .route("/maintenance/{id}", get(show).put(update).patch(update).delete(remove))
        .layer(axum::middleware::from_fn(crate::api::public_status::invalidate_after_writes))
}

fn window_json(conn: &Connection, w: &MaintenanceWindow) -> rusqlite::Result<Value> {
    let mut v = serde_json::to_value(w).unwrap_or_else(|_| json!({}));
    let schedule = crate::maintenance::Schedule::from_window(w);
    let state = crate::maintenance::window_state(&schedule, jiff::Timestamp::now());
    v["state"] = json!(state.label());
    v["monitor_ids"] = json!(db_maint::monitor_ids(conn, w.id)?);
    v["status_page_ids"] = json!(db_maint::status_page_ids(conn, w.id)?);
    Ok(v)
}

async fn load(state: &AppState, id: i64) -> V1Result<Value> {
    state
        .with_conn(move |c| match db_maint::get(c, id)? {
            Some(w) => window_json(c, &w).map(Some),
            None => Ok(None),
        })
        .await?
        .ok_or_else(V1Error::not_found)
}

async fn list(State(state): State<AppState>, _user: CurrentUser, headers: HeaderMap) -> V1Result<Response> {
    let data = state
        .with_conn(|c| db_maint::list(c)?.iter().map(|w| window_json(c, w)).collect::<rusqlite::Result<Vec<_>>>())
        .await?;
    Ok(json_ok(&headers, &json!({ "data": data, "next_cursor": null })))
}

async fn show(State(state): State<AppState>, _user: CurrentUser, headers: HeaderMap, Id(id): Id<i64>) -> V1Result<Response> {
    Ok(json_ok(&headers, &load(&state, id).await?))
}

#[derive(Debug, Default, Deserialize)]
struct WindowIn {
    title: Option<String>,
    description: Option<String>,
    strategy: Option<String>,
    active: Option<bool>,
    #[serde(default, deserialize_with = "double")]
    start_at: Option<Option<String>>,
    #[serde(default, deserialize_with = "double")]
    end_at: Option<Option<String>>,
    #[serde(default, deserialize_with = "double")]
    weekdays: Option<Option<String>>,
    #[serde(default, deserialize_with = "double")]
    time_from: Option<Option<String>>,
    #[serde(default, deserialize_with = "double")]
    time_to: Option<Option<String>>,
    timezone: Option<String>,
    monitor_ids: Option<Vec<i64>>,
    status_page_ids: Option<Vec<i64>>,
}

/// Keeps `null` (clear) apart from a missing field (keep).
fn double<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

/// Validates with the config importer's rules (ids are passed as keys).
fn validate(conn: &Connection, input: &WindowIn, base: Option<(&MaintenanceWindow, Vec<i64>, Vec<i64>)>) -> rusqlite::Result<Result<NewWindow, V1Error>> {
    let (bw, bm, bp) = match base {
        Some((w, m, p)) => (Some(w), m, p),
        None => (None, Vec::new(), Vec::new()),
    };
    let pick = |new: &Option<Option<String>>, old: Option<&Option<String>>| match new {
        Some(v) => v.clone(),
        None => old.cloned().flatten(),
    };
    let doc = MaintenanceDoc {
        title: input.title.clone().or_else(|| bw.map(|w| w.title.clone())).unwrap_or_default(),
        description: input.description.clone().or_else(|| bw.map(|w| w.description.clone())).unwrap_or_default(),
        strategy: input.strategy.clone().or_else(|| bw.map(|w| w.strategy.clone())).unwrap_or_else(|| "manual".into()),
        active: input.active.or(bw.map(|w| w.active)).unwrap_or(true),
        start_at: pick(&input.start_at, bw.map(|w| &w.start_at)),
        end_at: pick(&input.end_at, bw.map(|w| &w.end_at)),
        weekdays: pick(&input.weekdays, bw.map(|w| &w.weekdays)),
        time_from: pick(&input.time_from, bw.map(|w| &w.time_from)),
        time_to: pick(&input.time_to, bw.map(|w| &w.time_to)),
        timezone: input.timezone.clone().or_else(|| bw.map(|w| w.timezone.clone())).unwrap_or_else(|| "UTC".into()),
        monitors: input.monitor_ids.clone().unwrap_or(bm).iter().map(i64::to_string).collect(),
        status_pages: input.status_page_ids.clone().unwrap_or(bp).iter().map(i64::to_string).collect(),
    };
    let ids = |sql: &str| -> rusqlite::Result<HashMap<String, i64>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![], |r| r.get::<_, i64>(0))?;
        rows.map(|r| r.map(|id| (id.to_string(), id))).collect()
    };
    let monitors = ids("SELECT id FROM monitors")?;
    let pages = ids("SELECT id FROM status_pages")?;
    Ok(config_io::validate_window(&doc, &monitors, &pages).map_err(|errors| {
        V1Error::fields(errors.into_iter().map(|(k, v)| {
            let k = match k.as_str() {
                "monitors" => "monitor_ids".to_string(),
                "status_pages" => "status_page_ids".to_string(),
                _ => k,
            };
            (k, v.replace("with key", "with id").replace("with slug", "with id"))
        }))
    }))
}

fn changed(state: &AppState) {
    crate::maintenance::invalidate();
    state.notify_update();
}

async fn create(State(state): State<AppState>, user: CurrentUser, Body(input): Body<WindowIn>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let id = state
        .with_conn(move |c| match validate(c, &input, None)? {
            Ok(row) => db_maint::insert(c, &row).map(Ok),
            Err(e) => Ok(Err(e)),
        })
        .await??;
    changed(&state);
    Ok(created(format!("/api/v1/maintenance/{id}"), &load(&state, id).await?))
}

async fn update(State(state): State<AppState>, user: CurrentUser, Id(id): Id<i64>, Body(input): Body<WindowIn>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    state
        .with_conn(move |c| {
            let Some(w) = db_maint::get(c, id)? else {
                return Ok(Err(V1Error::not_found()));
            };
            let base = (&w, db_maint::monitor_ids(c, id)?, db_maint::status_page_ids(c, id)?);
            match validate(c, &input, Some(base))? {
                Ok(row) => db_maint::update(c, id, &row).map(|_| Ok(())),
                Err(e) => Ok(Err(e)),
            }
        })
        .await??;
    changed(&state);
    Ok(json_ok(&HeaderMap::new(), &load(&state, id).await?))
}

async fn remove(State(state): State<AppState>, user: CurrentUser, Id(id): Id<i64>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    if !state.with_conn(move |c| db_maint::delete(c, id)).await? {
        return Err(V1Error::not_found());
    }
    changed(&state);
    Ok(no_content())
}
