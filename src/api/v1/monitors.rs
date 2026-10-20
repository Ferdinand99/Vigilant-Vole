//! `/api/v1/monitors/**`. Validation is the UI's own
//! ([`crate::api::monitors::form::validate`]); this module only maps JSON in
//! and out.

use std::collections::{BTreeMap, HashMap};

use axum::Router;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post, put};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Body, Id, Params, V1Error, V1Result, created, json_ok, limit, no_content, paginate, parse_cursor};
use crate::api::AppState;
use crate::api::monitors::form::{self, ChannelMode, FormContext, FormValues};
use crate::auth::{CurrentUser, Role};
use crate::db::events as db_events;
use crate::db::heartbeats as db_heartbeats;
use crate::db::models::{HeartbeatStatus, Monitor, MonitorType, NewMonitor};
use crate::db::tags::{self as db_tags, MonitorTag};
use crate::db::{monitors as db_monitors, notifications as db_notifications, rollups};
use crate::notification::SECRET_MASK;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/monitors", get(list).post(create))
        .route("/monitors/{id}", get(show).put(replace).patch(patch).delete(remove))
        .route("/monitors/{id}/pause", post(pause))
        .route("/monitors/{id}/resume", post(resume))
        .route("/monitors/{id}/check", post(check_now))
        .route("/monitors/{id}/heartbeats", get(heartbeats))
        .route("/monitors/{id}/uptime", get(uptime))
        .route("/monitors/{id}/events", get(events))
        .route("/monitors/{id}/tags", put(set_tags))
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Status shown in lists: the latest heartbeat, `paused`, or `unknown`.
fn status_label(m: &Monitor, status: Option<HeartbeatStatus>) -> &'static str {
    if !m.active {
        return "paused";
    }
    status.map_or("unknown", HeartbeatStatus::as_str)
}

/// Authentication secrets besides `basic_auth_pass`: masked in every response,
/// and protected like it when a request moves the monitor elsewhere.
const AUTH_SECRETS: &[&str] = &["secret", "oauth_client_secret", "tls_client_key"];

/// A monitor as JSON. Secrets are masked; push tokens only for editors.
pub fn monitor_json(
    m: &Monitor,
    status: Option<HeartbeatStatus>,
    tags: &[MonitorTag],
    links: &[(i64, bool)],
    can_edit: bool,
) -> Value {
    let mut v = serde_json::to_value(m).unwrap_or_else(|_| json!({}));
    if let Some(obj) = v.as_object_mut() {
        if m.basic_auth_pass.is_some() {
            obj.insert("basic_auth_pass".into(), json!(SECRET_MASK));
        }
        for secret in AUTH_SECRETS {
            if obj.get(*secret).is_some_and(|v| !v.is_null()) {
                obj.insert((*secret).into(), json!(SECRET_MASK));
            }
        }
        if !can_edit && m.push_token.is_some() {
            obj.insert("push_token".into(), json!(SECRET_MASK));
        }
        if let Some(h) = &m.http_headers
            && let Ok(parsed @ Value::Object(_)) = serde_json::from_str::<Value>(h)
        {
            obj.insert("http_headers".into(), if can_edit { parsed } else { crate::config_io::redact_headers(h) });
        }
        obj.insert("status".into(), json!(status_label(m, status)));
        obj.insert(
            "tags".into(),
            json!(tags.iter().map(|t| json!({"id": t.tag_id, "name": t.name, "color": t.color, "value": t.value})).collect::<Vec<_>>()),
        );
        obj.insert(
            "notifications".into(),
            json!(links.iter().map(|(id, esc)| json!({"channel_id": id, "escalation": esc})).collect::<Vec<_>>()),
        );
    }
    v
}

fn load_one(conn: &Connection, id: i64, can_edit: bool) -> rusqlite::Result<Option<Value>> {
    let Some(m) = db_monitors::get_monitor(conn, id)? else {
        return Ok(None);
    };
    let status = db_heartbeats::latest_local(conn, id)?.map(|h| h.status);
    let tags = db_tags::tags_for_monitor(conn, id)?;
    let links = db_notifications::monitor_channel_links(conn, id)?;
    Ok(Some(monitor_json(&m, status, &tags, &links, can_edit)))
}

