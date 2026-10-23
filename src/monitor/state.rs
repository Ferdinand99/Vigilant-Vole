//! Per-monitor runtime state for the alerting state machine, plus the DB
//! queries that seed it at startup (so a restart doesn't send a spurious
//! "Up"), gather agent confirmation votes, and read group children.
//! Owned by the engine agent.

use std::collections::VecDeque;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use crate::db::events::{self, MonitorEventKind};
use crate::db::models::{HeartbeatStatus, Monitor};

/// How many recent checks flap detection looks at.
pub const FLAP_WINDOW: usize = 10;

/// In-memory state of one monitor between checks. Pure data: [`super::alerting::evaluate`]
/// takes the previous value and returns the next one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MonitorState {
    /// Status of the most recent recorded heartbeat.
    pub last_status: Option<HeartbeatStatus>,
    /// When `last_status` started (first heartbeat of the current run of that status).
    pub status_since: Option<OffsetDateTime>,
    /// Real result recorded with the most recent heartbeat (differs from
    /// `last_status` only during maintenance; `None` = unknown).
    pub last_raw_status: Option<HeartbeatStatus>,
    pub last_recorded_at: Option<OffsetDateTime>,
    /// Actual settled state: `Up`, `Down` (an outage event is open) or
    /// `Maintenance`. `None` = unknown. Drives `monitor_events`.
    pub settled: Option<HeartbeatStatus>,
    /// The last state communicated to notification channels (`Up` / `Down`).
    /// Differs from `settled` while flapping mutes transitions.
    pub notified: Option<HeartbeatStatus>,
    /// Consecutive failed checks (after upside-down), reset by an `Up`.
    pub consecutive_failures: i64,
    /// First failed check of the current failing streak.
    pub failing_since: Option<OffsetDateTime>,
    /// Start of the current settled outage (= `failing_since` when it became down).
    pub down_since: Option<OffsetDateTime>,
    /// Start of the outage users were told about (for the Up message's downtime).
    pub notified_down_since: Option<OffsetDateTime>,
    /// Down checks since the last Down/Reminder notification.
    pub checks_since_notice: i64,
    /// Escalation channels were already notified for the current outage.
    pub escalated: bool,
    /// Oldest → newest "was up" flags of the last [`FLAP_WINDOW`] checks.
    pub history: VecDeque<bool>,
    pub flapping: bool,
    /// Consecutive checks with the same up/not-up result (flap recovery).
    pub stable_checks: i64,
    /// Push monitors: last time a push arrived (or the watchdog started).
    pub last_push_at: Option<OffsetDateTime>,
    /// Push monitors: last time the watchdog recorded a missed push.
    pub last_miss_at: Option<OffsetDateTime>,
}

impl MonitorState {
    /// Number of up/not-up changes between adjacent entries of `history`.
    pub fn transitions(&self) -> usize {
        self.history
            .iter()
            .zip(self.history.iter().skip(1))
            .filter(|(a, b)| a != b)
            .count()
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            status: self.last_status,
            since: self.status_since.map(fmt_db_time),
            last_check_at: self.last_recorded_at.map(fmt_db_time),
            consecutive_failures: self.consecutive_failures,
            flapping: self.flapping,
            in_maintenance: self.settled == Some(HeartbeatStatus::Maintenance),
            down_since: self.down_since.map(fmt_db_time),
        }
    }
}

/// Read-only view of a monitor's in-memory state (`Scheduler::status_snapshot`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusSnapshot {
    pub status: Option<HeartbeatStatus>,
    /// When the current status began (UTC `YYYY-MM-DD HH:MM:SS`).
    pub since: Option<String>,
    pub last_check_at: Option<String>,
    pub consecutive_failures: i64,
    pub flapping: bool,
    pub in_maintenance: bool,
    /// Start of the ongoing outage, if down.
    pub down_since: Option<String>,
}

/// Fresh remote-agent results for one monitor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentVotes {
    pub up: u32,
    pub down: u32,
}

impl AgentVotes {
    pub fn total(&self) -> u32 {
        self.up + self.down
    }
}

// ---------------------------------------------------------------------------
// Time helpers (DB format: UTC `YYYY-MM-DD HH:MM:SS`)
// ---------------------------------------------------------------------------

