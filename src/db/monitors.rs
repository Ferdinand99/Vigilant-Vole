use rusqlite::{Connection, OptionalExtension, Row, params};

use super::models::{HeartbeatStatus, Monitor, MonitorType, MonitorWithStatus, NewMonitor};

pub fn insert_monitor(conn: &Connection, m: &NewMonitor) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO monitors (name, monitor_type, target, interval_seconds, timeout_seconds, retries)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            m.name,
            m.monitor_type.as_str(),
            m.target,
            m.interval_seconds,
            m.timeout_seconds,
            m.retries
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_monitor(conn: &Connection, id: i64, m: &NewMonitor) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE monitors
         SET name = ?1, monitor_type = ?2, target = ?3, interval_seconds = ?4, timeout_seconds = ?5, retries = ?6
         WHERE id = ?7",
        params![
            m.name,
            m.monitor_type.as_str(),
            m.target,
            m.interval_seconds,
            m.timeout_seconds,
            m.retries,
            id
        ],
    )?;
    Ok(())
}

pub fn delete_monitor(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM monitors WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn get_monitor(conn: &Connection, id: i64) -> rusqlite::Result<Option<Monitor>> {
    conn.query_row(
        "SELECT id, name, monitor_type, target, interval_seconds, timeout_seconds, retries, active
         FROM monitors WHERE id = ?1",
        params![id],
        row_to_monitor,
    )
    .optional()
}

pub fn list_active_monitors(conn: &Connection) -> rusqlite::Result<Vec<Monitor>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, monitor_type, target, interval_seconds, timeout_seconds, retries, active
         FROM monitors WHERE active = 1 ORDER BY name",
    )?;
    let rows = stmt.query_map([], row_to_monitor)?;
    rows.collect()
}

pub fn list_monitors_with_status(conn: &Connection) -> rusqlite::Result<Vec<MonitorWithStatus>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.name, m.monitor_type, m.target, m.interval_seconds, m.timeout_seconds, m.retries, m.active,
                h.status, h.response_time_ms
         FROM monitors m
         LEFT JOIN heartbeats h ON h.id = (
             SELECT id FROM heartbeats WHERE monitor_id = m.id ORDER BY checked_at DESC, id DESC LIMIT 1
         )
         ORDER BY m.name",
    )?;
    let rows = stmt.query_map([], |row| {
        let monitor = row_to_monitor(row)?;
        let status: Option<String> = row.get(8)?;
        let response_time_ms: Option<i64> = row.get(9)?;
        Ok(MonitorWithStatus {
            monitor,
            status: status.and_then(|s| HeartbeatStatus::parse(&s)),
            response_time_ms,
        })
    })?;
    rows.collect()
}

pub fn insert_heartbeat(
    conn: &Connection,
    monitor_id: i64,
    status: HeartbeatStatus,
    response_time_ms: Option<i64>,
    message: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO heartbeats (monitor_id, status, response_time_ms, message) VALUES (?1, ?2, ?3, ?4)",
        params![monitor_id, status.as_str(), response_time_ms, message],
    )?;
    Ok(())
}

fn row_to_monitor(row: &Row) -> rusqlite::Result<Monitor> {
    let monitor_type: String = row.get(2)?;
    let active: i64 = row.get(7)?;
    Ok(Monitor {
        id: row.get(0)?,
        name: row.get(1)?,
        monitor_type: MonitorType::parse(&monitor_type).unwrap_or(MonitorType::Http),
        target: row.get(3)?,
        interval_seconds: row.get(4)?,
        timeout_seconds: row.get(5)?,
        retries: row.get(6)?,
        active: active != 0,
    })
}