async fn fetch(state: &AppState, id: i64, can_edit: bool) -> V1Result<Value> {
    state.with_conn(move |c| load_one(c, id, can_edit)).await?.ok_or_else(V1Error::not_found)
}

// ---------------------------------------------------------------------------
// List / show
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    /// up | down | pending | maintenance | paused | unknown
    status: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Tag name (case-insensitive).
    tag: Option<String>,
    /// Search in name, target and description.
    q: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

async fn list(state: axum::extract::State<AppState>, user: CurrentUser, headers: HeaderMap, Params(q): Params<ListQuery>) -> V1Result<Response> {
    let cursor = parse_cursor(q.cursor.as_deref())?;
    if let Some(s) = &q.status
        && !["up", "down", "pending", "maintenance", "paused", "unknown"].contains(&s.as_str())
    {
        return Err(V1Error::field("status", "Use up, down, pending, maintenance, paused or unknown."));
    }
    if let Some(t) = &q.kind
        && MonitorType::parse(t).is_none()
    {
        return Err(V1Error::field("type", format!("Unknown monitor type “{t}”.")));
    }
    let can_edit = user.can_edit();
    let rows = state
        .with_conn(move |c| {
            let monitors = db_monitors::list_monitors_with_status(c)?;
            let tags = db_tags::tags_by_monitor(c)?;
            let mut links: HashMap<i64, Vec<(i64, bool)>> = HashMap::new();
            let mut stmt = c.prepare("SELECT monitor_id, channel_id, escalation FROM monitor_notifications ORDER BY channel_id")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, bool>(2)?)))? {
                let (m, ch, esc) = row?;
                links.entry(m).or_default().push((ch, esc));
            }
            Ok((monitors, tags, links))
        })
        .await?;
    let (monitors, tags, links) = rows;
    let needle = q.q.as_deref().map(str::to_lowercase).filter(|s| !s.is_empty());
    let mut items: Vec<(i64, Value)> = monitors
        .into_iter()
        .filter(|row| {
            let m = &row.monitor;
            let tag_list = tags.get(&m.id).map(Vec::as_slice).unwrap_or_default();
            q.status.as_deref().is_none_or(|s| status_label(m, row.status) == s)
                && q.kind.as_deref().is_none_or(|k| m.monitor_type.as_str() == k)
                && q.tag.as_deref().is_none_or(|t| tag_list.iter().any(|x| x.name.eq_ignore_ascii_case(t)))
                && needle.as_deref().is_none_or(|n| {
                    m.name.to_lowercase().contains(n) || m.target.to_lowercase().contains(n) || m.description.to_lowercase().contains(n)
                })
        })
        .map(|row| {
            let m = &row.monitor;
            let v = monitor_json(
                m,
                row.status,
                tags.get(&m.id).map(Vec::as_slice).unwrap_or_default(),
                links.get(&m.id).map(Vec::as_slice).unwrap_or_default(),
                can_edit,
            );
            (m.id, v)
        })
        .collect();
    items.sort_by_key(|(id, _)| *id);
    let page = paginate(items, |(id, _)| *id, cursor, limit(q.limit));
    let page = super::ListPage { data: page.data.into_iter().map(|(_, v)| v).collect(), next_cursor: page.next_cursor };
    Ok(json_ok(&headers, &page))
}

