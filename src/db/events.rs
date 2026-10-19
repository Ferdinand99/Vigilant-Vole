//! Monitor state-change history (`monitor_events`). Owned by the engine agent.
//!
//! Two shapes of event live in the table:
//! - **interval events** (`down`, `maintenance`, `flapping`, `paused`) are
//!   opened with `ended_at = NULL` and closed later, which fills `ended_at` and
//!   `duration_seconds`. An open `down` event is an ongoing outage.
//! - **point events** (`cert_expiring`, `domain_expiring`, `resumed`, `up`) are
//!   written already closed (`ended_at = started_at`, duration 0).
//!
//! The engine writes outage/maintenance/flapping/expiry events itself. The
//! monitors agent calls [`log_paused`] / [`log_resumed`] from the pause and
//! resume handlers. All timestamps are UTC `YYYY-MM-DD HH:MM:SS`.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

/// `monitor_events.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorEventKind {
    Down,
    Up,
    Maintenance,
    Paused,
    Resumed,
    CertExpiring,
    DomainExpiring,
    Flapping,
}

impl MonitorEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MonitorEventKind::Down => "down",
            MonitorEventKind::Up => "up",
            MonitorEventKind::Maintenance => "maintenance",
            MonitorEventKind::Paused => "paused",
            MonitorEventKind::Resumed => "resumed",
            MonitorEventKind::CertExpiring => "cert_expiring",
            MonitorEventKind::DomainExpiring => "domain_expiring",
            MonitorEventKind::Flapping => "flapping",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "down" => MonitorEventKind::Down,
            "up" => MonitorEventKind::Up,
            "maintenance" => MonitorEventKind::Maintenance,
            "paused" => MonitorEventKind::Paused,
            "resumed" => MonitorEventKind::Resumed,
            "cert_expiring" => MonitorEventKind::CertExpiring,
            "domain_expiring" => MonitorEventKind::DomainExpiring,
            "flapping" => MonitorEventKind::Flapping,
            _ => return None,
        })
    }

    /// Human-readable label for timelines.
    pub fn label(self) -> &'static str {
        match self {
            MonitorEventKind::Down => crate::i18n::t("status.down"),
            MonitorEventKind::Up => crate::i18n::t("status.up"),
            MonitorEventKind::Maintenance => crate::i18n::t("status.maintenance"),
            MonitorEventKind::Paused => crate::i18n::t("status.paused"),
            MonitorEventKind::Resumed => crate::i18n::t("event_kind.resumed"),
            MonitorEventKind::CertExpiring => crate::i18n::t("event_kind.cert_expiring"),
            MonitorEventKind::DomainExpiring => crate::i18n::t("event_kind.domain_expiring"),
            MonitorEventKind::Flapping => crate::i18n::t("event_kind.flapping"),
        }
    }

    /// Whether this kind signals a problem (for colour/severity in the UI).
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            MonitorEventKind::Down
                | MonitorEventKind::CertExpiring
                | MonitorEventKind::DomainExpiring
                | MonitorEventKind::Flapping
        )
    }
}

