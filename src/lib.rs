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
pub mod kuma_import;
pub mod maintenance;
pub mod monitor;
pub mod notification;
pub mod server;
