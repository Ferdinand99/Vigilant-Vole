//! The alert model every notification provider works with: the channel a
//! message goes to, what happened (`EventKind`) and the facts about it
//! (`NotificationEvent`). The `monitor_*` fields name the thing being
//! watched: the kit family's products all watch something.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A row of `notification_channels`. `kind` is free text in the schema; the
/// notification module validates it against its provider list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationChannel {
    pub id: i64,
    pub name: String,
    pub kind: String,
    /// JSON object text with provider-specific settings.
    pub config: String,
    pub active: bool,
    /// Auto-attach to newly created monitors.
    pub is_default: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Down,
    Up,
    Reminder,
    Escalation,
    CertExpiring,
    DomainExpiring,
    MaintenanceStart,
    MaintenanceEnd,
    Flapping,
    Test,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Down => "down",
            EventKind::Up => "up",
            EventKind::Reminder => "reminder",
            EventKind::Escalation => "escalation",
            EventKind::CertExpiring => "cert_expiring",
            EventKind::DomainExpiring => "domain_expiring",
            EventKind::MaintenanceStart => "maintenance_start",
            EventKind::MaintenanceEnd => "maintenance_end",
            EventKind::Flapping => "flapping",
            EventKind::Test => "test",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EventKind::Down => crate::i18n::t("event_kind.down"),
            EventKind::Up => crate::i18n::t("event_kind.up"),
            EventKind::Reminder => crate::i18n::t("event_kind.reminder"),
            EventKind::Escalation => crate::i18n::t("event_kind.escalation"),
            EventKind::CertExpiring => crate::i18n::t("event_kind.cert_expiring"),
            EventKind::DomainExpiring => crate::i18n::t("event_kind.domain_expiring"),
            EventKind::MaintenanceStart => crate::i18n::t("event_kind.maintenance_start"),
            EventKind::MaintenanceEnd => crate::i18n::t("event_kind.maintenance_end"),
            EventKind::Flapping => crate::i18n::t("event_kind.flapping"),
            EventKind::Test => crate::i18n::t("event_kind.test"),
        }
    }

    /// Whether this event signals a problem (used for colour/severity).
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            EventKind::Down
                | EventKind::Reminder
                | EventKind::Escalation
                | EventKind::CertExpiring
                | EventKind::DomainExpiring
                | EventKind::Flapping
        )
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything a notification provider needs to render one alert.
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationEvent {
    pub kind: EventKind,
    pub monitor_id: Option<i64>,
    pub monitor_name: String,
    /// `MonitorType::as_str()` of the monitor (empty for non-monitor events).
    pub monitor_type: String,
    pub target: String,
    pub message: Option<String>,
    /// On `Up`: how long the monitor was down.
    pub downtime: Option<Duration>,
    /// On `CertExpiring` / `DomainExpiring`.
    pub expires_in_days: Option<i64>,
    /// Deep link (`base_url + /monitors/{id}`), when a base URL is configured.
    pub url: Option<String>,
    pub occurred_at: OffsetDateTime,
}
