//! Discord live status boards (`live_boards`, `live_board_items`,
//! `live_board_messages`).
//!
//! `webhook_url` is stored sealed (see [`crate::kit::notify::sealed::seal_value`]); nothing
//! in this module opens it, and nothing that leaves the server may carry it.

use rusqlite::{Connection, OptionalExtension, Row, params};

/// Longest board name (characters).
pub const MAX_NAME_LEN: usize = 80;
/// Longest embed title a board sets (Discord allows 256).
pub const MAX_TITLE_LEN: usize = 200;
/// Longest description a board sets (Discord allows 4096).
pub const MAX_DESCRIPTION_LEN: usize = 1000;
/// Shortest refresh interval (seconds).
pub const REFRESH_MIN: i64 = 60;
/// Longest refresh interval (seconds).
pub const REFRESH_MAX: i64 = 86_400;
/// Default refresh interval (seconds).
pub const REFRESH_DEFAULT: i64 = 300;
/// Most monitors and groups one board can list.
pub const MAX_ITEMS: usize = 500;
/// Most boards one instance keeps.
pub const MAX_BOARDS: i64 = 50;
/// Consecutive failed refreshes after which a board pauses itself.
pub const MAX_FAILURES: i64 = 5;

/// A row of `live_boards`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    pub id: i64,
    pub name: String,
    /// The webhook URL, sealed. Never shown, never logged.
    pub webhook_url: String,
    /// `discord` | `fluxer`.
    pub platform: String,
    /// `page` | `items`.
    pub source: String,
    pub status_page_id: Option<i64>,
    pub show_uptime_24h: bool,
    pub show_uptime_30d: bool,
    pub show_response: bool,
    pub show_incidents: bool,
    pub title: String,
    pub description: String,
    /// "" (follow the status), a palette name or `#rrggbb`.
    pub color: String,
    pub refresh_seconds: i64,
    pub notify_down: bool,
    /// A role id (digits), or "".
    pub mention: String,
    pub lang: String,
    /// `active` | `paused`.
    pub state: String,
    pub failures: i64,
    pub last_error: Option<String>,
    pub last_ok_at: Option<String>,
    /// Ids of the monitors that were down at the last refresh.
    pub known_down: Vec<i64>,
    pub created_at: String,
    pub updated_at: String,
}

impl Board {
    pub fn is_paused(&self) -> bool {
        self.state == "paused"
    }
}

/// What a form, the API or an import sets. `webhook_url` is already sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBoard {
    pub name: String,
    pub webhook_url: String,
    pub platform: String,
    pub source: String,
    pub status_page_id: Option<i64>,
    pub show_uptime_24h: bool,
    pub show_uptime_30d: bool,
    pub show_response: bool,
    pub show_incidents: bool,
    pub title: String,
    pub description: String,
    pub color: String,
    pub refresh_seconds: i64,
    pub notify_down: bool,
    pub mention: String,
    pub lang: String,
    /// Monitors and groups of an `items` board, in order.
    pub items: Vec<i64>,
}

/// A message a board keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub slot: i64,
    pub message_id: String,
    pub content_hash: String,
}

const COLUMNS: &str = "id, name, webhook_url, platform, source, status_page_id, show_uptime_24h, show_uptime_30d, \
    show_response, show_incidents, title, description, color, refresh_seconds, notify_down, mention, lang, state, \
    failures, last_error, last_ok_at, known_down, created_at, updated_at";

