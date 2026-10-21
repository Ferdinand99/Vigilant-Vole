//! Database layer. The plumbing (pool, migration runner, key/value
//! settings, users, API tokens) is [`crate::kit::db`]; this module adds the
//! Vigilant Vole tables and tells the runner what its migrations are.

use rusqlite::Connection;
use rust_embed::Embed;

pub mod agents;
pub mod events;
pub mod heartbeats;
pub mod incidents;
pub mod maintenance;
pub mod models;
pub mod monitors;
pub mod notifications;
pub mod remote_browsers;
pub mod rollups;
pub mod settings;
pub mod status_pages;
pub mod subscribers;
pub mod tags;

pub use crate::kit::db::migrate::{
    apply_one, backup_before_migration, embedded_migrations, prepare_tracking,
};
#[cfg(test)]
pub use crate::kit::db::test_conn;
pub use crate::kit::db::{
    CONNECTION_PRAGMAS, DbError, MigrationError, MigrationReport, MigrationSet, UpgradeError,
    api_tokens, apply_migrations, build_engine_pool, build_pool, interact, migrate, run_migrations,
    users,
};

#[derive(Embed)]
#[folder = "migrations/"]
struct Migrations;

/// Every embedded migration, sorted by file name.
fn migration_files() -> Vec<(String, String)> {
    let mut names: Vec<_> = Migrations::iter().filter(|n| n.ends_with(".sql")).collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let file = Migrations::get(&name).expect("embedded migration missing");
            let sql =
                String::from_utf8(file.data.into_owned()).expect("migration is not valid utf8");
            (name.into_owned(), sql)
        })
        .collect()
}

/// Data fix-ups SQL can't do (hashing); each does nothing once done.
fn data_fixups(conn: &mut Connection) -> Result<(), MigrationError> {
    let hashed = subscribers::hash_legacy_tokens(conn)?;
    if hashed > 0 {
        tracing::info!(count = hashed, "hashed status page subscriber tokens");
    }
    Ok(())
}

/// How the shared runner applies this product's migrations.
pub static MIGRATIONS: MigrationSet = MigrationSet {
    migrations: migration_files,
    // SQLite ignores `PRAGMA foreign_keys` inside a transaction, so a
    // migration that rebuilds a table says so with this marker.
    fk_off_marker: "-- vv:foreign_keys=off",
    // The first migration predates the tracking table; databases created by
    // it are detected by the presence of its tables.
    baseline: Some(crate::kit::db::migrate::Baseline {
        name: "0001_init.sql",
        tables: &["users", "monitors"],
    }),
    after: data_fixups,
};

#[cfg(test)]
mod tests;
