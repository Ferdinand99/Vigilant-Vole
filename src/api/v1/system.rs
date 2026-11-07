//! `/api/v1/me`, `/api/v1/health`, `/api/v1/summary`, `/api/v1/agents`,
//! `/api/v1/settings`.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Body, Params, V1Error, V1Result, json_ok};
use crate::api::AppState;
use crate::auth::{AuthMethod, CurrentUser};
use crate::config_io::{self, Document, Mode, SettingsDoc};
use crate::db::agents as db_agents;
use crate::db::models::{HeartbeatStatus, MonitorType};
use crate::db::{heartbeats as db_heartbeats, monitors as db_monitors, rollups};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/me", get(me))
        .route("/health", get(health))
        .route("/summary", get(summary))
        .route("/agents", get(agents))
        .route(
            "/settings",
            get(get_settings).patch(patch_settings).put(patch_settings),
        )
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

async fn health(
    State(state): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
) -> V1Result<Response> {
    let counts = state
        .with_conn(|c| {
            let monitors = crate::db::monitors::list_monitors_with_latest(c)?;
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

#[derive(Debug, Default, Deserialize)]
struct SummaryQuery {
    days: Option<i64>,
}

/// At most this many monitors in `down`.
const SUMMARY_DOWN_CAP: usize = 25;
/// Longest `down[].message`, in characters (the agent protocol's limit for
/// check messages).
const SUMMARY_MESSAGE_CHARS: usize = 500;

/// One call for a status card: monitor counts, mean uptime and what is down.
/// Open to every signed-in caller, as `/monitors` is: monitors have no
/// per-user visibility, so a viewer sees exactly the monitors the list shows.
async fn summary(
    State(state): State<AppState>,
    _user: CurrentUser,
    headers: HeaderMap,
    Params(q): Params<SummaryQuery>,
) -> V1Result<Response> {
    let days = q.days.unwrap_or(30);
    if !(1..=365).contains(&days) {
        return Err(V1Error::field("days", "Use 1 to 365 days."));
    }
    let v = state.with_conn(move |c| build_summary(c, days)).await?;
    Ok(json_ok(&headers, &v))
}

/// Mean of per-monitor percentages, rounded to two decimals.
fn mean_pct(sum: f64, n: usize) -> Option<f64> {
    (n > 0).then(|| (sum / n as f64 * 100.0).round() / 100.0)
}

fn build_summary(c: &rusqlite::Connection, days: i64) -> rusqlite::Result<Value> {
    // Groups aggregate their children (their own heartbeats are the worst of
    // them), so counting them too would count every outage and every uptime
    // twice. The dashboard skips them for the same reason.
    let monitors: Vec<_> = db_monitors::list_monitors_with_latest(c)?
        .into_iter()
        .filter(|row| row.monitor.monitor_type != MonitorType::Group)
        .collect();
    let covered = crate::maintenance::covered_monitors(c)?;
    let (mut up, mut down, mut pending, mut maintenance, mut paused, mut unknown) =
        (0_i64, 0_i64, 0_i64, 0_i64, 0_i64, 0_i64);
    let mut down_ids = Vec::new();
    let (mut sum, mut counted) = (0.0_f64, 0_usize);
    let (mut sum_24h, mut counted_24h) = (0.0_f64, 0_usize);
    for row in &monitors {
        let m = &row.monitor;
        // Paused monitors are neither down nor part of the uptime figures
        // (as in `/metrics`).
        if !m.active {
            paused += 1;
            continue;
        }
        if covered.contains(&m.id) {
            maintenance += 1;
        } else {
            match row.status {
                Some(HeartbeatStatus::Up) => up += 1,
                Some(HeartbeatStatus::Down) => {
                    down += 1;
                    down_ids.push((m.id, m.name.clone()));
                }
                Some(HeartbeatStatus::Pending) => pending += 1,
                Some(HeartbeatStatus::Maintenance) => maintenance += 1,
                None => unknown += 1,
            }
        }
        let pct = rollups::uptime_pct(c, m.id, days)?;
        if let Some(p) = pct {
            sum += p;
            counted += 1;
        }
        let pct_24h = if days == 1 {
            pct
        } else {
            rollups::uptime_24h_cached(c, m.id)?
        };
        if let Some(p) = pct_24h {
            sum_24h += p;
            counted_24h += 1;
        }
    }
    let mut down_list = Vec::with_capacity(down_ids.len());
    for (id, name) in down_ids {
        let raw = db_heartbeats::raw_run(c, id)?;
        let since = raw
            .as_ref()
            .and_then(|r| r.since.as_deref())
            .and_then(super::monitors::rfc3339);
        let message = raw
            .and_then(|r| r.message)
            .map(|m| m.chars().take(SUMMARY_MESSAGE_CHARS).collect::<String>());
        down_list.push((id, name, since, message));
    }
    // Longest outage first; RFC 3339 UTC timestamps sort as text; unknown last.
    down_list.sort_by(|a, b| (a.2.is_none(), &a.2, a.0).cmp(&(b.2.is_none(), &b.2, b.0)));
    down_list.truncate(SUMMARY_DOWN_CAP);
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "monitors": {
            "total": monitors.len(),
            "up": up,
            "down": down,
            "pending": pending,
            "maintenance": maintenance,
            "paused": paused,
            "unknown": unknown,
        },
        "uptime": {
            "days": days,
            "pct": mean_pct(sum, counted),
            "pct_24h": mean_pct(sum_24h, counted_24h),
            "monitors_counted": counted,
        },
        "down": down_list
            .into_iter()
            .map(|(id, name, since, message)| {
                json!({ "id": id, "name": name, "since": since, "message": message })
            })
            .collect::<Vec<_>>(),
    }))
}

async fn agents(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
) -> V1Result<Response> {
    user.require_admin()?;
    let data = state.with_conn(|c| db_agents::list_agents(c)).await?;
    Ok(json_ok(
        &headers,
        &json!({ "data": data, "next_cursor": null }),
    ))
}

async fn get_settings(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
) -> V1Result<Response> {
    user.require_admin()?;
    let s = state.settings().await?;
    Ok(json_ok(&headers, &s))
}

/// Partial update; same rules as the general settings page (via the config importer).
async fn patch_settings(
    State(state): State<AppState>,
    user: CurrentUser,
    Body(input): Body<SettingsDoc>,
) -> V1Result<Response> {
    user.require_admin()?;
    let doc = Document {
        settings: Some(input),
        ..Document::default()
    };
    match config_io::import(&state, doc, Mode::Merge, false).await {
        Ok(_) => {}
        Err(config_io::ImportError::Invalid(plan)) => {
            return Err(V1Error::fields(
                plan.errors.into_iter().map(|e| (e.key, e.message)),
            ));
        }
        Err(config_io::ImportError::Api(e)) => return Err(e.into()),
    }
    tracing::info!(user = %user.username, "settings updated via API");
    let s = state.settings().await?;
    Ok(json_ok(&HeaderMap::new(), &s))
}
