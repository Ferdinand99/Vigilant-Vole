//! Maintenance windows and their links (`maintenance_windows`, `maintenance_monitors`,
//! `maintenance_status_pages`). Owned by the statuspages agent.
//!
//! Times are stored as UTC `YYYY-MM-DD HH:MM:SS`; `time_from` / `time_to`
//! are local `HH:MM` in the window's `timezone`. The schedule math lives in
//! [`crate::maintenance`].

use rusqlite::{Connection, OptionalExtension, Row, params};

/// A row of `maintenance_windows`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MaintenanceWindow {
    pub id: i64,
    pub title: String,
    pub description: String,
    /// "manual" | "once" | "daily" | "weekly".
    pub strategy: String,
    /// Manual: "in maintenance right now". Scheduled: enabled (not paused).
    pub active: bool,
    /// Once: window start. Recurring: first moment occurrences may start.
    pub start_at: Option<String>,
    /// Once: window end. Recurring: occurrences must start before this.
    pub end_at: Option<String>,
    /// Weekly: ISO weekdays, e.g. "1,3,5" (Mon = 1).
    pub weekdays: Option<String>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
    pub timezone: String,
    /// Recurring occurrences that started before this were ended early.
    pub skip_until: Option<String>,
    pub created_at: String,
}

/// Fields written by create/update.
/// Longest window description, in characters (rendered as Markdown on public pages).
pub const DESCRIPTION_MAX: usize = 20_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewWindow {
    pub title: String,
    pub description: String,
    pub strategy: String,
    pub active: bool,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub weekdays: Option<String>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
    pub timezone: String,
    pub monitor_ids: Vec<i64>,
    pub status_page_ids: Vec<i64>,
}

const COLUMNS: &str = "id, title, description, strategy, active, start_at, end_at, weekdays, time_from, time_to, \
                       timezone, skip_until, created_at";

pub fn row_to_window(row: &Row) -> rusqlite::Result<MaintenanceWindow> {
    Ok(MaintenanceWindow {
        id: row.get("id")?,
        title: row.get("title")?,
        description: row.get("description")?,
        strategy: row.get("strategy")?,
        active: row.get("active")?,
        start_at: row.get("start_at")?,
        end_at: row.get("end_at")?,
        weekdays: row.get("weekdays")?,
        time_from: row.get("time_from")?,
        time_to: row.get("time_to")?,
        timezone: row.get("timezone")?,
        skip_until: row.get("skip_until")?,
        created_at: row.get("created_at")?,
    })
}

pub fn list(conn: &Connection) -> rusqlite::Result<Vec<MaintenanceWindow>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM maintenance_windows ORDER BY id DESC"))?;
    let rows = stmt.query_map([], row_to_window)?;
    rows.collect()
}

/// Windows that may be in effect (manual ones switched on, scheduled ones not paused).
pub fn list_enabled(conn: &Connection) -> rusqlite::Result<Vec<MaintenanceWindow>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM maintenance_windows WHERE active = 1"))?;
    let rows = stmt.query_map([], row_to_window)?;
    rows.collect()
}

/// Enabled windows linked to a status page.
pub fn list_for_status_page(conn: &Connection, status_page_id: i64) -> rusqlite::Result<Vec<MaintenanceWindow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM maintenance_windows
         WHERE active = 1 AND id IN (SELECT maintenance_id FROM maintenance_status_pages WHERE status_page_id = ?1)
         ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![status_page_id], row_to_window)?;
    rows.collect()
}

pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<MaintenanceWindow>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM maintenance_windows WHERE id = ?1"),
        params![id],
        row_to_window,
    )
    .optional()
}

pub fn insert(conn: &mut Connection, w: &NewWindow) -> rusqlite::Result<i64> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO maintenance_windows
             (title, description, strategy, active, start_at, end_at, weekdays, time_from, time_to, timezone)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            w.title,
            w.description,
            w.strategy,
            w.active,
            w.start_at,
            w.end_at,
            w.weekdays,
            w.time_from,
            w.time_to,
            w.timezone
        ],
    )?;
    let id = tx.last_insert_rowid();
    write_links(&tx, id, w)?;
    tx.commit()?;
    Ok(id)
}

/// Updates a window and replaces its links. Clears any early-end marker,
/// since the schedule may have changed.
pub fn update(conn: &mut Connection, id: i64, w: &NewWindow) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    let n = tx.execute(
        "UPDATE maintenance_windows SET title = ?2, description = ?3, strategy = ?4, active = ?5, start_at = ?6,
             end_at = ?7, weekdays = ?8, time_from = ?9, time_to = ?10, timezone = ?11, skip_until = NULL
         WHERE id = ?1",
        params![
            id,
            w.title,
            w.description,
            w.strategy,
            w.active,
            w.start_at,
            w.end_at,
            w.weekdays,
            w.time_from,
            w.time_to,
            w.timezone
        ],
    )?;
    if n == 0 {
        return Ok(false);
    }
    tx.execute("DELETE FROM maintenance_monitors WHERE maintenance_id = ?1", params![id])?;
    tx.execute("DELETE FROM maintenance_status_pages WHERE maintenance_id = ?1", params![id])?;
    write_links(&tx, id, w)?;
    tx.commit()?;
    Ok(true)
}

