//! The alert model every notification provider works with: the channel a
//! message goes to ([`NotificationChannel`]) and one finished alert
//! ([`Alert`]): what happened, how bad it is, and the texts and facts to
//! show, already written in the reader's language by the product.

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

/// What kind of thing happened. Providers decide with it whether to open,
/// resolve or skip an incident, which priority and which icon to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertKind {
    /// Something broke.
    Problem,
    /// It works again.
    Recovered,
    /// It is still broken (a repeat).
    Reminder,
    /// It has been broken for too long.
    Escalation,
    /// Something will expire (a certificate, a domain).
    Expiring,
    /// It keeps changing state.
    Unstable,
    /// Planned work began.
    MaintenanceStarted,
    /// Planned work ended.
    MaintenanceEnded,
    /// A test message.
    Test,
}

/// How serious an alert is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Critical,
    Warning,
    Ok,
    Info,
}

impl AlertKind {
    /// The default level of this kind of alert.
    pub fn level(self) -> Level {
        match self {
            AlertKind::Problem | AlertKind::Reminder | AlertKind::Escalation => Level::Critical,
            AlertKind::Expiring | AlertKind::Unstable => Level::Warning,
            AlertKind::Recovered => Level::Ok,
            AlertKind::MaintenanceStarted | AlertKind::MaintenanceEnded | AlertKind::Test => {
                Level::Info
            }
        }
    }

    /// The default emoji of this kind of alert.
    pub fn emoji(self) -> &'static str {
        match self {
            AlertKind::Problem | AlertKind::Reminder => "\u{1f534}",
            AlertKind::Escalation => "\u{1f6a8}",
            AlertKind::Recovered => "\u{1f7e2}",
            AlertKind::Expiring => "\u{1f7e1}",
            AlertKind::Unstable => "\u{1f7e0}",
            AlertKind::MaintenanceStarted | AlertKind::MaintenanceEnded => "\u{1f535}",
            AlertKind::Test => "\u{1f514}",
        }
    }
}

impl Level {
    /// RGB colour (Apple system colours) for embeds and cards.
    pub fn colour(self) -> u32 {
        match self {
            Level::Critical => 0xFF3B30,
            Level::Warning => 0xFF9500,
            Level::Ok => 0x34C759,
            Level::Info => 0x007AFF,
        }
    }
}

/// One alert, ready to send: every text is final (translated, with the
/// product's wording), so providers only lay it out.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub kind: AlertKind,
    /// Machine name of the event ("down", "new_issue"): webhook `event`,
    /// card ids.
    pub event: String,
    /// The event's name for people ("Down").
    pub event_label: String,
    /// The coarse status a receiver can switch on ("down", "up", …).
    pub status: String,
    pub level: Level,
    /// RGB colour for embeds and cards.
    pub colour: u32,
    pub emoji: String,
    /// "Payments API is down": no emoji, no detail.
    pub title: String,
    /// The part after the dash: the check message, how long it was down, …
    pub detail: Option<String>,
    /// Facts worth showing as fields: `(label, value)`.
    pub facts: Vec<(String, String)>,
    /// Text of the button or link that opens [`Self::url`] ("View monitor").
    pub link_label: String,
    /// Id of the thing the alert is about (dedup keys, webhooks).
    pub subject_id: Option<i64>,
    /// Name of the thing the alert is about.
    pub subject: String,
    /// Machine name of its type ("http"); empty when it has none.
    pub subject_type: String,
    /// What is being watched (a URL, `host:port`), safe to send.
    pub target: String,
    /// The raw message of the check or event, if any.
    pub message: Option<String>,
    /// How long the problem lasted (on recovery) or has lasted.
    pub duration: Option<Duration>,
    /// Days until something expires.
    pub expires_in_days: Option<i64>,
    /// Deep link into the product, when it knows its own address.
    pub url: Option<String>,
    pub occurred_at: OffsetDateTime,
}
