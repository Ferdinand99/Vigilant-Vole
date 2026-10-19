//! Wire format shared by the server endpoints (`api::agents`) and the agent
//! client. Versioned by URL (`/api/agent/v1/...`).
//!
//! - `GET  /api/agent/v1/monitors` → [`MonitorsResponse`]; supports
//!   `If-None-Match` (the response carries a strong `ETag`).
//! - `POST /api/agent/v1/results` with a JSON array of [`ResultItem`]
//!   (≤ [`MAX_BATCH`]) → [`ResultsResponse`].
//!
//! Both require `Authorization: Bearer vva_…` and accept the optional
//! [`VERSION_HEADER`] / [`NAME_HEADER`] headers.

use serde::{Deserialize, Serialize};

use crate::db::models::{HeartbeatStatus, Monitor};

pub const MONITORS_PATH: &str = "/api/agent/v1/monitors";
pub const RESULTS_PATH: &str = "/api/agent/v1/results";
/// Most results accepted in one POST.
pub const MAX_BATCH: usize = 500;
/// Longest result message stored (characters).
pub const MAX_MESSAGE_CHARS: usize = 500;
/// Agent version, reported on every request.
pub const VERSION_HEADER: &str = "x-agent-version";
/// Agent display name (hostname by default), reported on every request.
pub const NAME_HEADER: &str = "x-agent-name";
/// How often agents should re-fetch their monitor list.
pub const POLL_SECONDS: u64 = 30;

/// Who the server thinks this agent is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: i64,
    pub name: String,
    pub location: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorsResponse {
    pub agent: AgentInfo,
    /// Suggested poll interval in seconds.
    pub poll_seconds: u64,
    /// Full monitor configs (including the monitor's own credentials, which
    /// the agent needs to run the check). Push tokens are stripped.
    pub monitors: Vec<Monitor>,
}

/// One raw check result (before upside-down; the server applies it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultItem {
    pub monitor_id: i64,
    pub status: HeartbeatStatus,
    #[serde(default)]
    pub response_time_ms: Option<i64>,
    #[serde(default)]
    pub message: Option<String>,
    /// When the check ran (UTC `YYYY-MM-DD HH:MM:SS`). Results older than
    /// [`MAX_RESULT_AGE_SECS`] (or 2× the monitor interval, if longer) are
    /// dropped as stale, since heartbeats are recorded at arrival time.
    #[serde(default)]
    pub checked_at: Option<String>,
}

/// Minimum age after which a buffered result is considered stale.
pub const MAX_RESULT_AGE_SECS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejected {
    pub monitor_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultsResponse {
    pub accepted: usize,
    pub rejected: Vec<Rejected>,
}

/// Current UTC time in the database text format.
pub fn now_db() -> String {
    format_db(time::OffsetDateTime::now_utc())
}

pub fn format_db(t: time::OffsetDateTime) -> String {
    crate::monitor::state::fmt_db_time(t)
}

/// Parse `YYYY-MM-DD HH:MM:SS` (or RFC 3339) as UTC.
pub fn parse_db(s: &str) -> Option<time::OffsetDateTime> {
    let s = s.trim();
    if let Ok(t) = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339) {
        return Some(t);
    }
    if s.len() != 19 {
        return None;
    }
    crate::monitor::state::parse_db_time(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_time_round_trips() {
        let t = parse_db("2026-10-19 14:03:09").unwrap();
        assert_eq!(format_db(t), "2026-10-19 14:03:09");
        assert_eq!(format_db(parse_db("2026-10-19T16:03:09+02:00").unwrap()), "2026-10-19 14:03:09");
        assert!(parse_db("yesterday").is_none());
    }

    #[test]
    fn result_items_accept_minimal_json() {
        let item: ResultItem = serde_json::from_str(r#"{"monitor_id":3,"status":"up"}"#).unwrap();
        assert_eq!(item.status, HeartbeatStatus::Up);
        assert!(item.checked_at.is_none());
    }
}
