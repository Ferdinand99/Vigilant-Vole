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
//!
//! Compatibility across versions: the server only sends an agent the
//! monitors its [`VERSION_HEADER`] says it can run ([`agent_can_run`]), and
//! the agent reads the list one monitor at a time, skipping what it doesn't
//! understand ([`MonitorsResponse::monitors`]).

use std::collections::HashSet;
use std::sync::Mutex;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::db::models::{HeartbeatStatus, Monitor, MonitorType};

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
    /// Full monitor configs, including the monitor's own credentials (basic
    /// auth, service passwords and secrets, the OAuth2 client secret and the
    /// mTLS private key), which the agent needs to run the check. Push tokens
    /// are stripped. Read leniently: see [`read_monitors`].
    #[serde(deserialize_with = "read_monitors")]
    pub monitors: Vec<Monitor>,
}

/// The first agent version that runs the monitor types and HTTP
/// authentication methods added after 0.1.0. Older agents fail to read a
/// monitor list that contains an unknown type and would check OAuth2/mTLS
/// monitors without their authentication, so the server holds those back.
pub const SERVICE_MONITORS_SINCE: &str = "0.2.0";

/// The first agent version that runs the monitor types of the 0.3.0 batch
/// (SQL Server, MongoDB, and the types that follow in that release).
pub const BATCH_0_3_SINCE: &str = "0.3.0";

/// The agent version that introduced `kind` (`None`: every agent runs it).
/// No wildcard arm, so a new type needs a decision here.
pub fn introduced_in(kind: MonitorType) -> Option<&'static str> {
    use MonitorType::*;
    match kind {
        Http | Keyword | JsonQuery | Tcp | Ping | Dns | Push | Docker | Websocket | DomainExpiry | Ntp | Steam | Group => None,
        Tls | Smtp | Redis | Postgres | Mysql | Mqtt | Kafka | Rabbitmq | Grpc | Radius | Snmp => Some(SERVICE_MONITORS_SINCE),
        SqlServer | Mongodb => Some(BATCH_0_3_SINCE),
        TailscalePing => Some(BATCH_0_3_SINCE),
        GameServer => Some(BATCH_0_3_SINCE),
        // Agents also need `grpc_proto`, which older ones don't read.
        GrpcKeyword => Some(BATCH_0_3_SINCE),
        // Never sent to agents (see `agent_can_run`); an agent that got one
        // anyway from a 0.3.0 server would at least read the list.
        RealBrowser => Some(BATCH_0_3_SINCE),
    }
}

/// The agent version `m` needs, if not every agent can run it.
pub fn version_needed(m: &Monitor) -> Option<&'static str> {
    // Older agents read `?version=3` as an unsupported version.
    if m.monitor_type == MonitorType::Snmp && crate::monitor::checkers::snmp::is_v3(&m.target) {
        return Some(BATCH_0_3_SINCE);
    }
    introduced_in(m.monitor_type).or(matches!(m.auth_method.as_str(), "oauth2" | "mtls").then_some(SERVICE_MONITORS_SINCE))
}

/// `major.minor.patch` of a version string (a `-pre` / `+build` suffix is ignored).
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

/// Whether an agent reporting `agent_version` can run `m`. An agent that
/// reports no (readable) version counts as the oldest. A requirement newer
/// than this build counts as this build's own version: unreleased builds
/// still carry the last release's number until the release bumps it.
pub fn agent_can_run(m: &Monitor, agent_version: Option<&str>) -> bool {
    // Real-browser checks drive the server's remote browsers: no agent runs them.
    if !m.monitor_type.runs_on_agents() {
        return false;
    }
    let Some(needed) = version_needed(m).and_then(parse_version) else {
        return true;
    };
    let needed = parse_version(env!("CARGO_PKG_VERSION")).map_or(needed, |own| needed.min(own));
    agent_version.and_then(parse_version).is_some_and(|v| v >= needed)
}

/// Reads the monitor list one monitor at a time: one this agent can't
/// understand (a type from a newer server) is skipped with a warning,
/// once per type, instead of failing the whole list. Monitors from servers
/// that predate `auth_method` get it from their user name, as those servers
/// meant it.
fn read_monitors<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Monitor>, D::Error> {
    Ok(Vec::<Value>::deserialize(d)?.into_iter().filter_map(read_monitor).collect())
}