async fn show(state: axum::extract::State<AppState>, user: CurrentUser, headers: HeaderMap, Id(id): Id<i64>) -> V1Result<Response> {
    let v = fetch(&state, id, user.can_edit()).await?;
    Ok(json_ok(&headers, &v))
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Fields the API never accepts from clients (server-managed).
const READ_ONLY: &[&str] = &["id", "status", "created_at", "updated_at", "source", "external_id", "push_token"];

/// A tag reference in a request: `"name"`, `{"id": 3}` or `{"name": "env", "value": "prod"}`.
#[derive(Debug, Clone)]
struct TagIn {
    id: Option<i64>,
    name: Option<String>,
    value: String,
}

fn parse_tags(v: &Value) -> V1Result<Vec<TagIn>> {
    let Value::Array(list) = v else {
        return Err(V1Error::field("tags", "Send a list of tag names or {\"id\"|\"name\", \"value\"} objects."));
    };
    list.iter()
        .map(|t| match t {
            Value::String(name) => Ok(TagIn { id: None, name: Some(name.trim().to_string()), value: String::new() }),
            Value::Number(n) => Ok(TagIn { id: n.as_i64(), name: None, value: String::new() }),
            Value::Object(o) => Ok(TagIn {
                id: o.get("id").and_then(Value::as_i64),
                name: o.get("name").and_then(Value::as_str).map(|s| s.trim().to_string()),
                value: o.get("value").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
            }),
            _ => Err(V1Error::field("tags", "Each tag is a name, an id or an object.")),
        })
        .collect()
}

fn parse_links(v: &Value) -> V1Result<Vec<(i64, bool)>> {
    let Value::Array(list) = v else {
        return Err(V1Error::field("notifications", "Send a list of channel ids or {\"channel_id\", \"escalation\"} objects."));
    };
    list.iter()
        .map(|c| match c {
            Value::Number(n) => n.as_i64().map(|id| (id, false)).ok_or_else(|| V1Error::field("notifications", "Channel ids are integers.")),
            Value::Object(o) => o
                .get("channel_id")
                .or_else(|| o.get("id"))
                .and_then(Value::as_i64)
                .map(|id| (id, o.get("escalation").and_then(Value::as_bool).unwrap_or(false)))
                .ok_or_else(|| V1Error::field("notifications", "Each entry needs a channel_id.")),
            _ => Err(V1Error::field("notifications", "Each entry is a channel id or an object.")),
        })
        .collect()
}

/// What a write request asks for, before validation.
struct WriteRequest {
    monitor: NewMonitor,
    tags: Option<Vec<TagIn>>,
    channels: Option<Vec<(i64, bool)>>,
}

/// Merges the body into `base` (`NewMonitor` as JSON) and parses it.
fn read_body(body: Value, base: &NewMonitor, existing: Option<&Monitor>) -> V1Result<WriteRequest> {
    let Value::Object(mut body) = body else {
        return Err(V1Error::bad_request("Send a JSON object."));
    };
    let tags = body.remove("tags").map(|v| parse_tags(&v)).transpose()?;
    let channels = body.remove("notifications").map(|v| parse_links(&v)).transpose()?;
    let mut obj = match serde_json::to_value(base) {
        Ok(Value::Object(o)) => o,
        _ => Map::new(),
    };
    // Stored secrets carried over from `base` (PATCH) count as masked, so the
    // same "only while the destination stays the same" rule applies to them.
    for secret in ["basic_auth_pass"].iter().chain(AUTH_SECRETS) {
        if obj.get(*secret).is_some_and(|v| !v.is_null()) {
            obj.insert((*secret).into(), json!(SECRET_MASK));
        }
    }
    // Clients that only know basic auth don't send `auth_method`: a user name
    // then means basic, as it always has.
    let method_given = body.contains_key("auth_method");
    let basic_given = body.get("basic_auth_user").and_then(Value::as_str).is_some_and(|u| !u.is_empty());
    if let Some(h) = obj.get("http_headers").and_then(Value::as_str) {
        let redacted = crate::config_io::redact_headers(h);
        if redacted.is_object() {
            obj.insert("http_headers".into(), json!(redacted.to_string()));
        }
    }
    let mut unknown = BTreeMap::new();
    for (k, v) in body {
        if READ_ONLY.contains(&k.as_str()) {
            continue;
        }
        if !obj.contains_key(&k) {
            unknown.insert(k.clone(), format!("Unknown field “{k}”."));
            continue;
        }
        let v = match (k.as_str(), v) {
            ("http_headers", v @ Value::Object(_)) => Value::String(v.to_string()),
            (_, v) => v,
        };
        obj.insert(k, v);
    }
    if !unknown.is_empty() {
        return Err(V1Error::validation("", unknown));
    }
    let mut monitor: NewMonitor = serde_json::from_value(Value::Object(obj)).map_err(|e| V1Error::validation(format!("Invalid field: {e}"), BTreeMap::new()))?;
    if !method_given && basic_given && monitor.auth_method == "none" {
        monitor.auth_method = "basic".into();
    }
    // A masked password goes through to `form::validate`, which keeps the
    // stored one only while the target and proxy stay the same. Masked header
    // values follow the same rule here.
    let moved = existing.is_some_and(|e| {
        origin(&e.target) != origin(&monitor.target) || e.proxy_url.as_deref().map(str::trim) != monitor.proxy_url.as_deref().map(str::trim)
    });
    if moved && monitor.http_headers.as_deref().is_some_and(|h| h.contains(SECRET_MASK)) {
        let fields = BTreeMap::from([("http_headers".to_string(), crate::i18n::t("notif.err_reenter_secret").to_string())]);
        return Err(V1Error::validation("", fields));
    }
    if let Some(h) = monitor.http_headers.clone()
        && h.contains(SECRET_MASK)
        && let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(&h)
    {
        let old: Map<String, Value> = existing
            .and_then(|e| e.http_headers.as_deref())
            .and_then(|o| serde_json::from_str::<Value>(o).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        for (k, v) in map.iter_mut() {
            if v.as_str() == Some(SECRET_MASK)
                && let Some((_, prev)) = old.iter().find(|(ok, _)| ok.eq_ignore_ascii_case(k))
            {
                *v = prev.clone();
            }
        }
        monitor.http_headers = Some(Value::Object(map).to_string());
    }
    Ok(WriteRequest { monitor, tags, channels })
}

/// Scheme, host and port of a URL target; other targets as typed.
fn origin(target: &str) -> String {
    match url::Url::parse(target.trim()) {
        Ok(u) if u.has_host() => format!("{}://{}:{}", u.scheme(), u.host_str().unwrap_or_default(), u.port_or_known_default().unwrap_or(0)),
        _ => target.trim().to_ascii_lowercase(),
    }
}

/// Validates and stores; returns the monitor id.
fn save(conn: &mut Connection, existing: Option<&Monitor>, req: WriteRequest) -> rusqlite::Result<Result<i64, V1Error>> {
    // Context, as the monitor form builds it.
    let excluded: Vec<i64> = match existing {
        Some(m) => {
            let mut ids = db_monitors::descendant_ids(conn, m.id)?;
            ids.push(m.id);
            ids
        }
        None => Vec::new(),
    };
    let all = db_monitors::list_monitors(conn)?;
    let groups: Vec<i64> = all.iter().filter(|m| m.monitor_type == MonitorType::Group && !excluded.contains(&m.id)).map(|m| m.id).collect();
    let channel_ids: Vec<i64> = db_notifications::list_channels(conn)?.iter().map(|c| c.id).collect();
    let all_tags = db_tags::list_tags(conn)?;
    let child_count = match existing {
        Some(m) => db_monitors::children(conn, m.id)?.len(),
        None => 0,
    };

    if let Some(p) = req.monitor.parent_id
        && !groups.contains(&p)
    {
        return Ok(Err(V1Error::field("parent_id", "Choose a group monitor (not this monitor or one inside it).")));
    }
    let links = match &req.channels {
        Some(list) => {
            if let Some((bad, _)) = list.iter().find(|(id, _)| !channel_ids.contains(id)) {
                return Ok(Err(V1Error::field("notifications", format!("No notification channel with id {bad}."))));
            }
            list.clone()
        }
        None => match existing {
            Some(m) => db_notifications::monitor_channel_links(conn, m.id)?,
            None => Vec::new(),
        },
    };
    // Tags: by id (must exist) or by name (created if new; placeholder ids < 0).
    let mut new_tags: Vec<(i64, String)> = Vec::new();
    let tags: Option<Vec<(i64, String)>> = match &req.tags {
        None => None,
        Some(list) => {
            let mut out = Vec::new();
            for t in list {
                let id = match (t.id, &t.name) {
                    (Some(id), _) if all_tags.iter().any(|x| x.id == id) => id,
                    (Some(id), _) => return Ok(Err(V1Error::field("tags", format!("No tag with id {id}.")))),
                    (None, Some(name)) if !name.is_empty() => match all_tags.iter().find(|x| x.name.eq_ignore_ascii_case(name)) {
                        Some(x) => x.id,
                        None => {
                            if name.chars().count() > db_tags::MAX_NAME_LEN {
                                return Ok(Err(V1Error::field("tags", format!("Keep tag names under {} characters.", db_tags::MAX_NAME_LEN))));
                            }
                            match new_tags.iter().position(|(_, n)| n.eq_ignore_ascii_case(name)) {
                                Some(i) => -(i as i64) - 1,
                                None => {
                                    new_tags.push((-(new_tags.len() as i64) - 1, name.clone()));
                                    -(new_tags.len() as i64)
                                }
                            }
                        }
                    },
                    _ => return Ok(Err(V1Error::field("tags", "Each tag needs an id or a name."))),
                };
                out.retain(|(x, _): &(i64, String)| *x != id);
                out.push((id, t.value.clone()));
            }
            Some(out)
        }
    };
    let tag_views: Vec<MonitorTag> = tags
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|(tag_id, value)| MonitorTag { tag_id, name: String::new(), color: String::new(), value })
        .collect();
    let modes: Vec<(i64, ChannelMode)> = links
        .iter()
        .map(|(id, esc)| (*id, if *esc { ChannelMode::Escalation } else { ChannelMode::Alert }))
        .collect();
    let values = FormValues::from_new(&req.monitor, modes, &tag_views);
    let cx = FormContext {
        editing_id: existing.map(|m| m.id),
        allowed_parents: groups,
        channel_ids,
        tag_ids: tag_views.iter().map(|t| t.tag_id).collect(),
        child_count,
    };
    let base = existing.map(Monitor::to_new).unwrap_or_default();
    let valid = match form::validate(&values, &base, &cx) {
        Ok(v) => v,
        Err(errors) => {
            let fields = errors.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            return Ok(Err(V1Error::validation("", fields)));
        }
    };
    let mut monitor = valid.monitor;
    // Not part of the monitor form (pause/resume are separate actions there).
    monitor.sort_order = req.monitor.sort_order;
    monitor.active = req.monitor.active;
    if monitor.monitor_type == MonitorType::Push && monitor.push_token.is_none() {
        monitor.push_token = Some(crate::api::push::generate_push_token());
    }

    let tx = conn.transaction()?;
    let id = match existing {
        Some(m) => {
            db_monitors::update_monitor(&tx, m.id, &monitor)?;
            m.id
        }
        None => db_monitors::insert_monitor(&tx, &monitor)?,
    };
    if req.channels.is_some() {
        db_notifications::set_monitor_channels(&tx, id, &valid.channels)?;
    }
    if let Some(tags) = tags {
        let mut created: HashMap<i64, i64> = HashMap::new();
        for (ph, name) in &new_tags {
            created.insert(*ph, db_tags::get_or_create(&tx, name, db_tags::DEFAULT_COLOR)?);
        }
        let resolved: Vec<(i64, String)> = tags.into_iter().map(|(t, v)| (created.get(&t).copied().unwrap_or(t), v)).collect();
        db_tags::set_monitor_tags(&tx, id, &resolved)?;
    }
    tx.commit()?;
    Ok(Ok(id))
}

async fn after_change(state: &AppState, id: i64) {
    crate::maintenance::invalidate();
    state.scheduler.reload(id).await;
    state.notify_update();
}

async fn create(state: axum::extract::State<AppState>, user: CurrentUser, Body(body): Body<Value>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let req = read_body(body, &NewMonitor::default(), None)?;
    let id = state.with_conn(move |c| save(c, None, req)).await??;
    after_change(&state, id).await;
    tracing::info!(user = %user.username, monitor_id = id, "monitor created via API");
    let v = fetch(&state, id, true).await?;
    Ok(created(format!("/api/v1/monitors/{id}"), &v))
}

async fn write(state: &AppState, user: &CurrentUser, id: i64, body: Value, partial: bool) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let existing = state
        .with_conn(move |c| db_monitors::get_monitor(c, id))
        .await?
        .ok_or_else(V1Error::not_found)?;
    let id = state
        .with_conn(move |c| {
            let base = if partial {
                existing.to_new()
            } else {
                // Full replace: defaults + body, but keep server-managed fields.
                NewMonitor {
                    push_token: existing.push_token.clone(),
                    source: existing.source.clone(),
                    external_id: existing.external_id.clone(),
                    ..NewMonitor::default()
                }
            };
            let req = match read_body(body, &base, Some(&existing)) {
                Ok(r) => r,
                Err(e) => return Ok(Err(e)),
            };
            save(c, Some(&existing), req)
        })
        .await??;
    after_change(state, id).await;
    let headers = HeaderMap::new();
    Ok(json_ok(&headers, &fetch(state, id, true).await?))
}

