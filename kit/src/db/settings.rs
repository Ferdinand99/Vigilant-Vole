//! Key/value settings (`settings` table): the generic accessors and the keys
//! the kit itself reads. Products keep their own keys and typed view next to
//! these.

use rusqlite::{Connection, OptionalExtension, params};

pub const BASE_URL: &str = "base_url";
pub const TIMEZONE: &str = "timezone";
pub const LANGUAGE: &str = "language";
pub const SITE_NAME: &str = "site_name";

/// The instance's mail settings ([`crate::mail`]): server, port, security
/// (`starttls`, `tls` or `none`), user name, the password (sealed) and the
/// sender.
pub const SMTP_HOST: &str = "smtp_host";
pub const SMTP_PORT: &str = "smtp_port";
pub const SMTP_SECURITY: &str = "smtp_security";
pub const SMTP_USERNAME: &str = "smtp_username";
pub const SMTP_PASSWORD: &str = "smtp_password";
pub const SMTP_FROM: &str = "smtp_from";

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

/// `"1"`, `"true"`, `"on"` and `"yes"` (any case) are true; anything else is not.
pub fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// The configured public URL without trailing slash, if one is set.
pub fn base_url(conn: &Connection) -> rusqlite::Result<Option<String>> {
    Ok(get(conn, BASE_URL)?.and_then(|v| {
        let t = v.trim();
        (!t.is_empty()).then(|| t.trim_end_matches('/').to_string())
    }))
}
