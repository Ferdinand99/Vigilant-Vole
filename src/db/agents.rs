//! Remote probe agents (`agents`). Owned by the integrations agent.
//!
//! Tokens look like `vva_` + 40 base62 characters. Only the SHA-256 hex digest
//! is stored (`token_hash`) together with a short display/lookup `prefix`.

use rand::RngExt;
use rand::distr::Alphanumeric;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::models::Monitor;
use super::monitors::{MONITOR_COLUMNS, row_to_monitor};

/// Every agent token starts with this.
pub const TOKEN_PREFIX: &str = "vva_";
/// Random characters after [`TOKEN_PREFIX`].
const TOKEN_RANDOM_LEN: usize = 40;
/// Characters of the token kept in `agents.prefix` (`vva_` + 8).
const DISPLAY_PREFIX_LEN: usize = 12;
/// An agent counts as online when it was seen within this many seconds.
pub const ONLINE_WITHIN_SECS: i64 = 90;

/// A row of `agents` (never includes the token hash).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Agent {
    pub id: i64,
    pub name: String,
    pub location: String,
    pub prefix: String,
    pub version: Option<String>,
    /// UTC `YYYY-MM-DD HH:MM:SS`; `None` until the agent first connects.
    pub last_seen_at: Option<String>,
    pub active: bool,
    pub created_at: String,
}

/// An agent plus what the settings page and `/metrics` show about it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentSummary {
    pub agent: Agent,
    /// Seen within [`ONLINE_WITHIN_SECS`] (and active).
    pub online: bool,
    /// Seconds since `last_seen_at` (None if never seen).
    pub seen_secs_ago: Option<i64>,
    /// Distinct monitors this agent reported on in the last 15 minutes.
    pub reporting_monitors: i64,
}

/// A freshly generated token: the plaintext (show once), its display prefix
/// and the hash to store.
#[derive(Debug, Clone)]
pub struct NewToken {
    pub token: String,
    pub prefix: String,
    pub hash: String,
}

/// Generate a new random agent token.
pub fn generate_token() -> NewToken {
    let random: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(TOKEN_RANDOM_LEN)
        .map(char::from)
        .collect();
    let token = format!("{TOKEN_PREFIX}{random}");
    NewToken {
        prefix: token_prefix(&token),
        hash: hash_token(&token),
        token,
    }
}

/// SHA-256 hex digest of a token.
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// The display/lookup prefix of a token (first 12 characters).
pub fn token_prefix(token: &str) -> String {
    token.chars().take(DISPLAY_PREFIX_LEN).collect()
}

/// Constant-time byte comparison (length is not secret).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const AGENT_COLUMNS: &str = "id, name, location, prefix, version, last_seen_at, active, created_at";

fn row_to_agent(row: &Row) -> rusqlite::Result<Agent> {
    Ok(Agent {
        id: row.get("id")?,
        name: row.get("name")?,
        location: row.get("location")?,
        prefix: row.get("prefix")?,
        version: row.get("version")?,
        last_seen_at: row.get("last_seen_at")?,
        active: row.get("active")?,
        created_at: row.get("created_at")?,
    })
}

/// Insert an agent with the given token hash. Returns its id.
pub fn insert_agent(conn: &Connection, name: &str, location: &str, token: &NewToken) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO agents (name, location, prefix, token_hash) VALUES (?1, ?2, ?3, ?4)",
        params![name, location, token.prefix, token.hash],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_agent(conn: &Connection, id: i64) -> rusqlite::Result<Option<Agent>> {
    conn.query_row(
        &format!("SELECT {AGENT_COLUMNS} FROM agents WHERE id = ?1"),
        params![id],
        row_to_agent,
    )
    .optional()
}

/// Resolve a plaintext token to its agent (active or not). Candidates are
/// looked up by prefix and the hashes compared in constant time.
pub fn find_by_token(conn: &Connection, token: &str) -> rusqlite::Result<Option<Agent>> {
    if !token.starts_with(TOKEN_PREFIX) || token.len() > 200 {
        return Ok(None);
    }
    let hash = hash_token(token);
    let mut stmt = conn.prepare(&format!(
        "SELECT {AGENT_COLUMNS}, token_hash FROM agents WHERE prefix = ?1"
    ))?;
    let mut rows = stmt.query(params![token_prefix(token)])?;
    let mut found = None;
    while let Some(row) = rows.next()? {
        let stored: String = row.get("token_hash")?;
        // Compare every candidate so timing doesn't depend on which one matched.
        if constant_time_eq(stored.as_bytes(), hash.as_bytes()) {
            found = Some(row_to_agent(row)?);
        }
    }
    Ok(found)
}

/// Record that the agent just connected (and which version it runs).
pub fn touch(conn: &Connection, id: i64, version: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE agents SET last_seen_at = datetime('now'), version = COALESCE(?2, version) WHERE id = ?1",
        params![id, version],
    )?;
    Ok(())
}

/// Replace the agent's token; the old one stops working immediately.
pub fn rotate_token(conn: &Connection, id: i64, token: &NewToken) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE agents SET prefix = ?2, token_hash = ?3 WHERE id = ?1",
        params![id, token.prefix, token.hash],
    )?;
    Ok(n > 0)
}

pub fn set_active(conn: &Connection, id: i64, active: bool) -> rusqlite::Result<bool> {
    let n = conn.execute("UPDATE agents SET active = ?2 WHERE id = ?1", params![id, active])?;
    Ok(n > 0)
}