async fn replace(state: axum::extract::State<AppState>, user: CurrentUser, Id(id): Id<i64>, Body(body): Body<Value>) -> V1Result<Response> {
    write(&state, &user, id, body, false).await
}

async fn patch(state: axum::extract::State<AppState>, user: CurrentUser, Id(id): Id<i64>, Body(body): Body<Value>) -> V1Result<Response> {
    write(&state, &user, id, body, true).await
}

async fn remove(state: axum::extract::State<AppState>, user: CurrentUser, Id(id): Id<i64>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let found = state
        .with_conn(move |c| {
            let exists = db_monitors::get_monitor(c, id)?.is_some();
            if exists {
                db_monitors::delete_monitor(c, id)?;
            }
            Ok(exists)
        })
        .await?;
    if !found {
        return Err(V1Error::not_found());
    }
    after_change(&state, id).await;
    tracing::info!(user = %user.username, monitor_id = id, "monitor deleted via API");
    Ok(no_content())
}

async fn set_paused(state: &AppState, user: &CurrentUser, id: i64, paused: bool) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let found = state
        .with_conn(move |c| {
            let Some(m) = db_monitors::get_monitor(c, id)? else {
                return Ok(false);
            };
            if m.active == paused {
                db_monitors::set_active(c, id, !paused)?;
                if paused {
                    db_events::log_paused(c, id)?;
                } else {
                    db_events::log_resumed(c, id)?;
                }
            }
            Ok(true)
        })
        .await?;
    if !found {
        return Err(V1Error::not_found());
    }
    state.scheduler.reload(id).await;
    state.notify_update();
    Ok(json_ok(&HeaderMap::new(), &fetch(state, id, true).await?))
}

