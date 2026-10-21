//! Key/value application settings (`settings` table) and the typed view of them.

use rusqlite::{Connection, OptionalExtension, params};

pub const RETENTION_DAYS: &str = "retention_days";
pub const BASE_URL: &str = "base_url";
pub const TIMEZONE: &str = "timezone";
pub const LANGUAGE: &str = "language";
pub const SITE_NAME: &str = "site_name";
pub const DISCOVERY_ENABLED: &str = "discovery_enabled";
pub const DISCOVERY_DOCKER_HOST: &str = "discovery_docker_host";
pub const METRICS_ENABLED: &str = "metrics_enabled";
pub const BADGES_PUBLIC: &str = "badges_public";
pub const AGENT_CONFIRMATION_DEFAULT: &str = "agent_confirmation_default";

/// Typed settings with defaults applied for missing/invalid values.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub retention_days: i64,
    /// Public URL of this instance without trailing slash, used for deep links.
    pub base_url: Option<String>,
    /// IANA zone name used for displaying times.
    pub timezone: String,
    pub language: String,
    pub site_name: String,
    pub discovery_enabled: bool,
    pub discovery_docker_host: Option<String>,
    pub metrics_enabled: bool,
    pub badges_public: bool,
    pub agent_confirmation_default: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            retention_days: 90,
            base_url: None,
            timezone: "UTC".into(),
            language: "en".into(),
            site_name: "Vigilant Vole".into(),
            discovery_enabled: false,
            discovery_docker_host: None,
            metrics_enabled: false,
            badges_public: false,
            agent_confirmation_default: false,
        }
    }
}

impl Settings {
    /// Deep link to a monitor's detail page, when a base URL is configured.
    pub fn monitor_url(&self, monitor_id: i64) -> Option<String> {
        self.base_url
            .as_deref()
            .map(|base| format!("{}/monitors/{monitor_id}", base.trim_end_matches('/')))
    }
}

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

/// Loads every setting, falling back to [`Settings::default`] per key.
pub fn load(conn: &Connection) -> rusqlite::Result<Settings> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut s = Settings::default();
    for row in rows {
        let (key, value) = row?;
        apply(&mut s, &key, value);
    }
    Ok(s)
}

/// Writes every field of `s`.
pub fn save(conn: &Connection, s: &Settings) -> rusqlite::Result<()> {
    set(conn, RETENTION_DAYS, &s.retention_days.to_string())?;
    set_opt(conn, BASE_URL, s.base_url.as_deref())?;
    set(conn, TIMEZONE, &s.timezone)?;
    set(conn, LANGUAGE, &s.language)?;
    set(conn, SITE_NAME, &s.site_name)?;
    set_bool(conn, DISCOVERY_ENABLED, s.discovery_enabled)?;
    set_opt(
        conn,
        DISCOVERY_DOCKER_HOST,
        s.discovery_docker_host.as_deref(),
    )?;
    set_bool(conn, METRICS_ENABLED, s.metrics_enabled)?;
    set_bool(conn, BADGES_PUBLIC, s.badges_public)?;
    set_bool(
        conn,
        AGENT_CONFIRMATION_DEFAULT,
        s.agent_confirmation_default,
    )?;
    Ok(())
}

fn apply(s: &mut Settings, key: &str, value: String) {
    let non_empty = |v: String| {
        let t = v.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    match key {
        RETENTION_DAYS => {
            if let Ok(days) = value.trim().parse::<i64>()
                && days > 0
            {
                s.retention_days = days;
            }
        }
        BASE_URL => s.base_url = non_empty(value).map(|v| v.trim_end_matches('/').to_string()),
        TIMEZONE => {
            if let Some(v) = non_empty(value) {
                s.timezone = v;
            }
        }
        LANGUAGE => {
            if let Some(v) = non_empty(value) {
                s.language = v;
            }
        }
        SITE_NAME => {
            if let Some(v) = non_empty(value) {
                s.site_name = v;
            }
        }
        DISCOVERY_ENABLED => s.discovery_enabled = parse_bool(&value),
        DISCOVERY_DOCKER_HOST => s.discovery_docker_host = non_empty(value),
        METRICS_ENABLED => s.metrics_enabled = parse_bool(&value),
        BADGES_PUBLIC => s.badges_public = parse_bool(&value),
        AGENT_CONFIRMATION_DEFAULT => s.agent_confirmation_default = parse_bool(&value),
        _ => {}
    }
}

pub fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_conn;

    #[test]
    fn load_returns_defaults_on_empty_table() {
        let conn = test_conn();
        assert_eq!(load(&conn).unwrap(), Settings::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let conn = test_conn();
        let s = Settings {
            retention_days: 30,
            base_url: Some("https://up.example.com".into()),
            timezone: "Europe/Oslo".into(),
            language: "nb".into(),
            site_name: "Ops".into(),
            discovery_enabled: true,
            discovery_docker_host: Some("unix:///var/run/docker.sock".into()),
            metrics_enabled: true,
            badges_public: true,
            agent_confirmation_default: true,
        };
        save(&conn, &s).unwrap();
        assert_eq!(load(&conn).unwrap(), s);
    }

    #[test]
    fn invalid_values_fall_back_to_defaults() {
        let conn = test_conn();
        set(&conn, RETENTION_DAYS, "-3").unwrap();
        set(&conn, SITE_NAME, "   ").unwrap();
        let s = load(&conn).unwrap();
        assert_eq!(
            (s.retention_days, s.site_name.as_str()),
            (90, "Vigilant Vole")
        );
    }

    #[test]
    fn base_url_trailing_slash_is_trimmed_for_links() {
        let conn = test_conn();
        set(&conn, BASE_URL, "https://up.example.com/").unwrap();
        let s = load(&conn).unwrap();
        assert_eq!(
            s.monitor_url(7).as_deref(),
            Some("https://up.example.com/monitors/7")
        );
    }

    #[test]
    fn get_and_set_overwrite() {
        let conn = test_conn();
        set(&conn, "k", "a").unwrap();
        set(&conn, "k", "b").unwrap();
        assert_eq!(get(&conn, "k").unwrap().as_deref(), Some("b"));
    }
}
