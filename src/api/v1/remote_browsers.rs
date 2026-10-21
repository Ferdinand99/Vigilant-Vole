//! `/api/v1/remote-browsers/**`: the CDP endpoints real-browser monitors
//! use. Anyone signed in can list them (URLs with their token or password
//! masked); changing them needs an admin. A masked URL sent back keeps the
//! stored secret while it points at the same scheme, host and port.

use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Body, Id, V1Error, V1Result, created, json_ok, no_content};
use crate::api::AppState;
use crate::auth::{CurrentUser, Role};
use crate::db::remote_browsers::{self as db_rb, RemoteBrowser};
use crate::monitor::checkers::real_browser::{self, endpoint};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/remote-browsers", get(list).post(create))
        .route(
            "/remote-browsers/{id}",
            get(show).put(update).patch(update).delete(remove),
        )
        .route("/remote-browsers/{id}/test", post(test))
}

fn browser_json(b: &RemoteBrowser, monitors: i64, is_default: bool) -> Value {
    json!({
        "id": b.id,
        "name": b.name,
        "url": endpoint::masked(&b.url),
        "monitors": monitors,
        "default": is_default,
        "created_at": b.created_at,
        "updated_at": b.updated_at,
    })
}

fn load_all(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<Value>> {
    let used = db_rb::usage(conn)?;
    Ok(db_rb::list(conn)?
        .iter()
        .enumerate()
        .map(|(i, b)| browser_json(b, used.get(&b.id).copied().unwrap_or(0), i == 0))
        .collect())
}

fn load_one(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<Option<Value>> {
    Ok(load_all(conn)?.into_iter().find(|v| v["id"] == json!(id)))
}

async fn list(
    State(state): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
) -> V1Result<Response> {
    let data = state.with_conn(|c| load_all(c)).await?;
    Ok(json_ok(
        &headers,
        &json!({ "data": data, "next_cursor": null }),
    ))
}

async fn show(
    State(state): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    let v = state
        .with_conn(move |c| load_one(c, id))
        .await?
        .ok_or_else(V1Error::not_found)?;
    Ok(json_ok(&headers, &v))
}

#[derive(Debug, Deserialize)]
struct BrowserIn {
    name: Option<String>,
    url: Option<String>,
}

fn invalid(errors: crate::api::remote_browsers::Errors) -> V1Error {
    V1Error::validation(
        "",
        errors
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    Body(input): Body<BrowserIn>,
) -> V1Result<Response> {
    user.require(Role::Admin)?;
    let (name, url) = (
        input.name.unwrap_or_default(),
        input.url.unwrap_or_default(),
    );
    let saved = state
        .with_conn(move |c| {
            Ok(
                match crate::api::remote_browsers::validate(c, None, &name, &url)? {
                    Ok((name, url)) => {
                        let id = db_rb::insert(c, &name, &url)?;
                        Ok(load_one(c, id)?.unwrap_or(Value::Null))
                    }
                    Err(e) => Err(e),
                },
            )
        })
        .await?
        .map_err(invalid)?;
    let id = saved["id"].as_i64().unwrap_or_default();
    Ok(created(format!("/api/v1/remote-browsers/{id}"), &saved))
}

async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
    Body(input): Body<BrowserIn>,
) -> V1Result<Response> {
    user.require(Role::Admin)?;
    let saved = state
        .with_conn(move |c| {
            let Some(current) = db_rb::get(c, id)? else {
                return Ok(None);
            };
            let name = input.name.unwrap_or_else(|| current.name.clone());
            // Leaving the URL out keeps it (secret included).
            let url = input.url.unwrap_or_else(|| current.url.clone());
            Ok(Some(
                match crate::api::remote_browsers::validate(c, Some(&current), &name, &url)? {
                    Ok((name, url)) => {
                        db_rb::update(c, id, &name, &url)?;
                        Ok(load_one(c, id)?.unwrap_or(Value::Null))
                    }
                    Err(e) => Err(e),
                },
            ))
        })
        .await?
        .ok_or_else(V1Error::not_found)?
        .map_err(invalid)?;
    Ok(json_ok(&HeaderMap::new(), &saved))
}

async fn remove(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    user.require(Role::Admin)?;
    if !state.with_conn(move |c| db_rb::delete(c, id)).await? {
        return Err(V1Error::not_found());
    }
    Ok(no_content())
}

async fn test(
    State(state): State<AppState>,
    user: CurrentUser,
    Id(id): Id<i64>,
) -> V1Result<Response> {
    user.require(Role::Admin)?;
    let b = state
        .with_conn(move |c| db_rb::get(c, id))
        .await?
        .ok_or_else(V1Error::not_found)?;
    let body = match real_browser::test_connection(&b.url, Duration::from_secs(10)).await {
        Ok(v) => {
            json!({"ok": true, "product": v.product, "protocol_version": v.protocol, "user_agent": v.user_agent})
        }
        Err(err) => json!({"ok": false, "error": err}),
    };
    Ok(json_ok(&HeaderMap::new(), &body))
}
