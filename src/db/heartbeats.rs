//! Heartbeat storage. Owned by the monitors agent.
//!
//! Heartbeats with `agent_id IS NOT NULL` are remote-agent results; every
//! query here that describes a monitor's *own* history filters them out.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::models::{Heartbeat, HeartbeatStatus};

/// Maps [`COLUMNS`]. A row without `raw_status` (recorded before it existed)
/// gets its `status`, unless that was `maintenance`: then the real result is
/// unknown.
pub fn row_to_heartbeat(row: &Row) -> rusqlite::Result<Heartbeat> {
    let status: String = row.get("status")?;
    let status = HeartbeatStatus::parse(&status).unwrap_or(HeartbeatStatus::Pending);
    let raw: Option<String> = row.get("raw_status")?;
    Ok(Heartbeat {
        id: row.get("id")?,
        monitor_id: row.get("monitor_id")?,
        agent_id: row.get("agent_id")?,
        status,
        raw_status: match raw {
            Some(r) => HeartbeatStatus::parse(&r),
            None => real(status),
        },
        response_time_ms: row.get("response_time_ms")?,
        message: row.get("message")?,
        checked_at: row.get("checked_at")?,
    })
}

/// `status` as a real result: `None` for `maintenance`.
fn real(status: HeartbeatStatus) -> Option<HeartbeatStatus> {
    (status != HeartbeatStatus::Maintenance).then_some(status)
}

/// Records one heartbeat (`agent_id = None` for local checks) whose real
/// result is its status. Returns its id.
pub fn insert(
    conn: &Connection,
    monitor_id: i64,
    agent_id: Option<i64>,
    status: HeartbeatStatus,
    response_time_ms: Option<i64>,
    message: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO heartbeats (monitor_id, agent_id, status, raw_status, response_time_ms, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            monitor_id,
            agent_id,
            status.as_str(),
            real(status).map(HeartbeatStatus::as_str),
            response_time_ms,
            message
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Records one local heartbeat with its real result (`raw_status`), which
/// differs from `status` during maintenance. Returns its id.
pub fn insert_with_raw(
    conn: &Connection,
    monitor_id: i64,
    status: HeartbeatStatus,
    raw_status: Option<HeartbeatStatus>,
    response_time_ms: Option<i64>,
    message: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO heartbeats (monitor_id, status, raw_status, response_time_ms, message)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            monitor_id,
            status.as_str(),
            raw_status.and_then(real).map(HeartbeatStatus::as_str),
            response_time_ms,
            message
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The columns [`row_to_heartbeat`] reads.
pub const COLUMNS: &str =
    "id, monitor_id, agent_id, status, raw_status, response_time_ms, message, checked_at";

/// Checks [`raw_run`] looks back over.
const RAW_RUN_CHECKS: i64 = 100;

/// A monitor's current real result: the newest local heartbeat's
/// `raw_status` and `message`, and when that real status began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRun {
    pub status: Option<HeartbeatStatus>,
    /// First heartbeat of the current run of `status` within the last
    /// [`RAW_RUN_CHECKS`] checks (UTC `YYYY-MM-DD HH:MM:SS`); `None` when
    /// `status` is unknown.
    pub since: Option<String>,
    pub message: Option<String>,
}

/// The current real result of a monitor (see [`RawRun`]); `None` without
/// local heartbeats.
pub fn raw_run(conn: &Connection, monitor_id: i64) -> rusqlite::Result<Option<RawRun>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM heartbeats WHERE monitor_id = ?1 AND agent_id IS NULL
         ORDER BY checked_at DESC, id DESC LIMIT ?2"
    ))?;
    let beats = stmt
        .query_map(params![monitor_id, RAW_RUN_CHECKS], row_to_heartbeat)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(latest) = beats.first() else {
        return Ok(None);
    };
    let since = latest.raw_status.and_then(|status| {
        beats
            .iter()
            .take_while(|h| h.raw_status == Some(status))
            .last()
            .map(|h| h.checked_at.clone())
    });
    Ok(Some(RawRun {
        status: latest.raw_status,
        since,
        message: latest.message.clone(),
    }))
}

/// The newest local heartbeat of a monitor.
pub fn latest_local(conn: &Connection, monitor_id: i64) -> rusqlite::Result<Option<Heartbeat>> {
    conn.query_row(
        &format!(
            "SELECT {COLUMNS} FROM heartbeats WHERE monitor_id = ?1 AND agent_id IS NULL
             ORDER BY checked_at DESC, id DESC LIMIT 1"
        ),
        params![monitor_id],
        row_to_heartbeat,
    )
    .optional()
}

