//! Vigilant Vole: self-hosted uptime monitoring.
//!
//! The binary (`main.rs`) is a thin CLI over [`server::run`] and [`agent::run`].

pub mod agent;
pub mod api;
pub mod auth;
pub mod config;
pub mod config_io;
pub mod db;
pub mod discovery;
pub mod i18n;
pub use vole_kit as kit;
pub mod kuma_import;
pub mod maintenance;
pub mod mcp;
pub mod mcp_bridge;
pub mod monitor;
pub mod notification;
pub mod product;
pub mod server;
pub mod subscribers;
pub mod tls_roots {
    //! Moved to [`crate::kit::tls_roots`].
    pub use crate::kit::tls_roots::*;
}

/// Unit tests have no `main`: install the kit's configuration before any of
/// them runs, so no test can forget it.
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn install_kit_config_for_tests() {
    product::install();
}
