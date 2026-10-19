//! Notification channel storage and monitor ↔ channel links. Owned by the
//! notifications agent.
//!
//! `config` is the provider's JSON object *including secrets*; never send it
//! to a browser as-is — use `notification::redacted_config`.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::models::NotificationChannel;

const CHANNEL_COLUMNS: &str = "id, name, kind, config, active, is_default, created_at";

/// Creates a channel. When `is_default`, it is also attached to every existing
/// monitor (and, via `monitors::insert_monitor`, to every future one) — this
/// keeps the pre-0002 "every channel notifies every monitor" behaviour.
pub fn insert_channel(
    conn: &Connection,
    name: &str,
    kind: &str,
    config: &str,
    is_default: bool,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO notification_channels (name, kind, config, is_default) VALUES (?1, ?2, ?3, ?4)",
        params![name, kind, config, is_default],
    )?;
    let id = conn.last_insert_rowid();
    if is_default {
        apply_to_all(conn, id)?;
    }
    Ok(id)
}

/// Updates name, config and the default flag (the kind never changes).
/// Returns `false` when the channel doesn't exist. Turning `is_default` on
/// here does not link existing monitors — call [`apply_to_all`] for that.
pub fn update_channel(
    conn: &Connection,
    id: i64,
    name: &str,
    config: &str,
    is_default: bool,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE notification_channels SET name = ?2, config = ?3, is_default = ?4 WHERE id = ?1",
        params![id, name, config, is_default],
    )?;
    Ok(n > 0)
}

pub fn delete_channel(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM notification_channels WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn get_channel(conn: &Connection, id: i64) -> rusqlite::Result<Option<NotificationChannel>> {
    conn.query_row(
        &format!("SELECT {CHANNEL_COLUMNS} FROM notification_channels WHERE id = ?1"),
        params![id],
        row_to_channel,
    )
    .optional()
}

/// Pauses/resumes a channel. Returns `false` when it doesn't exist.
pub fn set_active(conn: &Connection, id: i64, active: bool) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE notification_channels SET active = ?2 WHERE id = ?1",
        params![id, active],
    )?;
    Ok(n > 0)
}

/// Whether new monitors get this channel automatically. Returns `false` when
/// it doesn't exist.
pub fn set_default(conn: &Connection, id: i64, is_default: bool) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE notification_channels SET is_default = ?2 WHERE id = ?1",
        params![id, is_default],
    )?;
    Ok(n > 0)
}

pub fn list_channels(conn: &Connection) -> rusqlite::Result<Vec<NotificationChannel>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CHANNEL_COLUMNS} FROM notification_channels ORDER BY name COLLATE NOCASE, id"
    ))?;
    let rows = stmt.query_map([], row_to_channel)?;
    rows.collect()
}

/// Every channel with the number of monitors linked to it (regular or escalation).
pub fn list_channels_with_counts(
    conn: &Connection,
) -> rusqlite::Result<Vec<(NotificationChannel, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.name, c.kind, c.config, c.active, c.is_default, c.created_at,
                (SELECT COUNT(*) FROM monitor_notifications mn WHERE mn.channel_id = c.id) AS link_count
         FROM notification_channels c
         ORDER BY c.name COLLATE NOCASE, c.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row_to_channel(row)?, row.get::<_, i64>("link_count")?))
    })?;
    rows.collect()
}

pub fn list_active_channels(conn: &Connection) -> rusqlite::Result<Vec<NotificationChannel>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CHANNEL_COLUMNS} FROM notification_channels WHERE active = 1 ORDER BY name COLLATE NOCASE, id"
    ))?;
    let rows = stmt.query_map([], row_to_channel)?;
    rows.collect()
}

/// Active channels linked to a monitor; `escalation` selects the regular
/// (`false`) or the escalation (`true`) set.
pub fn list_channels_for_monitor(
    conn: &Connection,
    monitor_id: i64,
    escalation: bool,
) -> rusqlite::Result<Vec<NotificationChannel>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.name, c.kind, c.config, c.active, c.is_default, c.created_at
         FROM notification_channels c
         JOIN monitor_notifications mn ON mn.channel_id = c.id
         WHERE mn.monitor_id = ?1 AND mn.escalation = ?2 AND c.active = 1
         ORDER BY c.name COLLATE NOCASE, c.id",
    )?;
    let rows = stmt.query_map(params![monitor_id, escalation], row_to_channel)?;
    rows.collect()
}