fn read_monitor(mut value: Value) -> Option<Monitor> {
    if let Value::Object(obj) = &mut value
        && !obj.contains_key("auth_method")
    {
        let method = crate::monitor::checkers::http_auth::implied_method(obj.get("basic_auth_user").and_then(Value::as_str));
        obj.insert("auth_method".into(), method.into());
    }
    let id = value.get("id").and_then(Value::as_i64);
    let kind = value.get("monitor_type").and_then(Value::as_str).unwrap_or("?").to_string();
    match serde_json::from_value::<Monitor>(value) {
        Ok(m) => Some(m),
        Err(err) => {
            static WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
            let first = WARNED.lock().map(|mut w| w.get_or_insert_with(HashSet::new).insert(kind.clone())).unwrap_or(true);
            if first {
                tracing::warn!(monitor_id = ?id, monitor_type = %kind, %err, "skipping a monitor this agent can't run; upgrade the agent");
            }
            None
        }
    }
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

    fn listing(monitors: Value) -> Value {
        serde_json::json!({"agent": {"id": 1, "name": "a", "location": "eu"}, "poll_seconds": 30, "monitors": monitors})
    }

    fn monitor_json(id: i64, kind: &str) -> Value {
        let mut m = crate::monitor::checkers::test_support::monitor(MonitorType::Tcp, "db:5432");
        m.id = id;
        let mut v = serde_json::to_value(m).unwrap();
        v["monitor_type"] = kind.into();
        v
    }

    #[test]
    fn unknown_monitor_types_are_skipped_not_fatal() {
        let list = listing(serde_json::json!([monitor_json(1, "tcp"), monitor_json(2, "quantum_link"), monitor_json(3, "redis")]));
        let parsed: MonitorsResponse = serde_json::from_value(list).unwrap();
        assert_eq!(parsed.monitors.iter().map(|m| m.id).collect::<Vec<_>>(), [1, 3]);
    }

    #[test]
    fn monitors_from_servers_without_auth_method_keep_basic_auth() {
        let mut old = monitor_json(1, "http");
        old.as_object_mut().unwrap().remove("auth_method");
        old["basic_auth_user"] = "bot".into();
        let mut plain = monitor_json(2, "http");
        plain.as_object_mut().unwrap().remove("auth_method");
        let mut off = monitor_json(3, "http");
        off["auth_method"] = "none".into();
        off["basic_auth_user"] = "bot".into();
        let parsed: MonitorsResponse = serde_json::from_value(listing(serde_json::json!([old, plain, off]))).unwrap();
        let methods: Vec<&str> = parsed.monitors.iter().map(|m| m.auth_method.as_str()).collect();
        assert_eq!(methods, ["basic", "none", "none"]);
    }

    #[test]
    fn new_types_and_auth_methods_need_a_new_enough_agent() {
        let mut m = crate::monitor::checkers::test_support::monitor(MonitorType::Http, "https://x.io");
        for v in [None, Some("0.0.9"), Some("garbage"), Some("9.9.9")] {
            assert!(agent_can_run(&m, v), "{v:?}");
        }
        m.auth_method = "oauth2".into();
        for (v, ok) in [(None, false), (Some("0.0.9"), false), (Some("bad"), false), (Some("9.9.9"), true), (Some("v9.0.0-rc.1"), true)] {
            assert_eq!(agent_can_run(&m, v), ok, "{v:?}");
        }
        m.auth_method = "basic".into();
        m.monitor_type = MonitorType::Postgres;
        assert!(!agent_can_run(&m, Some("0.0.9")) && agent_can_run(&m, Some(SERVICE_MONITORS_SINCE)));
        // This build's own agents always qualify, released or not.
        assert!(agent_can_run(&m, Some(env!("CARGO_PKG_VERSION"))));
        for kind in [MonitorType::SqlServer, MonitorType::Mongodb, MonitorType::GameServer] {
            m.monitor_type = kind;
            assert_eq!(introduced_in(kind), Some(BATCH_0_3_SINCE));
            assert!(!agent_can_run(&m, Some("0.1.9")) && !agent_can_run(&m, None));
            assert!(agent_can_run(&m, Some("0.3.0")) && agent_can_run(&m, Some(env!("CARGO_PKG_VERSION"))));
        }
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
    }

    #[test]
    fn snmpv3_and_tailscale_ping_need_a_batch_agent() {
        let mut m = crate::monitor::checkers::test_support::monitor(MonitorType::Snmp, "snmp://router.local?version=2c");
        assert_eq!(version_needed(&m), Some(SERVICE_MONITORS_SINCE));
        m.target = "snmp://router.local/1.3.6.1.2.1.1.3.0?version=3&auth=sha".into();
        assert_eq!(version_needed(&m), Some(BATCH_0_3_SINCE));
        assert!(!agent_can_run(&m, Some("0.1.9")) && agent_can_run(&m, Some("0.3.0")));
        let ts = crate::monitor::checkers::test_support::monitor(MonitorType::TailscalePing, "nas");
        assert_eq!(version_needed(&ts), Some(BATCH_0_3_SINCE));
        assert!(!agent_can_run(&ts, Some("0.1.9")) && agent_can_run(&ts, Some("0.3.0")));
    }

    #[test]
    fn real_browser_monitors_never_go_to_agents() {
        let m = crate::monitor::checkers::test_support::monitor(MonitorType::RealBrowser, "https://x.io");
        assert_eq!(introduced_in(MonitorType::RealBrowser), Some(BATCH_0_3_SINCE));
        for v in [None, Some("0.3.0"), Some(env!("CARGO_PKG_VERSION")), Some("99.0.0")] {
            assert!(!agent_can_run(&m, v), "{v:?}");
        }
    }

    #[test]
    fn result_items_accept_minimal_json() {
        let item: ResultItem = serde_json::from_str(r#"{"monitor_id":3,"status":"up"}"#).unwrap();
        assert_eq!(item.status, HeartbeatStatus::Up);
        assert!(item.checked_at.is_none());
    }
}