/// One page of a monitor's local heartbeats, newest first (by `checked_at`,
/// then id). Pass the id of the last row of the previous page as
/// `before_id` to get the next (older) page.
pub fn list_page(
    conn: &Connection,
    monitor_id: i64,
    before_id: Option<i64>,
    limit: i64,
) -> rusqlite::Result<Vec<Heartbeat>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM heartbeats
         WHERE monitor_id = ?1 AND agent_id IS NULL
           AND (?2 IS NULL OR (checked_at, id) < (SELECT checked_at, id FROM heartbeats WHERE id = ?2))
         ORDER BY checked_at DESC, id DESC LIMIT ?3"
    ))?;
    let rows = stmt.query_map(
        params![monitor_id, before_id, limit.clamp(1, 500)],
        row_to_heartbeat,
    )?;
    rows.collect()
}

/// One time bucket of the response-time chart.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartBucket {
    /// 0-based bucket index from the window start.
    pub index: i64,
    /// Average response time of non-down checks in the bucket.
    pub avg_ms: Option<f64>,
    pub down: i64,
    pub maintenance: i64,
    pub total: i64,
}

/// Local heartbeats from `start_unix` on, grouped into `bucket_secs` buckets
/// (only buckets with data are returned, ascending).
pub fn chart_buckets(
    conn: &Connection,
    monitor_id: i64,
    start_unix: i64,
    bucket_secs: i64,
) -> rusqlite::Result<Vec<ChartBucket>> {
    let mut stmt = conn.prepare(
        "SELECT (unixepoch(checked_at) - ?2) / ?3 AS b,
                AVG(CASE WHEN status <> 'down' THEN response_time_ms END),
                SUM(status = 'down'), SUM(status = 'maintenance'), COUNT(*)
         FROM heartbeats
         WHERE monitor_id = ?1 AND agent_id IS NULL AND checked_at >= datetime(?2, 'unixepoch')
         GROUP BY b ORDER BY b",
    )?;
    let rows = stmt.query_map(params![monitor_id, start_unix, bucket_secs.max(1)], |r| {
        Ok(ChartBucket {
            index: r.get(0)?,
            avg_ms: r.get(1)?,
            down: r.get(2)?,
            maintenance: r.get(3)?,
            total: r.get(4)?,
        })
    })?;
    rows.collect()
}

/// The latest result each remote agent reported for a monitor.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentResult {
    pub agent_id: i64,
    pub agent_name: String,
    pub location: String,
    pub status: HeartbeatStatus,
    pub response_time_ms: Option<i64>,
    pub message: Option<String>,
    pub checked_at: String,
}

