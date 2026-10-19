//! Database plumbing: connection pool, tracked migrations, and a small
//! `interact` helper so callers don't repeat the pool/interact/error dance.

use std::path::Path;

use deadpool_sqlite::{Config as PoolConfig, Hook, HookError, InteractError, Pool, PoolError, Runtime};
use rusqlite::{Connection, OptionalExtension, params};
use rust_embed::Embed;

pub mod agents;
pub mod api_tokens;
pub mod events;
pub mod heartbeats;
pub mod incidents;
pub mod maintenance;
pub mod models;
pub mod monitors;
pub mod notifications;
pub mod rollups;
pub mod settings;
pub mod status_pages;
pub mod tags;
pub mod users;

#[derive(Embed)]
#[folder = "migrations/"]
struct Migrations;

/// Applied to every new pooled connection. `foreign_keys` is a per-connection
/// setting; rusqlite's bundled SQLite defaults it on, but set it explicitly so
/// ON DELETE CASCADE / SET NULL never silently depend on build flags.
const CONNECTION_PRAGMAS: &str = "PRAGMA foreign_keys = ON;
     PRAGMA busy_timeout = 5000;
     PRAGMA synchronous = NORMAL;";

/// Marker a migration file may contain (anywhere, conventionally line 1) to run
/// with foreign key enforcement disabled — required for the SQLite
/// "create new table, copy, drop old, rename" rebuild procedure. SQLite ignores
/// `PRAGMA foreign_keys` inside a transaction, so the runner toggles it around
/// the migration's transaction and runs `PRAGMA foreign_key_check` before commit.
const FK_OFF_MARKER: &str = "-- vv:foreign_keys=off";

/// The first migration predates the tracking table; databases created by it
/// are detected by the presence of its tables and marked as applied.
const BASELINE_MIGRATION: &str = "0001_init.sql";

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database pool error: {0}")]
    Pool(#[from] PoolError),
    #[error("database task failed: {0}")]
    Interact(String),
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("migration bookkeeping failed: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("migration {name} failed: {source}")]
    Failed {
        name: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error("migration {name} left foreign key violations: {violations}")]
    ForeignKeyViolations { name: String, violations: String },
    #[error("pre-migration backup failed: {0}")]
    Backup(String),
}

impl From<InteractError> for DbError {
    fn from(err: InteractError) -> Self {
        DbError::Interact(err.to_string())
    }
}

pub fn build_pool(db_path: &Path) -> Pool {
    PoolConfig::new(db_path)
        .builder(Runtime::Tokio1)
        .expect("invalid sqlite pool config")
        .post_create(Hook::async_fn(|conn, _| {
            Box::pin(async move {
                conn.interact(|c| c.execute_batch(CONNECTION_PRAGMAS))
                    .await
                    .map_err(|e| HookError::message(e.to_string()))?
                    .map_err(HookError::Backend)
            })
        }))
        .build()
        .expect("failed to create sqlite pool")
}

/// Run `f` on a pooled connection on the blocking thread pool.
///
/// ```ignore
/// let monitor = db::interact(&pool, move |c| db::monitors::get_monitor(c, id)).await?;
/// ```
pub async fn interact<T, F>(pool: &Pool, f: F) -> Result<T, DbError>
where
    F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let conn = pool.get().await?;
    Ok(conn.interact(f).await??)
}

/// Apply all pending embedded migrations. Panics on failure: the server must
/// not start against a half-migrated schema.
///
/// Before migrating a database that already holds a schema, a consistent copy
/// is written to `backup_dir` (`VACUUM INTO`); restoring that file is the
/// rollback path, since SQLite migrations here are forward-only.
pub async fn run_migrations(pool: &Pool, backup_dir: &Path) {
    let backup_dir = backup_dir.to_path_buf();
    let conn = pool.get().await.expect("failed to get db connection");
    conn.interact(move |conn| -> Result<(), MigrationError> {
        // WAL is persistent per database file; set it once here rather than per connection.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let existing = prepare_tracking(conn)?;
        let pending = pending_migrations(conn)?;
        if existing && !pending.is_empty() {
            let first = pending.first().map(|(name, _)| name.trim_end_matches(".sql")).unwrap_or("");
            backup_before_migration(conn, &backup_dir, first)?;
        }
        apply_migrations(conn)?;
        Ok(())
    })
    .await
    .expect("migration task panicked")
    .unwrap_or_else(|e| panic!("database migration failed: {e}"));
}

fn backup_before_migration(conn: &Connection, dir: &Path, label: &str) -> Result<(), MigrationError> {
    std::fs::create_dir_all(dir).map_err(|e| MigrationError::Backup(e.to_string()))?;
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
    let path = dir.join(format!("vigilant-vole-before-{label}-{stamp}.db"));
    let path_str = path
        .to_str()
        .ok_or_else(|| MigrationError::Backup(format!("non-UTF-8 backup path {}", path.display())))?;
    conn.execute("VACUUM INTO ?1", params![path_str])
        .map_err(|e| MigrationError::Backup(e.to_string()))?;
    tracing::info!(path = %path.display(), "backed up database before migrating");
    Ok(())
}