fn row_to_board(r: &Row) -> rusqlite::Result<Board> {
    let known: String = r.get("known_down")?;
    Ok(Board {
        id: r.get("id")?,
        name: r.get("name")?,
        webhook_url: r.get("webhook_url")?,
        platform: r.get("platform")?,
        source: r.get("source")?,
        status_page_id: r.get("status_page_id")?,
        show_uptime_24h: r.get("show_uptime_24h")?,
        show_uptime_30d: r.get("show_uptime_30d")?,
        show_response: r.get("show_response")?,
        show_incidents: r.get("show_incidents")?,
        title: r.get("title")?,
        description: r.get("description")?,
        color: r.get("color")?,
        refresh_seconds: r.get("refresh_seconds")?,
        notify_down: r.get("notify_down")?,
        mention: r.get("mention")?,
        lang: r.get("lang")?,
        state: r.get("state")?,
        failures: r.get("failures")?,
        last_error: r.get("last_error")?,
        last_ok_at: r.get("last_ok_at")?,
        known_down: serde_json::from_str(&known).unwrap_or_default(),
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

/// Every board, oldest first.
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<Board>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM live_boards ORDER BY id"))?;
    let rows = stmt.query_map([], row_to_board)?;
    rows.collect()
}

/// Boards that are not paused.
pub fn list_active(conn: &Connection) -> rusqlite::Result<Vec<Board>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM live_boards WHERE state = 'active' ORDER BY id"
    ))?;
    let rows = stmt.query_map([], row_to_board)?;
    rows.collect()
}

pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Board>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM live_boards WHERE id = ?1"),
        params![id],
        row_to_board,
    )
    .optional()
}

pub fn get_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<Board>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM live_boards WHERE name = ?1"),
        params![name],
        row_to_board,
    )
    .optional()
}

/// Boards that follow a status page.
pub fn for_page(conn: &Connection, page_id: i64) -> rusqlite::Result<Vec<Board>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM live_boards WHERE status_page_id = ?1 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![page_id], row_to_board)?;
    rows.collect()
}

pub fn count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM live_boards", [], |r| r.get(0))
}

pub fn name_taken(conn: &Connection, name: &str, except_id: Option<i64>) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM live_boards WHERE name = ?1 AND id != ?2)",
        params![name, except_id.unwrap_or(-1)],
        |r| r.get(0),
    )
}

/// The monitors and groups of a board, in order.
pub fn items(conn: &Connection, board_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT monitor_id FROM live_board_items WHERE board_id = ?1 ORDER BY position, monitor_id",
    )?;
    let rows = stmt.query_map(params![board_id], |r| r.get(0))?;
    rows.collect()
}

fn set_items(conn: &Connection, board_id: i64, items: &[i64]) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM live_board_items WHERE board_id = ?1",
        params![board_id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO live_board_items (board_id, monitor_id, position) VALUES (?1, ?2, ?3)",
    )?;
    for (i, id) in items.iter().enumerate() {
        stmt.execute(params![board_id, id, i as i64])?;
    }
    Ok(())
}

/// Runs `f` as one unit (a savepoint, so it also works inside a transaction).
fn atomically<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
    conn.execute_batch("SAVEPOINT live_board")?;
    match f(conn) {
        Ok(v) => {
            conn.execute_batch("RELEASE live_board")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO live_board; RELEASE live_board");
            Err(e)
        }
    }
}

pub fn insert(conn: &Connection, b: &NewBoard) -> rusqlite::Result<i64> {
    atomically(conn, |tx| {
        tx.execute(
        "INSERT INTO live_boards (name, webhook_url, platform, source, status_page_id, show_uptime_24h, \
         show_uptime_30d, show_response, show_incidents, title, description, color, refresh_seconds, \
         notify_down, mention, lang) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            b.name,
            b.webhook_url,
            b.platform,
            b.source,
            b.status_page_id,
            b.show_uptime_24h,
            b.show_uptime_30d,
            b.show_response,
            b.show_incidents,
            b.title,
            b.description,
            b.color,
            b.refresh_seconds,
            b.notify_down,
            b.mention,
            b.lang,
        ],
    )?;
        let id = tx.last_insert_rowid();
        set_items(tx, id, &b.items)?;
        Ok(id)
    })
}