impl fmt::Display for MonitorEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A row of `monitor_events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorEvent {
    pub id: i64,
    pub monitor_id: i64,
    pub kind: MonitorEventKind,
    pub message: Option<String>,
    /// UTC, `YYYY-MM-DD HH:MM:SS`.
    pub started_at: String,
    /// `None` while the event is ongoing.
    pub ended_at: Option<String>,
    pub duration_seconds: Option<i64>,
}

impl MonitorEvent {
    /// Ongoing (not yet closed).
    pub fn is_open(&self) -> bool {
        self.ended_at.is_none()
    }
}

/// An event plus the name of its monitor (for global timelines and dashboards).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorEventWithName {
    #[serde(flatten)]
    pub event: MonitorEvent,
    pub monitor_name: String,
}

const EVENT_COLUMNS: &str = "e.id, e.monitor_id, e.kind, e.message, e.started_at, e.ended_at, e.duration_seconds";

/// Kinds closed when a monitor is paused (it is no longer observed).
const CLOSED_ON_PAUSE: [MonitorEventKind; 3] = [
    MonitorEventKind::Down,
    MonitorEventKind::Maintenance,
    MonitorEventKind::Flapping,
];

pub fn row_to_event(row: &Row) -> rusqlite::Result<MonitorEvent> {
    let kind: String = row.get("kind")?;
    Ok(MonitorEvent {
        id: row.get("id")?,
        monitor_id: row.get("monitor_id")?,
        // The CHECK constraint makes an unknown kind impossible; map defensively.
        kind: MonitorEventKind::parse(&kind).unwrap_or(MonitorEventKind::Down),
        message: row.get("message")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        duration_seconds: row.get("duration_seconds")?,
    })
}

fn row_to_event_with_name(row: &Row) -> rusqlite::Result<MonitorEventWithName> {
    Ok(MonitorEventWithName {
        event: row_to_event(row)?,
        monitor_name: row.get("monitor_name")?,
    })
}

/// Opens an interval event. `started_at = None` means now.
pub fn open(
    conn: &Connection,
    monitor_id: i64,
    kind: MonitorEventKind,
    message: Option<&str>,
    started_at: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO monitor_events (monitor_id, kind, message, started_at)
         VALUES (?1, ?2, ?3, COALESCE(?4, datetime('now')))",
        params![monitor_id, kind.as_str(), message, started_at],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Closes every open event of `kind` for a monitor. `ended_at = None` means
/// now. Returns how many were closed.
pub fn close_open(
    conn: &Connection,
    monitor_id: i64,
    kind: MonitorEventKind,
    ended_at: Option<&str>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE monitor_events
         SET ended_at = COALESCE(?3, datetime('now')),
             duration_seconds = MAX(0, CAST(strftime('%s', COALESCE(?3, datetime('now'))) AS INTEGER)
                                     - CAST(strftime('%s', started_at) AS INTEGER))
         WHERE monitor_id = ?1 AND kind = ?2 AND ended_at IS NULL",
        params![monitor_id, kind.as_str(), ended_at],
    )
}

/// Writes an already-closed point event (`ended_at = started_at`). `at = None` means now.
pub fn log_point(
    conn: &Connection,
    monitor_id: i64,
    kind: MonitorEventKind,
    message: Option<&str>,
    at: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO monitor_events (monitor_id, kind, message, started_at, ended_at, duration_seconds)
         VALUES (?1, ?2, ?3, COALESCE(?4, datetime('now')), COALESCE(?4, datetime('now')), 0)",
        params![monitor_id, kind.as_str(), message, at],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The newest open event of `kind` for a monitor, if any.
pub fn find_open(conn: &Connection, monitor_id: i64, kind: MonitorEventKind) -> rusqlite::Result<Option<MonitorEvent>> {
    conn.query_row(
        &format!(
            "SELECT {EVENT_COLUMNS} FROM monitor_events e
             WHERE e.monitor_id = ?1 AND e.kind = ?2 AND e.ended_at IS NULL
             ORDER BY e.started_at DESC, e.id DESC LIMIT 1"
        ),
        params![monitor_id, kind.as_str()],
        row_to_event,
    )
    .optional()
}

/// Call when a monitor is paused: closes its ongoing outage / maintenance /
/// flapping events (it is no longer observed) and opens a `paused` event.
/// Idempotent: pausing an already-paused monitor doesn't open a second event.
pub fn log_paused(conn: &Connection, monitor_id: i64) -> rusqlite::Result<()> {
    for kind in CLOSED_ON_PAUSE {
        close_open(conn, monitor_id, kind, None)?;
    }
    if find_open(conn, monitor_id, MonitorEventKind::Paused)?.is_none() {
        open(conn, monitor_id, MonitorEventKind::Paused, None, None)?;
    }
    Ok(())
}

/// Call when a monitor is resumed: closes the open `paused` event (its
/// duration is how long the monitor was paused). If there was none (e.g.
/// paused before event tracking existed) a `resumed` point event is logged.
pub fn log_resumed(conn: &Connection, monitor_id: i64) -> rusqlite::Result<()> {
    if close_open(conn, monitor_id, MonitorEventKind::Paused, None)? == 0 {
        log_point(conn, monitor_id, MonitorEventKind::Resumed, None, None)?;
    }
    Ok(())
}

/// A monitor's events, newest first.
pub fn list_for_monitor(conn: &Connection, monitor_id: i64, limit: i64) -> rusqlite::Result<Vec<MonitorEvent>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM monitor_events e
         WHERE e.monitor_id = ?1
         ORDER BY e.started_at DESC, e.id DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![monitor_id, limit], row_to_event)?;
    rows.collect()
}

/// Events across all monitors, newest first, with the monitor's name.
pub fn list_recent(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<MonitorEventWithName>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {EVENT_COLUMNS}, m.name AS monitor_name
         FROM monitor_events e JOIN monitors m ON m.id = e.monitor_id
         ORDER BY e.started_at DESC, e.id DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![limit], row_to_event_with_name)?;
    rows.collect()
}

/// Ongoing outages (open `down` events) of active monitors, newest first.
pub fn open_incidents(conn: &Connection) -> rusqlite::Result<Vec<MonitorEventWithName>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {EVENT_COLUMNS}, m.name AS monitor_name
         FROM monitor_events e JOIN monitors m ON m.id = e.monitor_id
         WHERE e.kind = 'down' AND e.ended_at IS NULL AND m.active = 1
         ORDER BY e.started_at DESC, e.id DESC"
    ))?;
    let rows = stmt.query_map([], row_to_event_with_name)?;
    rows.collect()
}

/// Deletes closed events that started before `cutoff` (UTC timestamp).
/// Open events are kept regardless of age. Returns the number deleted.
pub fn prune_closed_before(conn: &Connection, cutoff: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM monitor_events WHERE ended_at IS NOT NULL AND started_at < ?1",
        params![cutoff],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewMonitor;
    use crate::db::{monitors, test_conn};

    fn monitor(conn: &Connection, name: &str) -> i64 {
        monitors::insert_monitor(
            conn,
            &NewMonitor {
                name: name.into(),
                target: "https://example.com".into(),
                ..NewMonitor::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn kind_round_trips_through_its_string_form() {
        for kind in [
            MonitorEventKind::Down,
            MonitorEventKind::Up,
            MonitorEventKind::Maintenance,
            MonitorEventKind::Paused,
            MonitorEventKind::Resumed,
            MonitorEventKind::CertExpiring,
            MonitorEventKind::DomainExpiring,
            MonitorEventKind::Flapping,
        ] {
            assert_eq!(MonitorEventKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn close_open_fills_end_and_duration() {
        let conn = test_conn();
        let id = monitor(&conn, "a");
        open(
            &conn,
            id,
            MonitorEventKind::Down,
            Some("timeout"),
            Some("2026-01-01 10:00:00"),
        )
        .unwrap();
        let closed = close_open(&conn, id, MonitorEventKind::Down, Some("2026-01-01 10:05:30")).unwrap();
        assert_eq!(closed, 1);
        let events = list_for_monitor(&conn, id, 10).unwrap();
        assert_eq!(events[0].ended_at.as_deref(), Some("2026-01-01 10:05:30"));
        assert_eq!(events[0].duration_seconds, Some(330));
        assert!(find_open(&conn, id, MonitorEventKind::Down).unwrap().is_none());
    }

    #[test]
    fn close_open_only_touches_the_given_kind() {
        let conn = test_conn();
        let id = monitor(&conn, "a");
        open(&conn, id, MonitorEventKind::Down, None, None).unwrap();
        open(&conn, id, MonitorEventKind::Flapping, None, None).unwrap();
        close_open(&conn, id, MonitorEventKind::Flapping, None).unwrap();
        assert!(find_open(&conn, id, MonitorEventKind::Down).unwrap().is_some());
    }

    #[test]
    fn pause_closes_outage_and_opens_one_paused_event() {
        let conn = test_conn();
        let id = monitor(&conn, "a");
        open(&conn, id, MonitorEventKind::Down, None, None).unwrap();
        log_paused(&conn, id).unwrap();
        log_paused(&conn, id).unwrap();
        assert!(find_open(&conn, id, MonitorEventKind::Down).unwrap().is_none());
        let open_paused: Vec<_> = list_for_monitor(&conn, id, 10)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == MonitorEventKind::Paused && e.is_open())
            .collect();
        assert_eq!(open_paused.len(), 1);
    }

    #[test]
    fn resume_closes_paused_event() {
        let conn = test_conn();
        let id = monitor(&conn, "a");
        log_paused(&conn, id).unwrap();
        log_resumed(&conn, id).unwrap();
        let events = list_for_monitor(&conn, id, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, MonitorEventKind::Paused);
        assert!(!events[0].is_open());
    }

    #[test]
    fn resume_without_pause_logs_point_event() {
        let conn = test_conn();
        let id = monitor(&conn, "a");
        log_resumed(&conn, id).unwrap();
        let events = list_for_monitor(&conn, id, 10).unwrap();
        assert_eq!(events[0].kind, MonitorEventKind::Resumed);
        assert_eq!(events[0].duration_seconds, Some(0));
    }

    #[test]
    fn open_incidents_lists_ongoing_outages_of_active_monitors_with_names() {
        let conn = test_conn();
        let a = monitor(&conn, "alpha");
        let b = monitor(&conn, "beta");
        open(&conn, a, MonitorEventKind::Down, Some("boom"), None).unwrap();
        open(&conn, b, MonitorEventKind::Down, None, None).unwrap();
        conn.execute("UPDATE monitors SET active = 0 WHERE id = ?1", [b])
            .unwrap();
        let incidents = open_incidents(&conn).unwrap();
        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].monitor_name, "alpha");
        assert_eq!(incidents[0].event.message.as_deref(), Some("boom"));
    }

    #[test]
    fn list_recent_is_newest_first_across_monitors() {
        let conn = test_conn();
        let a = monitor(&conn, "alpha");
        let b = monitor(&conn, "beta");
        log_point(
            &conn,
            a,
            MonitorEventKind::CertExpiring,
            None,
            Some("2026-01-01 00:00:00"),
        )
        .unwrap();
        log_point(
            &conn,
            b,
            MonitorEventKind::CertExpiring,
            None,
            Some("2026-01-02 00:00:00"),
        )
        .unwrap();
        let names: Vec<_> = list_recent(&conn, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.monitor_name)
            .collect();
        assert_eq!(names, ["beta", "alpha"]);
    }

    #[test]
    fn prune_keeps_open_events() {
        let conn = test_conn();
        let id = monitor(&conn, "a");
        open(&conn, id, MonitorEventKind::Down, None, Some("2020-01-01 00:00:00")).unwrap();
        log_point(&conn, id, MonitorEventKind::Resumed, None, Some("2020-01-01 00:00:00")).unwrap();
        log_point(&conn, id, MonitorEventKind::Resumed, None, Some("2026-01-01 00:00:00")).unwrap();
        assert_eq!(prune_closed_before(&conn, "2025-01-01 00:00:00").unwrap(), 1);
        assert_eq!(list_for_monitor(&conn, id, 10).unwrap().len(), 2);
    }
}