fn write_links(conn: &Connection, id: i64, w: &NewWindow) -> rusqlite::Result<()> {
    let mut add_monitor = conn.prepare(
        "INSERT OR IGNORE INTO maintenance_monitors (maintenance_id, monitor_id)
         SELECT ?1, id FROM monitors WHERE id = ?2",
    )?;
    for mid in &w.monitor_ids {
        add_monitor.execute(params![id, mid])?;
    }
    let mut add_page = conn.prepare(
        "INSERT OR IGNORE INTO maintenance_status_pages (maintenance_id, status_page_id)
         SELECT ?1, id FROM status_pages WHERE id = ?2",
    )?;
    for pid in &w.status_page_ids {
        add_page.execute(params![id, pid])?;
    }
    Ok(())
}

pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM maintenance_windows WHERE id = ?1", params![id])? > 0)
}

pub fn set_active(conn: &Connection, id: i64, active: bool) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE maintenance_windows SET active = ?2, skip_until = NULL WHERE id = ?1",
        params![id, active],
    )? > 0)
}

pub fn set_times(conn: &Connection, id: i64, start_at: Option<&str>, end_at: Option<&str>) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE maintenance_windows SET start_at = ?2, end_at = ?3 WHERE id = ?1",
        params![id, start_at, end_at],
    )? > 0)
}

pub fn set_skip_until(conn: &Connection, id: i64, skip_until: Option<&str>) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE maintenance_windows SET skip_until = ?2 WHERE id = ?1",
        params![id, skip_until],
    )? > 0)
}

pub fn monitor_ids(conn: &Connection, maintenance_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt =
        conn.prepare_cached("SELECT monitor_id FROM maintenance_monitors WHERE maintenance_id = ?1 ORDER BY monitor_id")?;
    let rows = stmt.query_map(params![maintenance_id], |r| r.get(0))?;
    rows.collect()
}

pub fn status_page_ids(conn: &Connection, maintenance_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT status_page_id FROM maintenance_status_pages WHERE maintenance_id = ?1 ORDER BY status_page_id",
    )?;
    let rows = stmt.query_map(params![maintenance_id], |r| r.get(0))?;
    rows.collect()
}

/// `(maintenance_id, monitor_id)` for every link, for list pages.
pub fn all_monitor_links(conn: &Connection) -> rusqlite::Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare("SELECT maintenance_id, monitor_id FROM maintenance_monitors")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// `(child_id, parent_id)` for every monitor inside a group.
pub fn group_edges(conn: &Connection) -> rusqlite::Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare_cached("SELECT id, parent_id FROM monitors WHERE parent_id IS NOT NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewMonitor;
    use crate::db::{monitors, test_conn};

    fn window(title: &str, monitor_ids: Vec<i64>) -> NewWindow {
        NewWindow {
            title: title.into(),
            strategy: "manual".into(),
            active: true,
            timezone: "UTC".into(),
            monitor_ids,
            ..NewWindow::default()
        }
    }

    fn monitor(conn: &Connection, name: &str) -> i64 {
        monitors::insert_monitor(
            conn,
            &NewMonitor {
                name: name.into(),
                ..NewMonitor::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn crud_round_trip_with_links() {
        let mut conn = test_conn();
        let a = monitor(&conn, "a");
        let b = monitor(&conn, "b");
        let id = insert(&mut conn, &window("DB upgrade", vec![a, b, 9999])).unwrap();
        let w = get(&conn, id).unwrap().unwrap();
        assert_eq!(w.title, "DB upgrade");
        assert_eq!(w.strategy, "manual");
        assert!(w.active);
        // Unknown monitor ids are ignored, not errors.
        assert_eq!(monitor_ids(&conn, id).unwrap(), vec![a, b]);

        let mut changed = window("DB upgrade v2", vec![b]);
        changed.strategy = "daily".into();
        changed.time_from = Some("22:00".into());
        changed.time_to = Some("02:00".into());
        assert!(update(&mut conn, id, &changed).unwrap());
        let w = get(&conn, id).unwrap().unwrap();
        assert_eq!(w.title, "DB upgrade v2");
        assert_eq!(w.time_from.as_deref(), Some("22:00"));
        assert_eq!(monitor_ids(&conn, id).unwrap(), vec![b]);

        assert!(set_active(&conn, id, false).unwrap());
        assert!(list_enabled(&conn).unwrap().is_empty());
        assert_eq!(list(&conn).unwrap().len(), 1);

        assert!(delete(&conn, id).unwrap());
        assert!(get(&conn, id).unwrap().is_none());
        assert!(!update(&mut conn, id, &changed).unwrap());
    }

    #[test]
    fn deleting_a_monitor_drops_its_link() {
        let mut conn = test_conn();
        let a = monitor(&conn, "a");
        let id = insert(&mut conn, &window("w", vec![a])).unwrap();
        monitors::delete_monitor(&conn, a).unwrap();
        assert!(monitor_ids(&conn, id).unwrap().is_empty());
    }
}