/// Replaces a board's settings. Resets the failure count and un-pauses it:
/// saving is how a broken board is fixed. The messages it keeps stay (a
/// changed webhook starts over: see [`forget_messages`]).
pub fn update(conn: &Connection, id: i64, b: &NewBoard) -> rusqlite::Result<bool> {
    atomically(conn, |tx| {
        let n = tx.execute(
        "UPDATE live_boards SET name = ?2, webhook_url = ?3, platform = ?4, source = ?5, status_page_id = ?6, \
         show_uptime_24h = ?7, show_uptime_30d = ?8, show_response = ?9, show_incidents = ?10, title = ?11, \
         description = ?12, color = ?13, refresh_seconds = ?14, notify_down = ?15, mention = ?16, lang = ?17, \
         state = 'active', failures = 0, last_error = NULL, updated_at = datetime('now') WHERE id = ?1",
        params![
            id,
            b.name,
            b.webhook_url,
            b.platform,
            b.source,
            b.status_page_id,
            b.show_uptime_24h,
            b.show_uptime_30d,
            b.show_response,
            b.show_incidents,
            b.title,
            b.description,
            b.color,
            b.refresh_seconds,
            b.notify_down,
            b.mention,
            b.lang,
        ],
    )?;
        if n == 0 {
            return Ok(false);
        }
        set_items(tx, id, &b.items)?;
        Ok(true)
    })
}

pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM live_boards WHERE id = ?1", params![id])? > 0)
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

pub fn messages(conn: &Connection, board_id: i64) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT slot, message_id, content_hash FROM live_board_messages WHERE board_id = ?1 ORDER BY slot",
    )?;
    let rows = stmt.query_map(params![board_id], |r| {
        Ok(Message {
            slot: r.get(0)?,
            message_id: r.get(1)?,
            content_hash: r.get(2)?,
        })
    })?;
    rows.collect()
}

pub fn put_message(
    conn: &Connection,
    board_id: i64,
    slot: i64,
    message_id: &str,
    content_hash: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO live_board_messages (board_id, slot, message_id, content_hash) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (board_id, slot) DO UPDATE SET message_id = ?3, content_hash = ?4",
        params![board_id, slot, message_id, content_hash],
    )?;
    Ok(())
}

pub fn delete_message(conn: &Connection, board_id: i64, slot: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM live_board_messages WHERE board_id = ?1 AND slot = ?2",
        params![board_id, slot],
    )?;
    Ok(())
}

/// Forgets every message id of a board (a new webhook means a new channel).
pub fn forget_messages(conn: &Connection, board_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM live_board_messages WHERE board_id = ?1",
        params![board_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Refresh results
// ---------------------------------------------------------------------------

pub fn set_known_down(conn: &Connection, id: i64, down: &[i64]) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE live_boards SET known_down = ?2 WHERE id = ?1",
        params![
            id,
            serde_json::to_string(down).unwrap_or_else(|_| "[]".into())
        ],
    )?;
    Ok(())
}

