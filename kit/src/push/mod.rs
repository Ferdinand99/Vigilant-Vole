//! Push from the servers: a product's alerts as push notifications in the
//! Hedgerow app, through the Hedgerow push relay ("Postal Pigeon").
//!
//! Push is a notification channel of kind `hedgerow`
//! ([`crate::notify::hedgerow`]), not a system of its own: it fires for the
//! same monitors, rules and events as every other channel. Nothing is sent
//! unless an administrator has added such a channel.
//!
//! * [`devices`]: the app installs that registered a handle with this server
//!   (`push_devices`, handles sealed at rest).
//! * [`events`]: the text of each push, kept for 24 hours under a random
//!   `ref` (`push_events`). The relay and Apple only ever see the `ref`; the
//!   app fetches the text from this server with its own token.
//! * [`relay`]: the relay client: registration with proof of work, sending
//!   in batches of 25 and cleaning up after `gone` handles.
//! * [`api`]: the three routes the app calls.
//!
//! A product calls [`install`] once at start-up, after it has installed its
//! [`KitConfig`](crate::config::KitConfig) and built its
//! [`Core`](crate::state::Core), and mounts [`api::router`]. See
//! `docs/vole-kit.md`.

pub mod api;
pub mod devices;
pub mod events;
pub mod relay;

#[cfg(test)]
mod tests;

pub use relay::{Context, install};
