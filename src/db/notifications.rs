use rusqlite::{Connection, Row, params};

use super::models::{ChannelKind, NotificationChannel};

pub fn insert_channel(conn: &Connection, name: &str, kind: ChannelKind, config: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO notification_channels (name, kind, config) VALUES (?1, ?2, ?3)",
        params![name, kind.as_str(), config],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_channel(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM notification_channels WHERE id = ?1", params![id])?;
    Ok(())
}

pub fn list_channels(conn: &Connection) -> rusqlite::Result<Vec<NotificationChannel>> {
    let mut stmt = conn.prepare("SELECT id, name, kind, config, active FROM notification_channels ORDER BY name")?;
    let rows = stmt.query_map([], row_to_channel)?;
    rows.collect()
}

pub fn list_active_channels(conn: &Connection) -> rusqlite::Result<Vec<NotificationChannel>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, config, active FROM notification_channels WHERE active = 1 ORDER BY name",
    )?;
    let rows = stmt.query_map([], row_to_channel)?;
    rows.collect()
}

fn row_to_channel(row: &Row) -> rusqlite::Result<NotificationChannel> {
    let kind: String = row.get(2)?;
    let active: i64 = row.get(4)?;
    Ok(NotificationChannel {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: ChannelKind::parse(&kind).unwrap_or(ChannelKind::Webhook),
        config: row.get(3)?,
        active: active != 0,
    })
}