/// A refresh worked: clears the failure count and the error.
pub fn record_ok(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE live_boards SET failures = 0, last_error = NULL, last_ok_at = datetime('now') WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// A refresh failed. Pauses the board when `pause` is set or the failures
/// reach [`MAX_FAILURES`]. Returns whether it is paused now.
pub fn record_failure(
    conn: &Connection,
    id: i64,
    error: &str,
    pause: bool,
) -> rusqlite::Result<bool> {
    conn.execute(
        "UPDATE live_boards SET failures = failures + 1, last_error = ?2, \
         state = CASE WHEN ?3 = 1 OR failures + 1 >= ?4 THEN 'paused' ELSE state END WHERE id = ?1",
        params![id, error, pause, MAX_FAILURES],
    )?;
    conn.query_row(
        "SELECT state = 'paused' FROM live_boards WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )
}

/// "Retry": un-pauses a board and clears its error.
pub fn retry(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE live_boards SET state = 'active', failures = 0, last_error = NULL WHERE id = ?1",
        params![id],
    )? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_board(name: &str) -> NewBoard {
        NewBoard {
            name: name.into(),
            webhook_url: "sealed".into(),
            platform: "discord".into(),
            source: "items".into(),
            status_page_id: None,
            show_uptime_24h: true,
            show_uptime_30d: true,
            show_response: false,
            show_incidents: true,
            title: String::new(),
            description: String::new(),
            color: String::new(),
            refresh_seconds: REFRESH_DEFAULT,
            notify_down: false,
            mention: String::new(),
            lang: "en".into(),
            items: Vec::new(),
        }
    }

    #[test]
    fn insert_update_and_delete_round_trip() {
        let conn = crate::db::test_conn();
        let id = insert(&conn, &new_board("Support")).unwrap();
        let b = get(&conn, id).unwrap().unwrap();
        assert_eq!(b.name, "Support");
        assert_eq!(b.refresh_seconds, 300);
        assert!(!b.is_paused());
        assert!(name_taken(&conn, "support", None).unwrap());
        assert!(!name_taken(&conn, "support", Some(id)).unwrap());

        let mut changed = new_board("Support 2");
        changed.show_response = true;
        assert!(update(&conn, id, &changed).unwrap());
        let b = get(&conn, id).unwrap().unwrap();
        assert_eq!(b.name, "Support 2");
        assert!(b.show_response);

        assert!(delete(&conn, id).unwrap());
        assert!(get(&conn, id).unwrap().is_none());
    }

    #[test]
    fn messages_are_replaced_per_slot_and_go_with_the_board() {
        let conn = crate::db::test_conn();
        let id = insert(&conn, &new_board("A")).unwrap();
        put_message(&conn, id, 0, "111", "h1").unwrap();
        put_message(&conn, id, 1, "222", "h2").unwrap();
        put_message(&conn, id, 0, "333", "h3").unwrap();
        let m = messages(&conn, id).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].message_id, "333");
        delete_message(&conn, id, 1).unwrap();
        assert_eq!(messages(&conn, id).unwrap().len(), 1);
        delete(&conn, id).unwrap();
        assert!(messages(&conn, id).unwrap().is_empty());
    }

    #[test]
    fn repeated_failures_pause_the_board_and_retry_clears_it() {
        let conn = crate::db::test_conn();
        let id = insert(&conn, &new_board("A")).unwrap();
        for _ in 0..MAX_FAILURES - 1 {
            assert!(!record_failure(&conn, id, "boom", false).unwrap());
        }
        assert!(record_failure(&conn, id, "boom", false).unwrap());
        let b = get(&conn, id).unwrap().unwrap();
        assert!(b.is_paused());
        assert_eq!(b.last_error.as_deref(), Some("boom"));
        assert!(retry(&conn, id).unwrap());
        let b = get(&conn, id).unwrap().unwrap();
        assert!(!b.is_paused());
        assert_eq!((b.failures, b.last_error), (0, None));
        // A permanent error pauses at once.
        assert!(record_failure(&conn, id, "gone", true).unwrap());
    }

    #[test]
    fn items_keep_their_order_and_follow_deleted_monitors() {
        let conn = crate::db::test_conn();
        for n in 1..=3 {
            conn.execute(
                "INSERT INTO monitors (id, name, monitor_type, target) VALUES (?1, ?2, 'http', 'https://x.test')",
                params![n, format!("m{n}")],
            )
            .unwrap();
        }
        let mut b = new_board("A");
        b.items = vec![3, 1, 2];
        let id = insert(&conn, &b).unwrap();
        assert_eq!(items(&conn, id).unwrap(), vec![3, 1, 2]);
        conn.execute("DELETE FROM monitors WHERE id = 1", [])
            .unwrap();
        assert_eq!(items(&conn, id).unwrap(), vec![3, 2]);
    }

    #[test]
    fn deleting_a_status_page_keeps_the_board() {
        let conn = crate::db::test_conn();
        conn.execute(
            "INSERT INTO status_pages (id, slug, title) VALUES (1, 'sylo', 'Sylo')",
            [],
        )
        .unwrap();
        let mut b = new_board("A");
        b.source = "page".into();
        b.status_page_id = Some(1);
        let id = insert(&conn, &b).unwrap();
        conn.execute("DELETE FROM status_pages WHERE id = 1", [])
            .unwrap();
        let b = get(&conn, id).unwrap().unwrap();
        assert_eq!(b.status_page_id, None);
    }
}
