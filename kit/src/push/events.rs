//! The text of each push (`push_events`).
//!
//! A push carries only a random `ref`. The text stays here for 24 hours; the
//! app fetches it with `GET /api/v1/push/events/{ref}` after the push
//! arrives. The text is the alert's own, already translated into the
//! channel's language. No URL is stored: the app never opens anything from
//! a push.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use time::format_description::well_known::Rfc3339;

use crate::notify::alert::{Alert, AlertKind, Level};

/// How long the text of a push can be fetched.
pub const TTL_HOURS: i64 = 24;

/// Longest title and body kept (characters).
const TITLE_MAX: usize = 300;
const BODY_MAX: usize = 4_000;

/// What the app shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub title: String,
    pub body: String,
    /// `problem`, `recovered`, `reminder`, `escalation`, `expiring`,
    /// `unstable`, `maintenance_started`, `maintenance_ended` or `test`.
    pub kind: String,
    /// `critical`, `warning`, `ok` or `info`.
    pub level: String,
    /// RFC 3339, UTC.
    pub occurred_at: String,
}

fn kind_name(kind: AlertKind) -> &'static str {
    match kind {
        AlertKind::Problem => "problem",
        AlertKind::Recovered => "recovered",
        AlertKind::Reminder => "reminder",
        AlertKind::Escalation => "escalation",
        AlertKind::Expiring => "expiring",
        AlertKind::Unstable => "unstable",
        AlertKind::MaintenanceStarted => "maintenance_started",
        AlertKind::MaintenanceEnded => "maintenance_ended",
        AlertKind::Test => "test",
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Critical => "critical",
        Level::Warning => "warning",
        Level::Ok => "ok",
        Level::Info => "info",
    }
}

fn clip(text: String, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => text[..i].to_string(),
        None => text,
    }
}

impl Event {
    /// The push text of `alert`: the same headline and body the other push
    /// providers show. The monitor target has already lost its credentials
    /// (`notify::send` strips them first).
    pub fn from_alert(alert: &Alert) -> Event {
        let occurred = alert
            .occurred_at
            .to_offset(time::UtcOffset::UTC)
            .format(&Rfc3339)
            .unwrap_or_default();
        Event {
            title: clip(crate::notify::message::title(alert), TITLE_MAX),
            body: clip(crate::notify::message::body_text(alert), BODY_MAX),
            kind: kind_name(alert.kind).into(),
            level: level_name(alert.level).into(),
            occurred_at: occurred,
        }
    }
}

/// A new random `ref`: 16 random bytes as unpadded base64url (22 characters).
pub fn new_ref() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Whether `reference` can be a `ref` this module made (so a lookup of
/// anything else is a plain miss).
pub fn valid_ref(reference: &str) -> bool {
    (1..=64).contains(&reference.len())
        && reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Stores `event` under `reference` and drops what has expired.
pub fn insert(conn: &Connection, reference: &str, event: &Event) -> rusqlite::Result<()> {
    purge(conn)?;
    conn.execute(
        "INSERT INTO push_events (ref, title, body, kind, level, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            reference,
            event.title,
            event.body,
            event.kind,
            event.level,
            event.occurred_at
        ],
    )?;
    Ok(())
}

/// The event under `reference`, unless it has expired.
pub fn get(conn: &Connection, reference: &str) -> rusqlite::Result<Option<Event>> {
    conn.query_row(
        "SELECT title, body, kind, level, occurred_at FROM push_events
         WHERE ref = ?1 AND created_at > datetime('now', '-' || ?2 || ' hours')",
        params![reference, TTL_HOURS],
        |r| {
            Ok(Event {
                title: r.get(0)?,
                body: r.get(1)?,
                kind: r.get(2)?,
                level: r.get(3)?,
                occurred_at: r.get(4)?,
            })
        },
    )
    .optional()
}

/// Deletes the events older than [`TTL_HOURS`]; how many there were.
pub fn purge(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM push_events WHERE created_at <= datetime('now', '-' || ?1 || ' hours')",
        params![TTL_HOURS],
    )
}
