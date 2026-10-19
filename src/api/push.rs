//! Push monitor endpoint. Owned by the checkers agent.
//!
//! `GET|POST /api/push/{token}?status=up|down&msg=…&ping=…`
//!
//! - `status`: `up` (default; also `ok`, `1`, `true`) or `down` (also
//!   `fail`, `error`, `0`, `false`); anything else → 400.
//! - `msg`: optional message (max 500 chars); `ping`: optional response time
//!   in ms (decimals allowed).
//! - The token must belong to an active monitor of type `push`, else 404.
//! - Naive rate limit: at most one accepted push per token per second
//!   (extra pushes → 429).
//!
//! Responses are JSON: `{"ok":true}` / `{"ok":false,"msg":"…"}`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rand::RngExt;
use rand::distr::Alphanumeric;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::db::models::{CheckOutcome, HeartbeatStatus};

const MIN_PUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_MSG_CHARS: usize = 500;
const PUSH_TOKEN_LEN: usize = 32;

/// Unauthenticated routes.
pub fn public_router() -> Router<AppState> {
    Router::new().route("/api/push/{token}", get(push).post(push))
}

/// A new random push token: 32 URL-safe alphanumeric characters.
pub fn generate_push_token() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(PUSH_TOKEN_LEN)
        .map(char::from)
        .collect()
}

#[derive(Debug, Default, Deserialize)]
pub struct PushParams {
    status: Option<String>,
    msg: Option<String>,
    ping: Option<String>,
}

fn reply(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "ok": false, "msg": msg }))).into_response()
}

/// Parse the query into an outcome. `Err` = 400 message.
pub fn parse_push(params: &PushParams) -> Result<CheckOutcome, String> {
    let status = match params
        .status
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
    {
        None => HeartbeatStatus::Up,
        Some(s) => match s.as_str() {
            "" | "up" | "ok" | "1" | "true" => HeartbeatStatus::Up,
            "down" | "fail" | "failed" | "error" | "0" | "false" => HeartbeatStatus::Down,
            other => {
                return Err(format!(
                    "unknown status \"{}\" (use up or down)",
                    crate::monitor::checkers::util::truncate(other, 20)
                ));
            }
        },
    };
    let message = params
        .msg
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| crate::monitor::checkers::util::truncate(m, MAX_MSG_CHARS));
    let response_time_ms = params
        .ping
        .as_deref()
        .and_then(|p| p.trim().parse::<f64>().ok())
        .filter(|p| p.is_finite() && *p >= 0.0 && *p < 1e9)
        .map(|p| p.round() as i64);
    Ok(CheckOutcome {
        status,
        response_time_ms,
        message,
        expiry: None,
    })
}

/// True if this token may push now (and records the attempt).
fn allow(token: &str, now: Instant) -> bool {
    static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let Ok(mut map) = LAST.get_or_init(|| Mutex::new(HashMap::new())).lock() else {
        return true;
    };
    if map.len() > 4096 {
        map.retain(|_, at| now.duration_since(*at) < Duration::from_secs(60));
    }
    match map.get(token) {
        Some(at) if now.duration_since(*at) < MIN_PUSH_INTERVAL => false,
        _ => {
            map.insert(token.to_string(), now);
            true
        }
    }
}

async fn push(
    State(state): State<AppState>,
    Path(token): Path<String>,
    query: Result<Query<PushParams>, QueryRejection>,
) -> Response {
    let params = match query {
        Ok(Query(p)) => p,
        Err(e) => return reply(StatusCode::BAD_REQUEST, &e.body_text()),
    };
    let outcome = match parse_push(&params) {
        Ok(o) => o,
        Err(e) => return reply(StatusCode::BAD_REQUEST, &e),
    };
    if token.is_empty() || token.len() > 128 {
        return reply(StatusCode::NOT_FOUND, "Monitor not found or not active");
    }

    let lookup_token = token.clone();
    let found = state
        .with_conn(move |c| {
            c.query_row(
                "SELECT id FROM monitors WHERE push_token = ?1 AND monitor_type = 'push' AND active = 1",
                [lookup_token],
                |row| row.get::<_, i64>(0),
            )
            .optional()
        })
        .await;
    let monitor_id = match found {
        Ok(Some(id)) => id,
        Ok(None) => return reply(StatusCode::NOT_FOUND, "Monitor not found or not active"),
        Err(err) => {
            tracing::warn!(%err, "push lookup failed");
            return reply(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
        }
    };

    if !allow(&token, Instant::now()) {
        return reply(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many pushes; at most one per second",
        );
    }
    state
        .scheduler
        .ingest_external(monitor_id, outcome, None)
        .await;
    Json(json!({ "ok": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(status: Option<&str>, msg: Option<&str>, ping: Option<&str>) -> PushParams {
        PushParams {
            status: status.map(Into::into),
            msg: msg.map(Into::into),
            ping: ping.map(Into::into),
        }
    }

    #[test]
    fn token_shape() {
        let t = generate_push_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(t, generate_push_token());
    }

    #[test]
    fn default_push_is_up() {
        let o = parse_push(&PushParams::default()).unwrap();
        assert_eq!(o.status, HeartbeatStatus::Up);
        assert_eq!((o.message, o.response_time_ms), (None, None));
    }

    #[test]
    fn push_parameters() {
        let o = parse_push(&params(Some("DOWN"), Some("  disk full "), Some("12.6"))).unwrap();
        assert_eq!(o.status, HeartbeatStatus::Down);
        assert_eq!(o.message.as_deref(), Some("disk full"));
        assert_eq!(o.response_time_ms, Some(13));
        assert!(parse_push(&params(Some("maybe"), None, None)).is_err());
        assert_eq!(
            parse_push(&params(None, None, Some("-5")))
                .unwrap()
                .response_time_ms,
            None
        );
        assert_eq!(
            parse_push(&params(None, None, Some("abc")))
                .unwrap()
                .response_time_ms,
            None
        );
        let long = "x".repeat(1000);
        assert_eq!(
            parse_push(&params(None, Some(&long), None))
                .unwrap()
                .message
                .unwrap()
                .chars()
                .count(),
            MAX_MSG_CHARS + 1
        );
    }

    #[test]
    fn rate_limit_per_token() {
        let now = Instant::now();
        let token = generate_push_token();
        assert!(allow(&token, now));
        assert!(!allow(&token, now + Duration::from_millis(500)));
        assert!(allow(&token, now + Duration::from_millis(1100)));
        assert!(allow(&generate_push_token(), now));
    }

    #[test]
    fn lookup_query_matches_schema() {
        let conn = crate::db::test_conn();
        conn.execute(
            "INSERT INTO monitors (name, monitor_type, target, push_token) VALUES ('p', 'push', '', 'tok')",
            [],
        )
        .unwrap();
        let id: Option<i64> = conn
            .query_row(
                "SELECT id FROM monitors WHERE push_token = ?1 AND monitor_type = 'push' AND active = 1",
                ["tok"],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(id.is_some());
    }
}
