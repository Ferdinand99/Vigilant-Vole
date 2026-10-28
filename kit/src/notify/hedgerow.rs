//! The Hedgerow app — push notifications to the Hedgerow app on the phones
//! that registered with this server, through the Hedgerow push relay.
//!
//! The only provider with its own delivery ([`Transport::Relay`]): it needs
//! the database (devices, relay credential), so the work is done by
//! [`push::relay`](crate::push::relay) with the context a product registers
//! at start-up ([`push::install`](crate::push::install)). The channel has no
//! settings: whom it reaches is the app installs that registered.

use super::schema::{Category, ProviderSchema, Transport};
use crate::notify::alert::Alert;

pub const SCHEMA: ProviderSchema = ProviderSchema {
    kind: "hedgerow",
    label: "Hedgerow app",
    icon: "bell",
    category: Category::Push,
    description: "Push notifications in the Hedgerow app",
    docs_url: "https://ferdinand99.github.io/",
    fields: &[],
    transport: Transport::Relay,
    check: None,
};

/// Delivers `event` to every registered device. See
/// [`push::relay::deliver`](crate::push::relay::deliver) for the outcome.
///
/// An alert is pushed once even when the product attaches two `hedgerow`
/// channels to it ([`push::relay::deliver_once`](crate::push::relay::deliver_once)).
/// The kit can't refuse the second channel when it is saved, since a channel
/// is only a row in the product's table and the provider's check sees one
/// config at a time.
pub async fn send(event: &Alert) -> Result<(), String> {
    match crate::push::relay::current() {
        Some(ctx) => crate::push::relay::deliver_once(&ctx, event).await,
        None => Err(crate::i18n::t("notif.hedgerow.err_not_ready").to_string()),
    }
}
