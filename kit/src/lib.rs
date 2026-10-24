//! The shared product kit: everything that isn't specific to one product.
//!
//! `kit` is written to be moved into its own crate (`vole-kit`) without
//! changes, so it follows one rule: **nothing under `src/kit/` names
//! anything outside `crate`** (checked by `tests/kit_boundary.rs`).
//! The product (Vigilant Vole) depends on the kit, never the other way
//! round. What the kit needs to know about its product (names, cookie
//! names, token prefixes, translations, …) comes in through
//! [`config::KitConfig`]; see `docs/vole-kit.md`.

#![forbid(unsafe_code)]

pub mod auth;
pub mod components;
pub mod config;
pub mod crypto;
pub mod db;
pub mod docs;
pub mod http;
pub mod i18n;
pub mod mcp;
pub mod notify;
pub mod page;
pub mod settings;
pub mod state;
pub mod tls_roots;
pub mod tz;

#[cfg(test)]
mod testing;