async fn pause(state: axum::extract::State<AppState>, user: CurrentUser, Id(id): Id<i64>) -> V1Result<Response> {
    set_paused(&state, &user, id, true).await
}

async fn resume(state: axum::extract::State<AppState>, user: CurrentUser, Id(id): Id<i64>) -> V1Result<Response> {
    set_paused(&state, &user, id, false).await
}

async fn check_now(state: axum::extract::State<AppState>, user: CurrentUser, Id(id): Id<i64>) -> V1Result<Response> {
    user.require(Role::Editor)?;
    let exists = state.with_conn(move |c| db_monitors::get_monitor(c, id)).await?.is_some();
    if !exists {
        return Err(V1Error::not_found());
    }
    match state.scheduler.check_now(id).await {
        Some(status) => {
            let latest = state.with_conn(move |c| db_heartbeats::latest_local(c, id)).await?;
            Ok(json_ok(&HeaderMap::new(), &json!({ "status": status, "heartbeat": latest })))
        }
        None => Err(V1Error::conflict("This monitor can’t be checked on demand (paused, push or group).")),
    }
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct HeartbeatQuery {
    limit: Option<i64>,
    cursor: Option<String>,
    /// UTC `YYYY-MM-DD HH:MM:SS` (or RFC 3339): only newer heartbeats.
    since: Option<String>,
}

async fn heartbeats(state: axum::extract::State<AppState>, _user: CurrentUser, headers: HeaderMap, Id(id): Id<i64>, Params(q): Params<HeartbeatQuery>) -> V1Result<Response> {
    let cursor = parse_cursor(q.cursor.as_deref())?;
    let since = match q.since.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => Some(
            crate::maintenance::parse_db_ts(s)
                .map(crate::maintenance::fmt_db_ts)
                .ok_or_else(|| V1Error::field("since", "Use UTC time like 2026-10-19 14:00:00."))?,
        ),
    };
    let lim = limit(q.limit);
    let rows = state
        .with_conn(move |c| {
            if db_monitors::get_monitor(c, id)?.is_none() {
                return Ok(None);
            }
            let mut stmt = c.prepare(
                "SELECT id, monitor_id, agent_id, status, response_time_ms, message, checked_at FROM heartbeats
                 WHERE monitor_id = ?1 AND agent_id IS NULL
                   AND (?2 IS NULL OR checked_at >= ?2)
                   AND (?3 IS NULL OR (checked_at, id) < (SELECT checked_at, id FROM heartbeats WHERE id = ?3))
                 ORDER BY checked_at DESC, id DESC LIMIT ?4",
            )?;
            let rows = stmt.query_map(params![id, since, cursor, lim + 1], db_heartbeats::row_to_heartbeat)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map(Some)
        })
        .await?
        .ok_or_else(V1Error::not_found)?;
    let mut data = rows;
    let next_cursor = if data.len() as i64 > lim {
        data.truncate(lim as usize);
        data.last().map(|h| h.id.to_string())
    } else {
        None
    };
    Ok(json_ok(&headers, &super::ListPage { data, next_cursor }))
}

