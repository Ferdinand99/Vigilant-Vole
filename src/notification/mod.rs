//! Notification providers. Owned by the notifications agent.
//!
//! Contract: [`send`] delivers one event to one channel; [`send_all`] fans out
//! to several channels concurrently and logs (never propagates) failures.
//!
//! Every provider is a module with a `SCHEMA: ProviderSchema` (form fields,
//! labels, secret flags — rendered generically by the settings UI) and a pure
//! `build(&config, &event) -> Vec<OutRequest>` (or, for email, its own
//! `send`). [`PROVIDERS`] is the registry; [`SUPPORTED_KINDS`] derives from it.
//!
//! Errors returned by [`send`] are safe to log and show: they never contain
//! the request URL or any secret from the channel config.

pub mod apprise;
pub mod bark;
pub mod dingtalk;
pub mod discord;
pub mod email;
pub mod feishu;
pub mod googlechat;
pub mod gotify;
pub mod homeassistant;
pub mod http;
pub mod matrix;
pub mod mattermost;
pub mod message;
pub mod nextcloudtalk;
pub mod ntfy;
pub mod opsgenie;
pub mod pagerduty;
pub mod pushbullet;
pub mod pushover;
pub mod rocketchat;
pub mod schema;
pub mod signal;
pub mod slack;
pub mod teams;
pub mod telegram;
pub mod twilio;
pub mod webex;
pub mod threema;
pub mod webhook;
pub mod whatsapp;
pub mod zulip;

#[cfg(test)]
pub(crate) mod tests;

use futures_util::future::join_all;
use serde_json::Value;

pub use message::{format_duration, summary, test_event};
pub use schema::{Category, Field, FieldKind, ProviderSchema, SECRET_MASK, Transport};

use crate::db::models::{NotificationChannel, NotificationEvent};

/// Every provider, in picker order within each category.
pub const PROVIDERS: &[&ProviderSchema] = &[
    &discord::SCHEMA,
    &slack::SCHEMA,
    &teams::SCHEMA,
    &googlechat::SCHEMA,
    &mattermost::SCHEMA,
    &rocketchat::SCHEMA,
    &matrix::SCHEMA,
    &webex::SCHEMA,
    &feishu::SCHEMA,
    &dingtalk::SCHEMA,
    &telegram::SCHEMA,
    &signal::SCHEMA,
    &twilio::SCHEMA,
    &ntfy::SCHEMA,
    &pushover::SCHEMA,
    &gotify::SCHEMA,
    &pushbullet::SCHEMA,
    &bark::SCHEMA,
    &email::SCHEMA,
    &pagerduty::SCHEMA,
    &opsgenie::SCHEMA,
    &webhook::SCHEMA,
    &apprise::SCHEMA,
    &homeassistant::SCHEMA,
    &zulip::SCHEMA,
    &nextcloudtalk::SCHEMA,
    &threema::SCHEMA,
    &whatsapp::WAHA,
    &whatsapp::EVOLUTION,
];

/// Channel kinds this build can deliver to (derived from [`PROVIDERS`]).
pub const SUPPORTED_KINDS: [&str; PROVIDERS.len()] = {
    let mut kinds = [""; PROVIDERS.len()];
    let mut i = 0;
    while i < PROVIDERS.len() {
        kinds[i] = PROVIDERS[i].kind;
        i += 1;
    }
    kinds
};

pub fn is_supported_kind(kind: &str) -> bool {
    provider(kind).is_some()
}

pub fn provider(kind: &str) -> Option<&'static ProviderSchema> {
    PROVIDERS.iter().copied().find(|p| p.kind == kind)
}

/// The channel's config with secrets replaced by [`SECRET_MASK`] — the only
/// form of a config that may leave the server.
pub fn redacted_config(channel: &NotificationChannel) -> Value {
    let config = serde_json::from_str(&channel.config).unwrap_or(Value::Null);
    schema::redact(provider(&channel.kind), &config)
}

/// Deliver one event to one channel.
pub async fn send(
    http: &reqwest::Client,
    channel: &NotificationChannel,
    event: &NotificationEvent,
) -> Result<(), String> {
    let config: Value = serde_json::from_str(&channel.config)
        .map_err(|e| format!("invalid channel config: {e}"))?;
    send_config(http, &channel.kind, &config, event).await
}

/// Deliver one event using a kind + config that may not be saved yet
/// ("Send test" from the add form).
pub async fn send_config(
    http: &reqwest::Client,
    kind: &str,
    config: &Value,
    event: &NotificationEvent,
) -> Result<(), String> {
    // Messages use the request's language ("Send test") or, from background
    // jobs, the site default (Settings → General → Language).
    let lang = crate::i18n::current_or_site();
    crate::i18n::scope(lang, send_config_in_scope(http, kind, config, event)).await
}

async fn send_config_in_scope(
    http: &reqwest::Client,
    kind: &str,
    config: &Value,
    event: &NotificationEvent,
) -> Result<(), String> {
    let schema = provider(kind).ok_or_else(|| format!("unsupported channel kind '{kind}'"))?;
    // Monitor targets can carry credentials (`https://user:pass@…`, `?token=`);
    // they never leave in a notification.
    let event = &NotificationEvent {
        target: message::public_target(&event.target),
        ..event.clone()
    };
    let secrets = schema::secret_values(Some(schema), config);
    let result = match schema.transport {
        Transport::Http(build) => {
            let requests = build(config, event).map_err(|e| http::scrub(&e, &secrets))?;
            http::execute_all(http, &requests, &secrets).await
        }
        Transport::Smtp => email::send(config, event).await,
    };
    result.map_err(|e| http::scrub(&e, &secrets))
}

/// Deliver one event to every channel concurrently; failures are logged.
pub async fn send_all(
    http: &reqwest::Client,
    channels: &[NotificationChannel],
    event: &NotificationEvent,
) {
    let results = join_all(channels.iter().map(|c| send(http, c, event))).await;
    for (channel, result) in channels.iter().zip(results) {
        if let Err(err) = result {
            tracing::warn!(channel = %channel.name, kind = %channel.kind, %err, "failed to send notification");
        }
    }
}
