//! Notification providers.
//!
//! Contract: [`send`] delivers one [`Alert`](alert::Alert) to one channel;
//! [`send_all`] fans out to several channels concurrently and logs (never
//! propagates) failures. An alert arrives finished: the product has written
//! its title, detail and facts in the reader's language.
//!
//! Every provider is a module with a `SCHEMA: ProviderSchema` (form fields,
//! labels, secret flags — rendered generically by the settings UI) and a pure
//! `build(&config, &alert) -> Vec<OutRequest>` (or, for email and the
//! Hedgerow app, its own `send`). [`PROVIDERS`] is the registry; [`SUPPORTED_KINDS`] derives from it.
//!
//! Errors returned by [`send`] are safe to log and show: they never contain
//! the request URL or any secret from the channel config.
//!
//! Stored configs have their secrets sealed ([`sealed`]); open them
//! ([`sealed::open_channel`], [`sealed::open_config`]) before sending. A
//! config with a secret that is still sealed is refused, never sent.

pub mod alert;
pub mod apis;
pub mod apprise;
pub mod bark;
pub mod dingtalk;
pub mod discord;
pub mod email;
pub mod feishu;
pub mod form;
pub mod googlechat;
pub mod gotify;
pub mod hedgerow;
pub mod homeassistant;
pub mod http;
pub mod matrix;
pub mod mattermost;
pub mod message;
pub mod nextcloudtalk;
pub mod ntfy;
pub mod oncall;
pub mod opsgenie;
pub mod pagerduty;
pub mod pushbullet;
pub mod pushover;
pub mod rocketchat;
pub mod schema;
pub mod sealed;
pub mod signal;
pub mod slack;
pub mod teams;
pub mod telegram;
pub mod threema;
pub mod twilio;
pub mod webex;
pub mod webhook;
pub mod whatsapp;
pub mod zulip;

#[cfg(test)]
pub(crate) mod tests;

use futures_util::future::join_all;
use serde_json::Value;

pub use message::{format_duration, summary};
pub use schema::{Category, Field, FieldKind, ProviderSchema, SECRET_MASK, Transport};

use crate::notify::alert::{Alert, NotificationChannel};

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
    &oncall::SIGNL4,
    &oncall::SQUADCAST,
    &oncall::GRAFANA_ONCALL,
    &oncall::GOALERT,
    &oncall::SPLUNK,
    &oncall::KEEP,
    &apis::SENDGRID,
    &apis::BREVO,
    &apis::CLICKSEND,
    &apis::ELKS,
    &apis::GORUSH,
    &hedgerow::SCHEMA,
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
    event: &Alert,
) -> Result<(), String> {
    let config: Value = serde_json::from_str(&channel.config)
        .map_err(|e| format!("invalid channel config: {e}"))?;
    send_config(http, &channel.kind, &config, event).await
}

/// Deliver one event using a kind + config that may not be saved yet
/// ("Send test" from the add form), in the request's language or, from
/// background jobs, the site's ([`send_config_in`] for another).
pub async fn send_config(
    http: &reqwest::Client,
    kind: &str,
    config: &Value,
    event: &Alert,
) -> Result<(), String> {
    // Messages use the request's language ("Send test") or, from background
    // jobs, the site default (Settings → General → Language).
    send_config_in(http, kind, config, event, crate::i18n::current_or_site()).await
}

/// [`send_config`] in `lang`: the language of the provider's own words
/// (the email's footer, field labels), for a channel with a language of
/// its own. The alert's texts are already written; build them in `lang`
/// too (or use [`send_in`]).
pub async fn send_config_in(
    http: &reqwest::Client,
    kind: &str,
    config: &Value,
    event: &Alert,
    lang: crate::i18n::Lang,
) -> Result<(), String> {
    crate::i18n::scope(lang, send_config_in_scope(http, kind, config, event)).await
}

/// Builds the alert with `alert` in `lang` and sends it to `kind` with
/// `config` (which may not be saved yet): the alert's texts and the
/// provider's own words are then in the same language, the channel's. The
/// error is safe to show (no URL, no secret).
///
/// "Send test" with the kit's test message ([`Alert::test`]):
///
/// ```ignore
/// let lang = channel_language.unwrap_or_else(i18n::site_default);
/// notify::send_in(&core.http, kind, &config, lang, || {
///     // The product's own texts, looked up in `lang`.
///     Alert::test(title(), body(), event_label(), link_label(), url)
/// })
/// .await
/// ```
pub async fn send_in(
    http: &reqwest::Client,
    kind: &str,
    config: &Value,
    lang: crate::i18n::Lang,
    alert: impl FnOnce() -> Alert,
) -> Result<(), String> {
    crate::i18n::scope(lang, async {
        let alert = alert();
        send_config_in_scope(http, kind, config, &alert).await
    })
    .await
}

async fn send_config_in_scope(
    http: &reqwest::Client,
    kind: &str,
    config: &Value,
    event: &Alert,
) -> Result<(), String> {
    let schema = provider(kind).ok_or_else(|| format!("unsupported channel kind '{kind}'"))?;
    // A sealed secret here wasn't opened, or couldn't be (another instance
    // key): never send the sealed text to the provider.
    if sealed::has_sealed(schema, config) {
        return Err(sealed::unreadable_message().to_string());
    }
    // Monitor targets can carry credentials (`https://user:pass@…`, `?token=`);
    // they never leave in a notification.
    let event = &Alert {
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
        Transport::Relay => hedgerow::send(event).await,
    };
    result.map_err(|e| http::scrub(&e, &secrets))
}

/// Deliver one event to every channel concurrently; failures are logged.
pub async fn send_all(http: &reqwest::Client, channels: &[NotificationChannel], event: &Alert) {
    let results = join_all(channels.iter().map(|c| send(http, c, event))).await;
    for (channel, result) in channels.iter().zip(results) {
        if let Err(err) = result {
            tracing::warn!(channel = %channel.name, kind = %channel.kind, %err, "failed to send notification");
        }
    }
}
