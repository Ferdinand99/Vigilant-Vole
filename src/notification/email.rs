//! Email over SMTP: the kit's [`crate::kit::notify::email`], taking a
//! [`NotificationEvent`].

use lettre::Message;
use serde_json::Value;

use super::message::alert;
use crate::db::models::NotificationEvent;
pub use crate::kit::notify::email::*;

/// The message (pure, testable).
pub fn build_message(config: &Value, event: &NotificationEvent) -> Result<Message, String> {
    crate::kit::notify::email::build_message(config, &alert(event))
}

/// The HTML part of the message.
pub fn html_body(event: &NotificationEvent) -> String {
    crate::kit::notify::email::html_body(&alert(event))
}

pub async fn send(config: &Value, event: &NotificationEvent) -> Result<(), String> {
    crate::kit::notify::email::send(config, &alert(event)).await
}
