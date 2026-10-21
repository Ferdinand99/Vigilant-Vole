//! Key/value settings (`settings` table): the generic accessors and the keys
//! the kit itself reads. Products keep their own keys and typed view next to
//! these.

use rusqlite::{Connection, OptionalExtension, params};

pub const BASE_URL: &str = "base_url";
pub const TIMEZONE: &str = "timezone";
pub const LANGUAGE: &str = "language";
pub const SITE_NAME: &str = "site_name";

pub fn get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |r| r.get(0),
    )
    .optional()
}

pub fn set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
    Ok(())
}

/// Stores a bool as `"1"` / `"0"`.
pub fn set_bool(conn: &Connection, key: &str, value: bool) -> rusqlite::Result<()> {
    set(conn, key, if value { "1" } else { "0" })
}

/// Stores `Some` as the value, `None` (or blank) by deleting the key.
pub fn set_opt(conn: &Connection, key: &str, value: Option<&str>) -> rusqlite::Result<()> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => set(conn, key, v),
        None => delete(conn, key),
    }
}
