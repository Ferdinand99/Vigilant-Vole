//! Test fixtures for notifications.

use time::macros::datetime;

use crate::db::models::{EventKind, NotificationEvent};

/// "Payments API" (http, id 7) at a fixed time.
pub fn event(kind: EventKind) -> NotificationEvent {
    NotificationEvent {
        kind,
        monitor_id: Some(7),
        monitor_name: "Payments API".into(),
        monitor_type: "http".into(),
        target: "https://pay.example/health".into(),
        message: None,
        downtime: None,
        expires_in_days: None,
        url: None,
        occurred_at: datetime!(2026-10-19 14:03:12 UTC),
    }
}