#[derive(Debug, Default, Deserialize)]
struct UptimeQuery {
    days: Option<i64>,
}

async fn uptime(state: axum::extract::State<AppState>, _user: CurrentUser, headers: HeaderMap, Id(id): Id<i64>, Params(q): Params<UptimeQuery>) -> V1Result<Response> {
    let days = q.days.unwrap_or(30);
    if !(1..=365).contains(&days) {
        return Err(V1Error::field("days", "Use 1 to 365 days."));
    }
    let v = state
        .with_conn(move |c| {
            if db_monitors::get_monitor(c, id)?.is_none() {
                return Ok(None);
            }
            Ok(Some(json!({
                "monitor_id": id,
                "days": days,
                "uptime_pct": rollups::uptime_pct(c, id, days)?,
                "uptime_24h_pct": rollups::uptime_pct(c, id, 1)?,
                "daily": rollups::daily_uptime(c, id, days)?,
            })))
        })
        .await?
        .ok_or_else(V1Error::not_found)?;
    Ok(json_ok(&headers, &v))
}

#[derive(Debug, Default, Deserialize)]
struct EventsQuery {
    limit: Option<i64>,
}

async fn events(state: axum::extract::State<AppState>, _user: CurrentUser, headers: HeaderMap, Id(id): Id<i64>, Params(q): Params<EventsQuery>) -> V1Result<Response> {
    let lim = limit(q.limit);
    let list = state
        .with_conn(move |c| {
            if db_monitors::get_monitor(c, id)?.is_none() {
                return Ok(None);
            }
            db_events::list_for_monitor(c, id, lim).map(Some)
        })
        .await?
        .ok_or_else(V1Error::not_found)?;
    Ok(json_ok(&headers, &json!({ "data": list, "next_cursor": null })))
}

/// `PUT /monitors/{id}/tags` — replace the monitor's tags.
async fn set_tags(state: axum::extract::State<AppState>, user: CurrentUser, Id(id): Id<i64>, Body(body): Body<Value>) -> V1Result<Response> {
    write(&state, &user, id, json!({ "tags": body }), true).await
}