/// A monitor's links as `(channel_id, escalation)`, ordered by channel id.
pub fn monitor_channel_links(
    conn: &Connection,
    monitor_id: i64,
) -> rusqlite::Result<Vec<(i64, bool)>> {
    let mut stmt = conn.prepare(
        "SELECT channel_id, escalation FROM monitor_notifications WHERE monitor_id = ?1 ORDER BY channel_id",
    )?;
    let rows = stmt.query_map(params![monitor_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// Ids of the channels new monitors get by default (for pre-checking the
/// monitor form).
pub fn default_channel_ids(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT id FROM notification_channels WHERE is_default = 1 ORDER BY id")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    rows.collect()
}

/// Replaces all of a monitor's channel links with `links` —
/// `(channel_id, escalation)` pairs. A channel listed twice keeps the last
/// entry. Unknown channel ids are an error (foreign key). Atomic: runs in a
/// savepoint, so it is safe inside or outside a caller's transaction.
pub fn set_monitor_channels(
    conn: &Connection,
    monitor_id: i64,
    links: &[(i64, bool)],
) -> rusqlite::Result<()> {
    conn.execute_batch("SAVEPOINT vv_set_monitor_channels")?;
    let result = (|| {
        conn.execute(
            "DELETE FROM monitor_notifications WHERE monitor_id = ?1",
            params![monitor_id],
        )?;
        let mut stmt = conn.prepare(
            "INSERT OR REPLACE INTO monitor_notifications (monitor_id, channel_id, escalation) VALUES (?1, ?2, ?3)",
        )?;
        for (channel_id, escalation) in links {
            stmt.execute(params![monitor_id, channel_id, escalation])?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("RELEASE vv_set_monitor_channels"),
        Err(err) => {
            // Best effort: undo and leave the savepoint; report the original error.
            let _ = conn.execute_batch(
                "ROLLBACK TO vv_set_monitor_channels; RELEASE vv_set_monitor_channels",
            );
            Err(err)
        }
    }
}

/// Links one channel to one monitor (insert or update the escalation flag).
pub fn link_monitor(
    conn: &Connection,
    monitor_id: i64,
    channel_id: i64,
    escalation: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO monitor_notifications (monitor_id, channel_id, escalation) VALUES (?1, ?2, ?3)
         ON CONFLICT (monitor_id, channel_id) DO UPDATE SET escalation = excluded.escalation",
        params![monitor_id, channel_id, escalation],
    )?;
    Ok(())
}

pub fn unlink_monitor(conn: &Connection, monitor_id: i64, channel_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM monitor_notifications WHERE monitor_id = ?1 AND channel_id = ?2",
        params![monitor_id, channel_id],
    )?;
    Ok(())
}

/// Links the channel to every monitor that isn't linked yet (as a regular,
/// non-escalation channel). Returns how many links were added.
pub fn apply_to_all(conn: &Connection, channel_id: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT OR IGNORE INTO monitor_notifications (monitor_id, channel_id, escalation)
         SELECT id, ?1, 0 FROM monitors",
        params![channel_id],
    )
}

/// How many monitors use this channel (regular or escalation).
pub fn count_links(conn: &Connection, channel_id: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM monitor_notifications WHERE channel_id = ?1",
        params![channel_id],
        |r| r.get(0),
    )
}

pub fn row_to_channel(row: &Row) -> rusqlite::Result<NotificationChannel> {
    Ok(NotificationChannel {
        id: row.get("id")?,
        name: row.get("name")?,
        kind: row.get("kind")?,
        config: row.get("config")?,
        active: row.get("active")?,
        is_default: row.get("is_default")?,
        created_at: row.get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{MonitorType, NewMonitor};
    use crate::db::{monitors, test_conn};

    fn monitor(conn: &Connection, name: &str) -> i64 {
        let m = NewMonitor {
            name: name.into(),
            monitor_type: MonitorType::Http,
            target: "https://example.com".into(),
            ..NewMonitor::default()
        };
        monitors::insert_monitor(conn, &m).unwrap()
    }

    #[test]
    fn insert_get_update_delete_round_trip() {
        let conn = test_conn();
        let id = insert_channel(&conn, "Ops", "webhook", r#"{"url":"https://x"}"#, false).unwrap();
        let ch = get_channel(&conn, id).unwrap().unwrap();
        assert_eq!(
            (ch.name.as_str(), ch.kind.as_str(), ch.active, ch.is_default),
            ("Ops", "webhook", true, false)
        );

        assert!(update_channel(&conn, id, "Ops 2", r#"{"url":"https://y"}"#, true).unwrap());
        let ch = get_channel(&conn, id).unwrap().unwrap();
        assert_eq!(ch.name, "Ops 2");
        assert_eq!(ch.config, r#"{"url":"https://y"}"#);
        assert!(ch.is_default);
        assert!(!update_channel(&conn, 999, "x", "{}", false).unwrap());

        delete_channel(&conn, id).unwrap();
        assert!(get_channel(&conn, id).unwrap().is_none());
    }

    #[test]
    fn active_and_default_flags() {
        let conn = test_conn();
        let id = insert_channel(&conn, "A", "slack", "{}", false).unwrap();
        assert!(set_active(&conn, id, false).unwrap());
        assert!(list_active_channels(&conn).unwrap().is_empty());
        assert!(set_default(&conn, id, true).unwrap());
        assert_eq!(default_channel_ids(&conn).unwrap(), [id]);
        assert!(!set_active(&conn, 42, true).unwrap());
    }

    #[test]
    fn default_channels_link_existing_and_new_monitors() {
        let conn = test_conn();
        let m1 = monitor(&conn, "one");
        let ch = insert_channel(&conn, "Default", "webhook", "{}", true).unwrap();
        let m2 = monitor(&conn, "two");
        assert_eq!(monitor_channel_links(&conn, m1).unwrap(), [(ch, false)]);
        assert_eq!(monitor_channel_links(&conn, m2).unwrap(), [(ch, false)]);
        assert_eq!(count_links(&conn, ch).unwrap(), 2);
    }

    #[test]
    fn set_monitor_channels_replaces_links() {
        let conn = test_conn();
        let m = monitor(&conn, "m");
        let a = insert_channel(&conn, "A", "webhook", "{}", false).unwrap();
        let b = insert_channel(&conn, "B", "webhook", "{}", false).unwrap();
        set_monitor_channels(&conn, m, &[(a, false), (b, true)]).unwrap();
        assert_eq!(
            monitor_channel_links(&conn, m).unwrap(),
            [(a, false), (b, true)]
        );
        assert_eq!(list_channels_for_monitor(&conn, m, false).unwrap()[0].id, a);
        assert_eq!(list_channels_for_monitor(&conn, m, true).unwrap()[0].id, b);

        set_monitor_channels(&conn, m, &[(b, false), (b, true)]).unwrap();
        assert_eq!(
            monitor_channel_links(&conn, m).unwrap(),
            [(b, true)],
            "last entry wins"
        );

        set_monitor_channels(&conn, m, &[]).unwrap();
        assert!(monitor_channel_links(&conn, m).unwrap().is_empty());
    }

    #[test]
    fn set_monitor_channels_is_atomic_on_error() {
        let conn = test_conn();
        let m = monitor(&conn, "m");
        let a = insert_channel(&conn, "A", "webhook", "{}", false).unwrap();
        set_monitor_channels(&conn, m, &[(a, false)]).unwrap();
        assert!(set_monitor_channels(&conn, m, &[(a, true), (9999, false)]).is_err());
        assert_eq!(
            monitor_channel_links(&conn, m).unwrap(),
            [(a, false)],
            "old links survive"
        );
        // Still usable inside a caller's transaction.
        conn.execute_batch("BEGIN").unwrap();
        set_monitor_channels(&conn, m, &[(a, true)]).unwrap();
        conn.execute_batch("COMMIT").unwrap();
        assert_eq!(monitor_channel_links(&conn, m).unwrap(), [(a, true)]);
    }

    #[test]
    fn link_unlink_apply_all_and_counts() {
        let conn = test_conn();
        let m1 = monitor(&conn, "one");
        let m2 = monitor(&conn, "two");
        let ch = insert_channel(&conn, "C", "webhook", "{}", false).unwrap();
        link_monitor(&conn, m1, ch, true).unwrap();
        link_monitor(&conn, m1, ch, false).unwrap();
        assert_eq!(monitor_channel_links(&conn, m1).unwrap(), [(ch, false)]);
        assert_eq!(apply_to_all(&conn, ch).unwrap(), 1, "only m2 was missing");
        let counts = list_channels_with_counts(&conn).unwrap();
        assert_eq!(counts[0].1, 2);
        unlink_monitor(&conn, m2, ch).unwrap();
        assert_eq!(count_links(&conn, ch).unwrap(), 1);
        delete_channel(&conn, ch).unwrap();
        assert!(
            monitor_channel_links(&conn, m1).unwrap().is_empty(),
            "links cascade"
        );
    }
}
