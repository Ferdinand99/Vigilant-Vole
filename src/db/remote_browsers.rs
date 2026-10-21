//! Remote browsers (`remote_browsers`) for real-browser monitors, and the
//! latest failure screenshot per monitor (`monitor_screenshots`).
//!
//! A remote browser's `url` may carry a token or password (browserless
//! `?token=…`, `ws://user:pass@…`). It is stored as given; everything that
//! shows or exports it masks it first (see
//! `monitor::checkers::real_browser::endpoint::masked`).

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

/// Largest screenshot kept (bytes). Bigger captures are not stored.
pub const MAX_SCREENSHOT_BYTES: usize = 768 * 1024;
/// Longest remote browser name (characters).
pub const MAX_NAME_LEN: usize = 80;

/// A row of `remote_browsers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteBrowser {
    pub id: i64,
    pub name: String,
    /// CDP endpoint, unmasked.
    pub url: String,
    pub created_at: String,
    pub updated_at: String,
}

fn row_to_browser(r: &Row) -> rusqlite::Result<RemoteBrowser> {
    Ok(RemoteBrowser {
        id: r.get("id")?,
        name: r.get("name")?,
        url: r.get("url")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

const COLUMNS: &str = "id, name, url, created_at, updated_at";

/// Every remote browser, oldest first (the first one is the default).
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<RemoteBrowser>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM remote_browsers ORDER BY id"
    ))?;
    let rows = stmt.query_map([], row_to_browser)?;
    rows.collect()
}

/// Ids of every remote browser, oldest first.
pub fn ids(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM remote_browsers ORDER BY id")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    rows.collect()
}

pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<RemoteBrowser>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM remote_browsers WHERE id = ?1"),
        params![id],
        row_to_browser,
    )
    .optional()
}

/// By name, case-insensitively.
pub fn get_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<RemoteBrowser>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM remote_browsers WHERE name = ?1"),
        params![name.trim()],
        row_to_browser,
    )
    .optional()
}

/// The browser a monitor uses: its own, or the first configured one when it
/// has none (`None` when there is no remote browser at all, or its own was
/// deleted and none is left).
pub fn for_monitor(
    conn: &Connection,
    remote_browser_id: Option<i64>,
) -> rusqlite::Result<Option<RemoteBrowser>> {
    if let Some(id) = remote_browser_id
        && let Some(b) = get(conn, id)?
    {
        return Ok(Some(b));
    }
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM remote_browsers ORDER BY id LIMIT 1"),
        [],
        row_to_browser,
    )
    .optional()
}

pub fn insert(conn: &Connection, name: &str, url: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO remote_browsers (name, url) VALUES (?1, ?2)",
        params![name.trim(), url.trim()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Returns whether the browser exists.
pub fn update(conn: &Connection, id: i64, name: &str, url: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE remote_browsers SET name = ?1, url = ?2, updated_at = datetime('now') WHERE id = ?3",
        params![name.trim(), url.trim(), id],
    )?;
    Ok(n > 0)
}

/// Returns whether the browser existed. Monitors that used it fall back to
/// the first remote browser left (`ON DELETE SET NULL`).
pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM remote_browsers WHERE id = ?1", params![id])? > 0)
}

/// Whether another browser (not `except`) already has this name.
pub fn name_taken(conn: &Connection, name: &str, except: Option<i64>) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_browsers WHERE name = ?1 AND id IS NOT ?2)",
        params![name.trim(), except],
        |r| r.get(0),
    )
}

