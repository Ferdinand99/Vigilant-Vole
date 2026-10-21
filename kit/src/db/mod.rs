//! Database plumbing shared by every product: the connection pool, the
//! `interact` helper, key/value settings, users and API tokens, and the
//! migration runner ([`migrate`]).

use std::path::Path;

use deadpool_sqlite::{
    Config as PoolConfig, Hook, HookError, InteractError, Pool, PoolError, Runtime,
};
use rusqlite::{Connection, TransactionBehavior};

pub use migrate::{
    MigrationError, MigrationReport, MigrationSet, UpgradeError, apply_migrations, migrate,
    run_migrations,
};

pub mod api_tokens;
pub mod migrate;
pub mod role;
pub mod settings;
pub mod users;

/// Applied to every new pooled connection. `foreign_keys` is a per-connection
/// setting; rusqlite's bundled SQLite defaults it on, but set it explicitly so
/// ON DELETE CASCADE / SET NULL never silently depend on build flags.
///
/// `journal_size_limit`: after a checkpoint the WAL file is cut back to 64 MiB.
/// Without it the WAL keeps the size of its biggest burst for good; the first
/// retention run after lowering the retention (millions of deletes) left a
/// 650 MB WAL next to a 660 MB database in the soak test.
pub const CONNECTION_PRAGMAS: &str = "PRAGMA foreign_keys = ON;
     PRAGMA busy_timeout = 5000;
     PRAGMA synchronous = NORMAL;
     PRAGMA journal_size_limit = 67108864;";

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database pool error: {0}")]
    Pool(#[from] PoolError),
    #[error("database task failed: {0}")]
    Interact(String),
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
}

impl From<InteractError> for DbError {
    fn from(err: InteractError) -> Self {
        DbError::Interact(err.to_string())
    }
}

/// Connections in the check engine's own pool ([`build_engine_pool`]).
const ENGINE_POOL_SIZE: usize = 4;

pub fn build_pool(db_path: &Path) -> Pool {
    pool_with(db_path, None)
}

/// A separate pool for the check engine (heartbeats, alert state,
/// notifications). With one shared pool, a busy web UI (dashboards, status
/// pages, API clients) queues every heartbeat write behind its own reads and
/// checks fall minutes behind; with its own connections the engine only
/// competes for CPU and the write lock.
pub fn build_engine_pool(db_path: &Path) -> Pool {
    pool_with(db_path, Some(ENGINE_POOL_SIZE))
}

fn pool_with(db_path: &Path, max_size: Option<usize>) -> Pool {
    let builder = PoolConfig::new(db_path)
        .builder(Runtime::Tokio1)
        .expect("invalid sqlite pool config");
    let builder = match max_size {
        Some(n) => builder.max_size(n),
        None => builder,
    };
    builder
        .post_create(Hook::async_fn(|conn, _| {
            Box::pin(async move {
                conn.interact(|c| {
                    // Every transaction here writes. Taking the write lock at
                    // BEGIN makes a busy writer wait (busy_timeout) instead of
                    // failing with "database is locked" when another
                    // connection committed between its first read and write.
                    c.set_transaction_behavior(TransactionBehavior::Immediate);
                    c.execute_batch(CONNECTION_PRAGMAS)
                })
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

/// Open an in-memory database with connection pragmas and all of the
/// product's migrations applied. For tests in any module.
#[cfg(test)]
pub fn test_conn() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch(CONNECTION_PRAGMAS).expect("pragmas");
    apply_migrations(&mut conn).expect("migrations");
    conn
}