/// Latest heartbeat per remote agent for a monitor, by agent name.
pub fn agent_results(conn: &Connection, monitor_id: i64) -> rusqlite::Result<Vec<AgentResult>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.name, a.location, h.status, h.response_time_ms, h.message, h.checked_at
         FROM agents a
         JOIN heartbeats h ON h.id = (
             SELECT id FROM heartbeats WHERE agent_id = a.id AND monitor_id = ?1
             ORDER BY checked_at DESC, id DESC LIMIT 1
         )
         ORDER BY a.name COLLATE NOCASE, a.id",
    )?;
    let rows = stmt.query_map(params![monitor_id], |r| {
        let status: String = r.get(3)?;
        Ok(AgentResult {
            agent_id: r.get(0)?,
            agent_name: r.get(1)?,
            location: r.get(2)?,
            status: HeartbeatStatus::parse(&status).unwrap_or(HeartbeatStatus::Pending),
            response_time_ms: r.get(4)?,
            message: r.get(5)?,
            checked_at: r.get(6)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewMonitor;
    use crate::db::{monitors, test_conn};

    fn setup() -> (Connection, i64) {
        let conn = test_conn();
        let id = monitors::insert_monitor(&conn, &NewMonitor::default()).unwrap();
        (conn, id)
    }

    fn insert_at(
        conn: &Connection,
        id: i64,
        agent: Option<i64>,
        status: &str,
        ms: Option<i64>,
        at: &str,
    ) {
        conn.execute(
            "INSERT INTO heartbeats (monitor_id, agent_id, status, response_time_ms, checked_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, agent, status, ms, at],
        )
        .unwrap();
    }

    fn agent(conn: &Connection, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO agents (name, location, prefix, token_hash) VALUES (?1, 'eu', 'x', ?1)",
            params![name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn pagination_walks_backwards_without_overlap_and_skips_agents() {
        let (conn, id) = setup();
        let a = agent(&conn, "remote");
        for i in 0..25 {
            insert(&conn, id, None, HeartbeatStatus::Up, Some(i), None).unwrap();
            insert(&conn, id, Some(a), HeartbeatStatus::Down, None, None).unwrap();
        }
        let first = list_page(&conn, id, None, 10).unwrap();
        assert_eq!(first.len(), 10);
        assert!(first.iter().all(|h| h.agent_id.is_none()));
        assert_eq!(first[0].response_time_ms, Some(24));
        let second = list_page(&conn, id, first.last().map(|h| h.id), 10).unwrap();
        assert_eq!(second[0].response_time_ms, Some(14));
        let third = list_page(&conn, id, second.last().map(|h| h.id), 10).unwrap();
        assert_eq!(third.len(), 5);
        assert!(
            list_page(&conn, id, third.last().map(|h| h.id), 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            latest_local(&conn, id).unwrap().unwrap().response_time_ms,
            Some(24)
        );
    }

    #[test]
    fn raw_run_follows_the_real_result_through_maintenance() {
        use HeartbeatStatus::{Down, Maintenance, Up};
        let (conn, id) = setup();
        assert_eq!(raw_run(&conn, id).unwrap(), None);
        let at = |minute: i64, status: HeartbeatStatus, raw: Option<HeartbeatStatus>| {
            let hb = insert_with_raw(&conn, id, status, raw, None, Some("m")).unwrap();
            conn.execute(
                "UPDATE heartbeats SET checked_at = ?2 WHERE id = ?1",
                params![hb, format!("2026-01-01 00:{minute:02}:00")],
            )
            .unwrap();
        };
        at(0, Up, Some(Up));
        at(1, Maintenance, Some(Down));
        at(2, Maintenance, Some(Up));
        at(3, Maintenance, Some(Up));
        let run = raw_run(&conn, id).unwrap().unwrap();
        assert_eq!(
            (run.status, run.since.as_deref()),
            (Some(Up), Some("2026-01-01 00:02:00"))
        );
        let page = list_page(&conn, id, None, 10).unwrap();
        assert_eq!(
            (page[0].status, page[0].raw_status),
            (Maintenance, Some(Up))
        );
        assert_eq!(
            (page[2].status, page[2].raw_status),
            (Maintenance, Some(Down))
        );
        // Rows from before raw_status: equal to status, unknown in maintenance.
        insert_at(&conn, id, None, "maintenance", None, "2026-01-01 00:04:00");
        let run = raw_run(&conn, id).unwrap().unwrap();
        assert_eq!((run.status, run.since), (None, None));
        insert_at(&conn, id, None, "up", None, "2026-01-01 00:05:00");
        assert_eq!(
            latest_local(&conn, id).unwrap().unwrap().raw_status,
            Some(Up)
        );
    }

    #[test]
    fn pagination_follows_check_time_not_insert_order() {
        let (conn, id) = setup();
        // Inserted newest first (e.g. an import): pages must still be by time.
        for minute in (0..30).rev() {
            insert_at(
                &conn,
                id,
                None,
                "up",
                Some(minute),
                &format!("2026-01-01 00:{minute:02}:00"),
            );
        }
        let first = list_page(&conn, id, None, 10).unwrap();
        assert_eq!(first[0].response_time_ms, Some(29));
        let second = list_page(&conn, id, first.last().map(|h| h.id), 10).unwrap();
        assert_eq!(second[0].response_time_ms, Some(19));
        assert_eq!(second[9].response_time_ms, Some(10));
    }

    #[test]
    fn chart_buckets_group_and_ignore_agents() {
        let (conn, id) = setup();
        let a = agent(&conn, "remote");
        // 2026-01-01 00:00:00 UTC = 1767225600
        let start = 1_767_225_600;
        insert_at(&conn, id, None, "up", Some(100), "2026-01-01 00:00:10");
        insert_at(&conn, id, None, "up", Some(300), "2026-01-01 00:00:50");
        insert_at(&conn, id, None, "down", Some(9999), "2026-01-01 00:02:10");
        insert_at(&conn, id, None, "maintenance", None, "2026-01-01 00:03:00");
        insert_at(&conn, id, Some(a), "down", None, "2026-01-01 00:00:20");
        insert_at(&conn, id, None, "up", Some(5), "2025-12-31 23:59:59");
        let b = chart_buckets(&conn, id, start, 60).unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(
            (b[0].index, b[0].avg_ms, b[0].total, b[0].down),
            (0, Some(200.0), 2, 0)
        );
        assert_eq!((b[1].index, b[1].avg_ms, b[1].down), (2, None, 1));
        assert_eq!((b[2].index, b[2].maintenance), (3, 1));
    }

    #[test]
    fn agent_results_take_each_agents_latest() {
        let (conn, id) = setup();
        let berlin = agent(&conn, "Berlin");
        let austin = agent(&conn, "austin");
        let _idle = agent(&conn, "idle");
        insert_at(&conn, id, Some(berlin), "down", None, "2026-01-01 00:00:00");
        insert_at(
            &conn,
            id,
            Some(berlin),
            "up",
            Some(40),
            "2026-01-01 00:01:00",
        );
        insert_at(&conn, id, Some(austin), "down", None, "2026-01-01 00:00:30");
        let r = agent_results(&conn, id).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(
            (r[0].agent_name.as_str(), r[0].status),
            ("austin", HeartbeatStatus::Down)
        );
        assert_eq!(
            (r[1].agent_name.as_str(), r[1].response_time_ms),
            ("Berlin", Some(40))
        );
    }
}