/// Real-browser monitors per remote browser id. Monitors without one count
/// for the first (default) browser.
pub fn usage(conn: &Connection) -> rusqlite::Result<HashMap<i64, i64>> {
    let first: Option<i64> =
        conn.query_row("SELECT MIN(id) FROM remote_browsers", [], |r| r.get(0))?;
    let mut stmt = conn.prepare(
        "SELECT remote_browser_id, COUNT(*) FROM monitors WHERE monitor_type = 'real_browser' GROUP BY remote_browser_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, i64>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, n) = row?;
        if let Some(id) = id.or(first) {
            *out.entry(id).or_insert(0) += n;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Screenshots
// ---------------------------------------------------------------------------

/// The latest failure screenshot of a monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screenshot {
    pub image: Vec<u8>,
    pub mime: String,
    /// The check's message when it was taken.
    pub message: Option<String>,
    /// UTC `YYYY-MM-DD HH:MM:SS`.
    pub taken_at: String,
}

/// Screenshot details without the image (for pages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenshotInfo {
    pub message: Option<String>,
    pub taken_at: String,
    pub bytes: i64,
}

/// Stores (overwrites) the monitor's screenshot. Returns `false` (and keeps
/// the previous one) when the image is empty or over [`MAX_SCREENSHOT_BYTES`].
pub fn save_screenshot(
    conn: &Connection,
    monitor_id: i64,
    image: &[u8],
    mime: &str,
    message: Option<&str>,
) -> rusqlite::Result<bool> {
    if image.is_empty() || image.len() > MAX_SCREENSHOT_BYTES {
        return Ok(false);
    }
    let message: Option<String> = message.map(|m| m.chars().take(500).collect());
    // The monitor may have been deleted while its check ran.
    let n = conn.execute(
        "INSERT INTO monitor_screenshots (monitor_id, image, mime, message, taken_at)
         SELECT ?1, ?2, ?3, ?4, datetime('now') WHERE EXISTS(SELECT 1 FROM monitors WHERE id = ?1)
         ON CONFLICT(monitor_id) DO UPDATE SET image = excluded.image, mime = excluded.mime,
             message = excluded.message, taken_at = excluded.taken_at",
        params![monitor_id, image, mime, message],
    )?;
    Ok(n > 0)
}

pub fn get_screenshot(conn: &Connection, monitor_id: i64) -> rusqlite::Result<Option<Screenshot>> {
    conn.query_row(
        "SELECT image, mime, message, taken_at FROM monitor_screenshots WHERE monitor_id = ?1",
        params![monitor_id],
        |r| {
            Ok(Screenshot {
                image: r.get(0)?,
                mime: r.get(1)?,
                message: r.get(2)?,
                taken_at: r.get(3)?,
            })
        },
    )
    .optional()
}

pub fn screenshot_info(
    conn: &Connection,
    monitor_id: i64,
) -> rusqlite::Result<Option<ScreenshotInfo>> {
    conn.query_row(
        "SELECT message, taken_at, length(image) FROM monitor_screenshots WHERE monitor_id = ?1",
        params![monitor_id],
        |r| {
            Ok(ScreenshotInfo {
                message: r.get(0)?,
                taken_at: r.get(1)?,
                bytes: r.get(2)?,
            })
        },
    )
    .optional()
}

pub fn delete_screenshot(conn: &Connection, monitor_id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM monitor_screenshots WHERE monitor_id = ?1",
        params![monitor_id],
    )? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{MonitorType, NewMonitor};
    use crate::db::monitors::insert_monitor;
    use crate::db::test_conn;

    fn browser_monitor(conn: &Connection, name: &str, browser: Option<i64>) -> i64 {
        insert_monitor(
            conn,
            &NewMonitor {
                name: name.into(),
                monitor_type: MonitorType::RealBrowser,
                target: "https://example.com".into(),
                remote_browser_id: browser,
                ..NewMonitor::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn migration_adds_the_table_and_columns_to_fresh_and_upgraded_databases() {
        // Fresh.
        let conn = test_conn();
        let m = browser_monitor(&conn, "fresh", None);
        let fresh = crate::db::monitors::get_monitor(&conn, m).unwrap().unwrap();
        assert_eq!(
            (
                fresh.browser_wait.as_str(),
                fresh.browser_viewport.as_str(),
                fresh.browser_js_errors
            ),
            ("load", "desktop", false)
        );

        // Upgrade: a database from before 0099 keeps its monitors and gets the defaults.
        let mut old = rusqlite::Connection::open_in_memory().unwrap();
        old.execute_batch(crate::db::CONNECTION_PRAGMAS).unwrap();
        crate::db::prepare_tracking(&old).unwrap();
        for (name, sql) in crate::db::embedded_migrations()
            .into_iter()
            .filter(|(n, _)| n.as_str() < "0099")
        {
            crate::db::apply_one(&mut old, &name, &sql).unwrap();
        }
        old.execute_batch(
            "INSERT INTO monitors (id, name, monitor_type, target, keyword) VALUES (1, 'Web', 'http', 'https://a.test', 'OK'),
                 (2, 'Shop', 'real_browser', 'https://shop.test', NULL);
             INSERT INTO heartbeats (monitor_id, status, checked_at) VALUES (1, 'up', '2026-01-01 00:00:00');",
        )
        .unwrap();
        let ran = crate::db::apply_migrations(&mut old).unwrap();
        assert!(ran.iter().any(|n| n.starts_with("0099")), "{ran:?}");
        let web = crate::db::monitors::get_monitor(&old, 1).unwrap().unwrap();
        assert_eq!(
            (
                web.keyword.as_deref(),
                web.remote_browser_id,
                web.browser_selector.as_deref()
            ),
            (Some("OK"), None, None)
        );
        let shop = crate::db::monitors::get_monitor(&old, 2).unwrap().unwrap();
        assert_eq!(
            (shop.browser_wait.as_str(), shop.browser_viewport.as_str()),
            ("load", "desktop")
        );
        let beats: i64 = old
            .query_row("SELECT COUNT(*) FROM heartbeats", [], |r| r.get(0))
            .unwrap();
        assert_eq!(beats, 1);
        // The new foreign keys work on the upgraded schema.
        let b = insert(&old, "Chrome", "ws://chrome:9222").unwrap();
        old.execute(
            "UPDATE monitors SET remote_browser_id = ?1 WHERE id = 2",
            params![b],
        )
        .unwrap();
        assert!(
            old.execute(
                "UPDATE monitors SET remote_browser_id = 999 WHERE id = 2",
                []
            )
            .is_err(),
            "unknown browsers are refused"
        );
        assert!(save_screenshot(&old, 2, b"jpeg", "image/jpeg", None).unwrap());
        delete(&old, b).unwrap();
        assert_eq!(
            crate::db::monitors::get_monitor(&old, 2)
                .unwrap()
                .unwrap()
                .remote_browser_id,
            None
        );
        old.execute("DELETE FROM monitors WHERE id = 2", [])
            .unwrap();
        assert!(get_screenshot(&old, 2).unwrap().is_none());
    }

    #[test]
    fn crud_names_and_default_browser() {
        let conn = test_conn();
        assert!(for_monitor(&conn, None).unwrap().is_none());
        let a = insert(&conn, " Chrome A ", "ws://a:3000").unwrap();
        let b = insert(&conn, "Chrome B", "http://b:9222").unwrap();
        assert_eq!(get(&conn, a).unwrap().unwrap().name, "Chrome A");
        assert!(
            insert(&conn, "chrome a", "ws://dup").is_err(),
            "names are unique, case-insensitively"
        );
        assert!(name_taken(&conn, "CHROME B", None).unwrap());
        assert!(!name_taken(&conn, "Chrome B", Some(b)).unwrap());
        assert_eq!(get_by_name(&conn, "chrome b").unwrap().unwrap().id, b);
        assert_eq!(
            for_monitor(&conn, None).unwrap().unwrap().id,
            a,
            "the first browser is the default"
        );
        assert_eq!(for_monitor(&conn, Some(b)).unwrap().unwrap().id, b);
        assert_eq!(for_monitor(&conn, Some(999)).unwrap().unwrap().id, a);
        assert!(update(&conn, b, "Chrome B2", "ws://b2:3000").unwrap());
        assert_eq!(get(&conn, b).unwrap().unwrap().url, "ws://b2:3000");
        assert_eq!(ids(&conn).unwrap(), vec![a, b]);
        assert_eq!(list(&conn).unwrap().len(), 2);
    }

    #[test]
    fn usage_counts_default_and_deleting_unlinks_monitors() {
        let conn = test_conn();
        let a = insert(&conn, "A", "ws://a").unwrap();
        let b = insert(&conn, "B", "ws://b").unwrap();
        browser_monitor(&conn, "one", None);
        browser_monitor(&conn, "two", Some(a));
        let m = browser_monitor(&conn, "three", Some(b));
        let used = usage(&conn).unwrap();
        assert_eq!((used.get(&a), used.get(&b)), (Some(&2), Some(&1)));
        assert!(delete(&conn, b).unwrap());
        assert!(!delete(&conn, b).unwrap());
        let monitor = crate::db::monitors::get_monitor(&conn, m).unwrap().unwrap();
        assert_eq!(
            monitor.remote_browser_id, None,
            "falls back to the default browser"
        );
        assert_eq!(usage(&conn).unwrap().get(&a), Some(&3));
    }

    #[test]
    fn screenshots_are_capped_overwritten_and_deleted_with_the_monitor() {
        let conn = test_conn();
        let m = browser_monitor(&conn, "shot", None);
        assert!(save_screenshot(&conn, m, b"first", "image/jpeg", Some("HTTP 500")).unwrap());
        assert!(
            save_screenshot(
                &conn,
                m,
                b"second",
                "image/jpeg",
                Some("Selector #app not found")
            )
            .unwrap()
        );
        let shot = get_screenshot(&conn, m).unwrap().unwrap();
        assert_eq!(
            (shot.image.as_slice(), shot.message.as_deref()),
            (&b"second"[..], Some("Selector #app not found"))
        );
        // Too big or empty: refused, the previous one stays.
        let huge = vec![0u8; MAX_SCREENSHOT_BYTES + 1];
        assert!(!save_screenshot(&conn, m, &huge, "image/jpeg", None).unwrap());
        assert!(!save_screenshot(&conn, m, b"", "image/jpeg", None).unwrap());
        assert!(
            save_screenshot(
                &conn,
                m,
                &vec![1u8; MAX_SCREENSHOT_BYTES],
                "image/jpeg",
                None
            )
            .unwrap()
        );
        assert_eq!(
            screenshot_info(&conn, m).unwrap().unwrap().bytes,
            MAX_SCREENSHOT_BYTES as i64
        );
        // Unknown monitor: nothing stored.
        assert!(!save_screenshot(&conn, 9999, b"x", "image/jpeg", None).unwrap());
        conn.execute("DELETE FROM monitors WHERE id = ?1", params![m])
            .unwrap();
        assert!(
            get_screenshot(&conn, m).unwrap().is_none(),
            "cascades with the monitor"
        );
        assert!(!delete_screenshot(&conn, m).unwrap());
    }
}