/// Formats as SQLite's `datetime('now')` format.
pub fn fmt_db_time(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// Parses `YYYY-MM-DD HH:MM:SS` (also accepts a `T` separator and a bare
/// `YYYY-MM-DD`, read as midnight) as UTC.
pub fn parse_db_time(s: &str) -> Option<OffsetDateTime> {
    let s = s.trim();
    let num = |range: std::ops::Range<usize>| s.get(range).and_then(|p| p.parse::<u32>().ok());
    let year = s.get(0..4)?.parse::<i32>().ok()?;
    let month = Month::try_from(u8::try_from(num(5..7)?).ok()?).ok()?;
    let day = u8::try_from(num(8..10)?).ok()?;
    let date = Date::from_calendar_date(year, month, day).ok()?;
    let time = if s.len() >= 19 {
        Time::from_hms(
            u8::try_from(num(11..13)?).ok()?,
            u8::try_from(num(14..16)?).ok()?,
            u8::try_from(num(17..19)?).ok()?,
        )
        .ok()?
    } else {
        Time::MIDNIGHT
    };
    Some(PrimitiveDateTime::new(date, time).assume_utc())
}

// ---------------------------------------------------------------------------
// DB queries
// ---------------------------------------------------------------------------

/// Rebuilds a monitor's state from the database: recent local heartbeats and
/// open events. Used the first time a monitor is processed in this process
/// (startup, resume) so the state machine continues where it left off:
/// a monitor that was down stays "notified down" (no repeated Down, and an Up
/// is only sent when it really recovers); one that was up doesn't announce
/// "Up" after a restart.
pub fn seed(
    conn: &Connection,
    monitor: &Monitor,
    now: OffsetDateTime,
) -> rusqlite::Result<MonitorState> {
    let id = monitor.id;
    // Newest first.
    let recent: Vec<(HeartbeatStatus, OffsetDateTime)> = {
        let mut stmt = conn.prepare(
            "SELECT status, checked_at FROM heartbeats
             WHERE monitor_id = ?1 AND agent_id IS NULL
             ORDER BY checked_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![id, FLAP_WINDOW as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (status, at) = row?;
            if let (Some(status), Some(at)) = (HeartbeatStatus::parse(&status), parse_db_time(&at))
            {
                out.push((status, at));
            }
        }
        out
    };
    let last_up_down: Option<(HeartbeatStatus, OffsetDateTime)> = conn
        .query_row(
            "SELECT status, checked_at FROM heartbeats
             WHERE monitor_id = ?1 AND agent_id IS NULL AND status IN ('up', 'down')
             ORDER BY checked_at DESC, id DESC LIMIT 1",
            params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
        .and_then(|(s, at)| Some((HeartbeatStatus::parse(&s)?, parse_db_time(&at)?)));
    let open_down = events::find_open(conn, id, MonitorEventKind::Down)?;
    let open_maintenance = events::find_open(conn, id, MonitorEventKind::Maintenance)?;
    let open_flapping = events::find_open(conn, id, MonitorEventKind::Flapping)?;

    let mut s = MonitorState::default();
    if let Some(&(latest, at)) = recent.first() {
        s.last_status = Some(latest);
        s.last_recorded_at = Some(at);
        s.status_since = recent
            .iter()
            .take_while(|(status, _)| *status == latest)
            .last()
            .map(|(_, t)| *t);
    }

    s.notified = last_up_down.map(|(status, _)| status);
    s.settled = if open_maintenance.is_some() {
        Some(HeartbeatStatus::Maintenance)
    } else if open_down.is_some() {
        Some(HeartbeatStatus::Down)
    } else if s.notified == Some(HeartbeatStatus::Up) {
        Some(HeartbeatStatus::Up)
    } else {
        // Down without an open outage event (paused meanwhile, or a database
        // from before event tracking): the next Down re-opens one.
        None
    };

    // Trailing failing run (newest first until the first up/maintenance).
    let failing: Vec<OffsetDateTime> = recent
        .iter()
        .take_while(|(status, _)| {
            matches!(status, HeartbeatStatus::Down | HeartbeatStatus::Pending)
        })
        .map(|(_, t)| *t)
        .collect();
    s.consecutive_failures = failing.len() as i64;
    s.failing_since = failing.last().copied();
    s.down_since = open_down
        .as_ref()
        .and_then(|e| parse_db_time(&e.started_at));
    if s.settled == Some(HeartbeatStatus::Down) || s.notified == Some(HeartbeatStatus::Down) {
        // Already past the retry budget: the next failure is Down, not Pending.
        s.consecutive_failures = s.consecutive_failures.max(monitor.retries.max(0) + 1);
        s.failing_since = s.failing_since.or(s.down_since);
    }
    if s.notified == Some(HeartbeatStatus::Down) {
        s.notified_down_since = s
            .down_since
            .or(s.failing_since)
            .or(last_up_down.map(|(_, t)| t));
    }
    // Don't re-send an escalation that (most likely) already went out before the restart.
    s.escalated = monitor.escalation_minutes > 0
        && s.down_since.is_some_and(|since| {
            now - since >= time::Duration::minutes(monitor.escalation_minutes)
        });

    s.history = recent
        .iter()
        .rev()
        .filter(|(status, _)| *status != HeartbeatStatus::Maintenance)
        .map(|(status, _)| *status == HeartbeatStatus::Up)
        .collect();
    s.flapping = open_flapping.is_some();
    s.stable_checks = match s.history.back() {
        Some(last) => s.history.iter().rev().take_while(|v| *v == last).count() as i64,
        None => 0,
    };
    s.last_push_at = s.last_recorded_at;
    Ok(s)
}

/// Latest result per active remote agent for `monitor_id` that is no older
/// than `max_age_seconds` before `now`. Pending/maintenance results don't vote.
pub fn agent_votes(
    conn: &Connection,
    monitor_id: i64,
    max_age_seconds: i64,
    now: OffsetDateTime,
) -> rusqlite::Result<AgentVotes> {
    let since = fmt_db_time(now - time::Duration::seconds(max_age_seconds.max(1)));
    let mut stmt = conn.prepare(
        "SELECT h.status FROM heartbeats h
         JOIN (SELECT agent_id, MAX(id) AS id FROM heartbeats
               WHERE monitor_id = ?1 AND agent_id IS NOT NULL AND checked_at >= ?2
               GROUP BY agent_id) latest ON latest.id = h.id
         JOIN agents a ON a.id = h.agent_id AND a.active = 1",
    )?;
    let rows = stmt.query_map(params![monitor_id, since], |r| r.get::<_, String>(0))?;
    let mut votes = AgentVotes::default();
    for row in rows {
        match HeartbeatStatus::parse(&row?) {
            Some(HeartbeatStatus::Up) => votes.up += 1,
            Some(HeartbeatStatus::Down) => votes.down += 1,
            _ => {}
        }
    }
    Ok(votes)
}

/// A group child: name, latest recorded status, latest real status.
pub type GroupChild = (String, Option<HeartbeatStatus>, Option<HeartbeatStatus>);

/// Active children of a group with their latest local status and real
/// status (`None` = no heartbeat yet / real result unknown), ordered by name.
pub fn group_children(conn: &Connection, group_id: i64) -> rusqlite::Result<Vec<GroupChild>> {
    let mut stmt = conn.prepare(
        "SELECT m.name, h.status, COALESCE(h.raw_status, NULLIF(h.status, 'maintenance'))
         FROM monitors m
         LEFT JOIN heartbeats h ON h.id = (
             SELECT id FROM heartbeats
             WHERE monitor_id = m.id AND agent_id IS NULL
             ORDER BY checked_at DESC, id DESC LIMIT 1
         )
         WHERE m.parent_id = ?1 AND m.active = 1 AND m.id <> ?1
         ORDER BY m.sort_order, m.name",
    )?;
    let rows = stmt.query_map(params![group_id], |r| {
        let parse = |i: usize| -> rusqlite::Result<Option<HeartbeatStatus>> {
            Ok(r.get::<_, Option<String>>(i)?
                .and_then(|s| HeartbeatStatus::parse(&s)))
        };
        Ok((r.get::<_, String>(0)?, parse(1)?, parse(2)?))
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewMonitor;
    use crate::db::{monitors, test_conn};

    fn t(s: &str) -> OffsetDateTime {
        parse_db_time(s).unwrap()
    }

    fn new_monitor(conn: &Connection, new: NewMonitor) -> Monitor {
        let id = monitors::insert_monitor(conn, &new).unwrap();
        monitors::get_monitor(conn, id).unwrap().unwrap()
    }

    fn beat(conn: &Connection, id: i64, status: &str, at: &str) {
        conn.execute(
            "INSERT INTO heartbeats (monitor_id, status, checked_at) VALUES (?1, ?2, ?3)",
            params![id, status, at],
        )
        .unwrap();
    }

    #[test]
    fn db_time_round_trips() {
        let s = "2026-03-10 07:08:09";
        assert_eq!(fmt_db_time(t(s)), s);
        assert_eq!(fmt_db_time(t("2026-03-10T07:08:09")), s);
        assert_eq!(fmt_db_time(t("2026-03-10")), "2026-03-10 00:00:00");
        assert!(parse_db_time("garbage").is_none());
        assert!(parse_db_time("2026-13-01 00:00:00").is_none());
    }

    #[test]
    fn seed_of_an_up_monitor_is_settled_and_notified_up() {
        let conn = test_conn();
        let m = new_monitor(
            &conn,
            NewMonitor {
                name: "a".into(),
                ..NewMonitor::default()
            },
        );
        beat(&conn, m.id, "up", "2026-03-10 10:00:00");
        beat(&conn, m.id, "up", "2026-03-10 10:01:00");
        let s = seed(&conn, &m, t("2026-03-10 10:02:00")).unwrap();
        assert_eq!(s.notified, Some(HeartbeatStatus::Up));
        assert_eq!(s.settled, Some(HeartbeatStatus::Up));
        assert_eq!(s.status_since, Some(t("2026-03-10 10:00:00")));
        assert_eq!(s.consecutive_failures, 0);
    }

    #[test]
    fn seed_of_a_down_monitor_keeps_outage_and_retry_budget_spent() {
        let conn = test_conn();
        let m = new_monitor(
            &conn,
            NewMonitor {
                name: "a".into(),
                retries: 3,
                ..NewMonitor::default()
            },
        );
        beat(&conn, m.id, "up", "2026-03-10 10:00:00");
        beat(&conn, m.id, "down", "2026-03-10 10:01:00");
        events::open(
            &conn,
            m.id,
            MonitorEventKind::Down,
            None,
            Some("2026-03-10 10:01:00"),
        )
        .unwrap();
        let s = seed(&conn, &m, t("2026-03-10 10:02:00")).unwrap();
        assert_eq!(s.notified, Some(HeartbeatStatus::Down));
        assert_eq!(s.settled, Some(HeartbeatStatus::Down));
        assert_eq!(s.down_since, Some(t("2026-03-10 10:01:00")));
        assert_eq!(s.notified_down_since, Some(t("2026-03-10 10:01:00")));
        assert_eq!(s.consecutive_failures, 4);
    }

    #[test]
    fn seed_of_down_without_open_event_leaves_settled_unknown() {
        let conn = test_conn();
        let m = new_monitor(
            &conn,
            NewMonitor {
                name: "a".into(),
                ..NewMonitor::default()
            },
        );
        beat(&conn, m.id, "down", "2026-03-10 10:01:00");
        let s = seed(&conn, &m, t("2026-03-10 10:02:00")).unwrap();
        assert_eq!(s.notified, Some(HeartbeatStatus::Down));
        assert_eq!(s.settled, None);
    }

    #[test]
    fn seed_marks_long_outage_as_already_escalated() {
        let conn = test_conn();
        let m = new_monitor(
            &conn,
            NewMonitor {
                name: "a".into(),
                escalation_minutes: 10,
                ..NewMonitor::default()
            },
        );
        beat(&conn, m.id, "down", "2026-03-10 10:00:00");
        events::open(
            &conn,
            m.id,
            MonitorEventKind::Down,
            None,
            Some("2026-03-10 10:00:00"),
        )
        .unwrap();
        assert!(seed(&conn, &m, t("2026-03-10 10:30:00")).unwrap().escalated);
        assert!(!seed(&conn, &m, t("2026-03-10 10:05:00")).unwrap().escalated);
    }

    #[test]
    fn seed_restores_maintenance_and_flapping() {
        let conn = test_conn();
        let m = new_monitor(
            &conn,
            NewMonitor {
                name: "a".into(),
                ..NewMonitor::default()
            },
        );
        beat(&conn, m.id, "maintenance", "2026-03-10 10:00:00");
        events::open(&conn, m.id, MonitorEventKind::Maintenance, None, None).unwrap();
        events::open(&conn, m.id, MonitorEventKind::Flapping, None, None).unwrap();
        let s = seed(&conn, &m, t("2026-03-10 10:02:00")).unwrap();
        assert_eq!(s.settled, Some(HeartbeatStatus::Maintenance));
        assert!(s.flapping);
    }

    #[test]
    fn seed_of_new_monitor_is_empty() {
        let conn = test_conn();
        let m = new_monitor(
            &conn,
            NewMonitor {
                name: "a".into(),
                ..NewMonitor::default()
            },
        );
        assert_eq!(
            seed(&conn, &m, t("2026-03-10 10:02:00")).unwrap(),
            MonitorState::default()
        );
    }

    #[test]
    fn agent_votes_count_latest_fresh_result_per_active_agent() {
        let conn = test_conn();
        let m = new_monitor(
            &conn,
            NewMonitor {
                name: "a".into(),
                ..NewMonitor::default()
            },
        );
        conn.execute_batch(
            "INSERT INTO agents (id, name, prefix, token_hash, active) VALUES
                 (1, 'a', 'p', 'h1', 1), (2, 'b', 'p', 'h2', 1), (3, 'c', 'p', 'h3', 0), (4, 'd', 'p', 'h4', 1);",
        )
        .unwrap();
        let ins = |agent: i64, status: &str, at: &str| {
            conn.execute(
                "INSERT INTO heartbeats (monitor_id, agent_id, status, checked_at) VALUES (?1, ?2, ?3, ?4)",
                params![m.id, agent, status, at],
            )
            .unwrap();
        };
        ins(1, "down", "2026-03-10 10:00:00");
        ins(1, "up", "2026-03-10 10:01:00"); // latest for agent 1 → up
        ins(2, "down", "2026-03-10 10:01:00");
        ins(3, "up", "2026-03-10 10:01:00"); // inactive agent
        ins(4, "up", "2026-03-10 09:00:00"); // stale
        let votes = agent_votes(&conn, m.id, 120, t("2026-03-10 10:02:00")).unwrap();
        assert_eq!(votes, AgentVotes { up: 1, down: 1 });
    }

    #[test]
    fn group_children_lists_active_children_with_latest_status() {
        let conn = test_conn();
        let g = new_monitor(
            &conn,
            NewMonitor {
                name: "g".into(),
                ..NewMonitor::default()
            },
        );
        let a = new_monitor(
            &conn,
            NewMonitor {
                name: "a".into(),
                parent_id: Some(g.id),
                ..NewMonitor::default()
            },
        );
        new_monitor(
            &conn,
            NewMonitor {
                name: "b".into(),
                parent_id: Some(g.id),
                ..NewMonitor::default()
            },
        );
        new_monitor(
            &conn,
            NewMonitor {
                name: "c".into(),
                parent_id: Some(g.id),
                active: false,
                ..NewMonitor::default()
            },
        );
        beat(&conn, a.id, "up", "2026-03-10 10:00:00");
        beat(&conn, a.id, "down", "2026-03-10 10:01:00");
        let children = group_children(&conn, g.id).unwrap();
        use HeartbeatStatus::{Down, Maintenance};
        assert_eq!(
            children,
            vec![
                ("a".to_string(), Some(Down), Some(Down)),
                ("b".to_string(), None, None)
            ]
        );
        // In maintenance: the recorded and the real status are both listed.
        crate::db::heartbeats::insert_with_raw(&conn, a.id, Maintenance, Some(Down), None, None)
            .unwrap();
        assert_eq!(
            group_children(&conn, g.id).unwrap()[0],
            ("a".to_string(), Some(Maintenance), Some(Down))
        );
    }

    #[test]
    fn transitions_counts_changes() {
        let s = MonitorState {
            history: [true, false, false, true, false].into_iter().collect(),
            ..MonitorState::default()
        };
        assert_eq!(s.transitions(), 3);
    }
}
