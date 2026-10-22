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

/// Closes `pool` and its idle connections, one after another, and returns
/// once they are closed. Use this instead of [`Pool::close`] on shutdown.
///
/// `Pool::close` drops the idle connections on blocking threads, all at once
/// and without waiting for them. SQLite checkpoints a WAL database and
/// removes its `-wal` and `-shm` files only when the closing connection is
/// the last one (it gets an exclusive lock); connections that close at the
/// same moment each see the other and all skip that, so the database stays
/// in WAL state although nothing has it open any more. Closed one at a time,
/// the last connection cleans up.
///
/// Call it once nothing uses the pool any more: a connection still checked
/// out is closed when it is returned, as with `Pool::close`.
pub async fn close(pool: &Pool) {
    // Take the idle connections out before closing: the pool would drop them.
    let idle = pool.retain(|_, _| false).removed;
    pool.close();
    for conn in idle {
        // The wrapper owns the connection; swap in an in-memory placeholder
        // (which it then drops) so the real one can be closed right here.
        let closed = conn
            .interact(|c| {
                let placeholder = Connection::open_in_memory()?;
                std::mem::replace(c, placeholder)
                    .close()
                    .map_err(|(_, err)| err)
            })
            .await;
        if let Err(err) = closed
            .map_err(DbError::from)
            .and_then(|r| r.map_err(DbError::from))
        {
            tracing::warn!(%err, "closing a database connection failed");
        }
    }
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A fresh directory for one WAL database.
    fn temp_db() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "vole-kit-close-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("t.db")
    }

    fn wal(db: &Path) -> PathBuf {
        PathBuf::from(format!("{}-wal", db.display()))
    }

    /// A WAL database with `n` idle pooled connections that have all written.
    async fn pool_with_connections(db: &Path, n: usize) -> Pool {
        let pool = build_pool(db);
        interact(&pool, |c| {
            c.pragma_update(None, "journal_mode", "WAL")?;
            c.execute_batch("CREATE TABLE t (x)")
        })
        .await
        .unwrap();
        let mut held = Vec::new();
        for _ in 0..n {
            held.push(pool.get().await.unwrap());
        }
        for conn in &held {
            conn.interact(|c| c.execute("INSERT INTO t VALUES (1)", []))
                .await
                .unwrap()
                .unwrap();
        }
        drop(held);
        assert_eq!(pool.status().size, n);
        pool
    }

    /// Once `close` returns, every connection is closed and the last one
    /// has checkpointed the database and removed its WAL files.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn close_closes_every_connection_and_removes_the_wal() {
        for round in 0..50 {
            let db = temp_db();
            let pool = pool_with_connections(&db, 4).await;
            assert!(wal(&db).exists());
            close(&pool).await;
            assert!(pool.is_closed());
            assert_eq!(pool.status().size, 0);
            assert!(
                !wal(&db).exists(),
                "round {round}: a connection is still open after close"
            );
            let rows: i64 = Connection::open(&db)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 4, "round {round}");
            std::fs::remove_dir_all(db.parent().unwrap()).unwrap();
        }
    }
}
