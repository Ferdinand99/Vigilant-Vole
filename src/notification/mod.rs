//! Notification providers. The providers, their schemas and the delivery
//! code are [`crate::kit::notify`] and work on the kit's generic
//! [`Alert`](crate::kit::notify::alert::Alert). This module is Vigilant
//! Vole's face of it: the same names, taking Vigilant Vole's
//! [`NotificationEvent`], which [`message::alert`] turns into an alert in
//! the reader's language.

pub mod email;
pub mod message;
#[cfg(test)]
pub(crate) mod tests_support;

use std::ops::Deref;

use futures_util::future::join_all;
use serde_json::Value;

use crate::db::models::{NotificationChannel, NotificationEvent};
use crate::kit::notify as kit;
use crate::kit::notify::http::OutRequest;
pub use crate::kit::notify::*;
pub use message::{summary, test_event};

/// A provider as Vigilant Vole uses it: the kit's schema (fields, labels,
/// checks; reached through `Deref`) with a transport that takes a
/// [`NotificationEvent`].
pub struct ProviderSchema {
    schema: &'static kit::ProviderSchema,
    pub transport: Transport,
}

impl Deref for ProviderSchema {
    type Target = kit::ProviderSchema;

    fn deref(&self) -> &kit::ProviderSchema {
        self.schema
    }
}

/// How a provider delivers, for a [`NotificationEvent`].
#[derive(Clone, Copy)]
pub enum Transport {
    /// Builds the HTTP requests for one event.
    Http(fn(&Value, &NotificationEvent) -> Result<Vec<OutRequest>, String>),
    Smtp,
}

/// The kit's provider number `I`, building from a [`NotificationEvent`].
fn build_via<const I: usize>(
    config: &Value,
    event: &NotificationEvent,
) -> Result<Vec<OutRequest>, String> {
    match kit::PROVIDERS[I].transport {
        kit::Transport::Http(build) => build(config, &message::alert(event)),
        kit::Transport::Smtp => Err("this provider sends email over SMTP".into()),
    }
}

const fn transport<const I: usize>() -> Transport {
    match kit::PROVIDERS[I].transport {
        kit::Transport::Http(_) => Transport::Http(build_via::<I>),
        kit::Transport::Smtp => Transport::Smtp,
    }
}

macro_rules! providers {
    ($($i:literal)*) => {
        &[$(&ProviderSchema { schema: kit::PROVIDERS[$i], transport: transport::<$i>() }),*]
    };
}

/// Every provider, in the kit's order.
pub const PROVIDERS: &[&ProviderSchema] = providers!(
    0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19
    20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39
);
const _: () = assert!(
    PROVIDERS.len() == kit::PROVIDERS.len(),
    "list every kit provider"
);

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
    // jobs, the site default (Settings → General → Language). The alert's
    // texts are written in that language, so it is chosen first.
    let lang = crate::i18n::current_or_site();
    // Monitor targets can carry credentials (`https://user:pass@…`,
    // `?token=`); they never leave in a notification, not even as a fact.
    let event = NotificationEvent {
        target: message::public_target(&event.target),
        ..event.clone()
    };
    kit::send_in(http, kind, config, lang, || message::alert(&event)).await
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