/// Delete an agent (its heartbeats cascade).
pub fn delete_agent(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM agents WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// Every agent with online state and recent activity, newest first.
pub fn list_agents(conn: &Connection) -> rusqlite::Result<Vec<AgentSummary>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {AGENT_COLUMNS},
            CAST(strftime('%s', 'now') AS INTEGER) - CAST(strftime('%s', last_seen_at) AS INTEGER) AS seen_ago,
            (SELECT COUNT(DISTINCT h.monitor_id) FROM heartbeats h
              WHERE h.agent_id = agents.id AND h.checked_at >= datetime('now', '-15 minutes')) AS reporting
         FROM agents ORDER BY created_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map([], |row| {
        let agent = row_to_agent(row)?;
        let seen_secs_ago: Option<i64> = row.get("seen_ago")?;
        Ok(AgentSummary {
            online: agent.active && seen_secs_ago.is_some_and(|s| s <= ONLINE_WITHIN_SECS),
            seen_secs_ago,
            reporting_monitors: row.get("reporting")?,
            agent,
        })
    })?;
    rows.collect()
}

/// Active monitors that remote agents should run: `run_on_agents = 1` and an
/// type agents can run (never push, group or real browser).
pub fn list_agent_monitors(conn: &Connection) -> rusqlite::Result<Vec<Monitor>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MONITOR_COLUMNS} FROM monitors
         WHERE active = 1 AND run_on_agents = 1 AND monitor_type NOT IN ('push', 'group', 'real_browser')
         ORDER BY id"
    ))?;
    let rows = stmt.query_map([], row_to_monitor)?;
    rows.collect()
}

/// Number of monitors agents currently run.
pub fn count_agent_monitors(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM monitors
         WHERE active = 1 AND run_on_agents = 1 AND monitor_type NOT IN ('push', 'group', 'real_browser')",
        [],
        |r| r.get(0),
    )
}

/// Latest result per (agent, monitor) within the last hour.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentResult {
    pub agent_id: i64,
    pub monitor_id: i64,
    pub status: String,
    pub response_time_ms: Option<i64>,
}

pub fn latest_agent_results(conn: &Connection) -> rusqlite::Result<Vec<AgentResult>> {
    let mut stmt = conn.prepare(
        "SELECT h.agent_id, h.monitor_id, h.status, h.response_time_ms FROM heartbeats h
         WHERE h.id IN (
            SELECT MAX(id) FROM heartbeats
            WHERE agent_id IS NOT NULL AND checked_at >= datetime('now', '-1 hour')
            GROUP BY agent_id, monitor_id)
         ORDER BY h.agent_id, h.monitor_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(AgentResult {
            agent_id: r.get(0)?,
            monitor_id: r.get(1)?,
            status: r.get(2)?,
            response_time_ms: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// Is `monitor_id` a monitor agents may report on right now?
pub fn is_agent_monitor(monitor: &Monitor) -> bool {
    monitor.active && monitor.run_on_agents && monitor.monitor_type.runs_on_agents()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{MonitorType, NewMonitor};
    use crate::db::{monitors as db_monitors, test_conn};

    #[test]
    fn tokens_have_prefix_and_hash() {
        let t = generate_token();
        assert!(t.token.starts_with(TOKEN_PREFIX));
        assert_eq!(t.token.len(), TOKEN_PREFIX.len() + TOKEN_RANDOM_LEN);
        assert_eq!(t.prefix.len(), DISPLAY_PREFIX_LEN);
        assert_eq!(t.hash, hash_token(&t.token));
        assert_eq!(t.hash.len(), 64);
        assert_ne!(generate_token().token, t.token);
    }

    #[test]
    fn constant_time_eq_compares_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn find_by_token_round_trip_and_rotation() {
        let conn = test_conn();
        let t = generate_token();
        let id = insert_agent(&conn, "Frankfurt", "eu-central", &t).unwrap();
        assert_eq!(find_by_token(&conn, &t.token).unwrap().map(|a| a.id), Some(id));
        assert!(find_by_token(&conn, "vva_wrong").unwrap().is_none());
        assert!(find_by_token(&conn, "vv_notanagent").unwrap().is_none());

        let t2 = generate_token();
        assert!(rotate_token(&conn, id, &t2).unwrap());
        assert!(find_by_token(&conn, &t.token).unwrap().is_none());
        assert_eq!(find_by_token(&conn, &t2.token).unwrap().map(|a| a.id), Some(id));
    }

    #[test]
    fn list_reports_online_state() {
        let conn = test_conn();
        let id = insert_agent(&conn, "a", "", &generate_token()).unwrap();
        let list = list_agents(&conn).unwrap();
        assert!(!list[0].online);
        touch(&conn, id, Some("0.1.0")).unwrap();
        let list = list_agents(&conn).unwrap();
        assert!(list[0].online);
        assert_eq!(list[0].agent.version.as_deref(), Some("0.1.0"));
        set_active(&conn, id, false).unwrap();
        assert!(!list_agents(&conn).unwrap()[0].online);
    }

    #[test]
    fn agent_monitors_are_filtered() {
        let conn = test_conn();
        let base = NewMonitor {
            name: "tcp".into(),
            monitor_type: MonitorType::Tcp,
            target: "localhost:1".into(),
            run_on_agents: true,
            ..NewMonitor::default()
        };
        let on = db_monitors::insert_monitor(&conn, &base).unwrap();
        db_monitors::insert_monitor(&conn, &NewMonitor { run_on_agents: false, ..base.clone() }).unwrap();
        db_monitors::insert_monitor(&conn, &NewMonitor { active: false, ..base.clone() }).unwrap();
        db_monitors::insert_monitor(
            &conn,
            &NewMonitor { monitor_type: MonitorType::Push, target: String::new(), ..base.clone() },
        )
        .unwrap();
        let ids: Vec<i64> = list_agent_monitors(&conn).unwrap().iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![on]);
        assert_eq!(count_agent_monitors(&conn).unwrap(), 1);
    }
}