/// Every embedded migration, sorted by file name.
fn embedded_migrations() -> Vec<(String, String)> {
    let mut names: Vec<_> = Migrations::iter().filter(|n| n.ends_with(".sql")).collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let file = Migrations::get(&name).expect("embedded migration missing");
            let sql = String::from_utf8(file.data.into_owned()).expect("migration is not valid utf8");
            (name.into_owned(), sql)
        })
        .collect()
}

/// Synchronous migration runner (also used directly by tests).
///
/// Each migration runs exactly once, inside its own transaction, and is
/// recorded in `schema_migrations`. Returns the names applied in this call.
pub fn apply_migrations(conn: &mut Connection) -> Result<Vec<String>, MigrationError> {
    prepare_tracking(conn)?;
    let mut applied = Vec::new();
    for (name, sql) in pending_migrations(conn)? {
        apply_one(conn, &name, &sql)?;
        tracing::info!(migration = %name, "applied database migration");
        applied.push(name);
    }
    Ok(applied)
}

/// Ensures `schema_migrations` exists and records the legacy baseline.
/// Returns whether the database already had a schema (i.e. is not brand new).
fn prepare_tracking(conn: &Connection) -> rusqlite::Result<bool> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             name TEXT PRIMARY KEY,
             applied_at TEXT NOT NULL DEFAULT (datetime('now'))
         );",
    )?;
    mark_legacy_baseline(conn)?;
    let tracked: i64 = conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;
    Ok(tracked > 0)
}

/// Embedded migrations not yet recorded in `schema_migrations`, in order.
fn pending_migrations(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut pending = Vec::new();
    for (name, sql) in embedded_migrations() {
        let done: Option<i64> = conn
            .query_row("SELECT 1 FROM schema_migrations WHERE name = ?1", params![name], |r| r.get(0))
            .optional()?;
        if done.is_none() {
            pending.push((name, sql));
        }
    }
    Ok(pending)
}

/// Databases created before migration tracking existed already have the 0001
/// schema; record it so it isn't re-run (it is idempotent, but tracking should
/// reflect reality).
fn mark_legacy_baseline(conn: &Connection) -> rusqlite::Result<()> {
    let tracked: i64 = conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;
    if tracked > 0 {
        return Ok(());
    }
    let has_legacy_schema: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('users', 'monitors')",
        [],
        |r| r.get(0),
    )?;
    if has_legacy_schema > 0 {
        conn.execute(
            "INSERT INTO schema_migrations (name) VALUES (?1)",
            params![BASELINE_MIGRATION],
        )?;
        tracing::info!("existing database detected; marked {BASELINE_MIGRATION} as applied");
    }
    Ok(())
}

fn apply_one(conn: &mut Connection, name: &str, sql: &str) -> Result<(), MigrationError> {
    let fk_off = sql.contains(FK_OFF_MARKER);
    if fk_off {
        conn.pragma_update(None, "foreign_keys", "OFF")?;
    }

    let result = run_in_transaction(conn, name, sql, fk_off);

    if fk_off {
        // Restore enforcement even when the migration failed (the transaction
        // has been rolled back by then).
        conn.pragma_update(None, "foreign_keys", "ON")?;
    }
    result
}

fn run_in_transaction(conn: &mut Connection, name: &str, sql: &str, check_fks: bool) -> Result<(), MigrationError> {
    let failed = |source| MigrationError::Failed {
        name: name.to_string(),
        source,
    };
    let tx = conn.transaction()?;
    tx.execute_batch(sql).map_err(failed)?;
    if check_fks {
        let violations = foreign_key_violations(&tx)?;
        if !violations.is_empty() {
            return Err(MigrationError::ForeignKeyViolations {
                name: name.to_string(),
                violations: violations.join(", "),
            });
        }
    }
    tx.execute("INSERT INTO schema_migrations (name) VALUES (?1)", params![name])?;
    tx.commit()?;
    Ok(())
}

fn foreign_key_violations(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let rows = stmt.query_map([], |row| {
        let table: String = row.get(0)?;
        let rowid: Option<i64> = row.get(1)?;
        let parent: String = row.get(2)?;
        Ok(format!("{table} row {rowid:?} -> {parent}"))
    })?;
    rows.collect()
}

/// Open an in-memory database with connection pragmas and all migrations
/// applied. For tests in any module.
#[cfg(test)]
pub fn test_conn() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch(CONNECTION_PRAGMAS).expect("pragmas");
    apply_migrations(&mut conn).expect("migrations");
    conn
}

#[cfg(test)]
mod tests;
