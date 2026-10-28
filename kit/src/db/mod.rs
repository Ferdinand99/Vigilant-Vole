//! Database plumbing shared by every product: the connection pool, the
//! `interact` helper, key/value settings, users and API tokens, and the
//! migration runner ([`migrate`]).

use std::path::Path;

use deadpool_sqlite::{
    Config as PoolConfig, Hook, HookError, InteractError, Pool, PoolError, Runtime,
};
use rusqlite::{Connection, TransactionBehavior};

pub use migrate::{
    MigrateOptions, MigrationError, MigrationReport, MigrationSet, UpgradeError, apply_migrations,
    migrate, migrate_with, run_migrations,
};

pub mod api_tokens;
pub mod migrate;
pub mod passkeys;
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
    build_pool_with(db_path, PoolOptions::default())
}

/// A separate pool for the check engine (heartbeats, alert state,
/// notifications). With one shared pool, a busy web UI (dashboards, status
/// pages, API clients) queues every heartbeat write behind its own reads and
/// checks fall minutes behind; with its own connections the engine only
/// competes for CPU and the write lock.
pub fn build_engine_pool(db_path: &Path) -> Pool {
    build_pool_with(
        db_path,
        PoolOptions {
            max_size: Some(ENGINE_POOL_SIZE),
            ..PoolOptions::default()
        },
    )
}

/// How [`build_pool_with`] sets up its connections. The default is
/// [`build_pool`]'s.
#[derive(Debug, Clone, Copy, Default)]
pub struct PoolOptions {
    /// Most connections at once; `None` is deadpool's default.
    pub max_size: Option<usize>,
    /// SQL run on every new connection after [`CONNECTION_PRAGMAS`]
    /// (`PRAGMA cache_size = -16000;`). Empty for none.
    pub pragmas: &'static str,
    /// SQL run on a new connection while the database has no schema yet,
    /// before anything creates a table: settings that only take effect
    /// then, like `PRAGMA auto_vacuum = INCREMENTAL;` or
    /// `PRAGMA page_size = 8192;`. Every connection of the pool runs it
    /// while the file is empty, so whichever one creates the first table
    /// has it. Empty for none.
    pub new_database_pragmas: &'static str,
}

/// A pool for any database file: [`build_pool`] with [`PoolOptions`], for a
/// product's second database (a store, a cache) that needs its own size or
/// pragmas. Migrate it with [`migrate_with`].
pub fn build_pool_with(db_path: &Path, options: PoolOptions) -> Pool {
    let builder = PoolConfig::new(db_path)
        .builder(Runtime::Tokio1)
        .expect("invalid sqlite pool config");
    let builder = match options.max_size {
        Some(n) => builder.max_size(n),
        None => builder,
    };
    builder
        .post_create(Hook::async_fn(move |conn, _| {
            Box::pin(async move {
                conn.interact(move |c| {
                    // Every transaction here writes. Taking the write lock at
                    // BEGIN makes a busy writer wait (busy_timeout) instead of
                    // failing with "database is locked" when another
                    // connection committed between its first read and write.
                    c.set_transaction_behavior(TransactionBehavior::Immediate);
                    c.execute_batch(CONNECTION_PRAGMAS)?;
                    if !options.pragmas.is_empty() {
                        c.execute_batch(options.pragmas)?;
                    }
                    if !options.new_database_pragmas.is_empty() {
                        apply_new_database_pragmas(c, options.new_database_pragmas)?;
                    }
                    Ok(())
                })
                .await
                .map_err(|e| HookError::message(e.to_string()))?
                .map_err(HookError::Backend)
            })
        }))
        .build()
        .expect("failed to create sqlite pool")
}

/// Runs `pragmas` when the database has no schema yet (nothing in
/// `sqlite_master`): the hook behind [`PoolOptions::new_database_pragmas`],
/// for code that opens a [`Connection`] itself. Returns whether it ran them.
pub fn apply_new_database_pragmas(conn: &Connection, pragmas: &str) -> rusqlite::Result<bool> {
    let objects: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))?;
    if objects > 0 {
        return Ok(false);
    }
    conn.execute_batch(pragmas)?;
    Ok(true)
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

    fn no_fixups(_: &mut Connection) -> Result<(), MigrationError> {
        Ok(())
    }

    fn store_v1() -> Vec<(String, String)> {
        vec![(
            "0001_items.sql".into(),
            "CREATE TABLE items (id INTEGER PRIMARY KEY, body BLOB);".into(),
        )]
    }

    fn store_v2() -> Vec<(String, String)> {
        let mut list = store_v1();
        list.push((
            "0002_kind.sql".into(),
            "ALTER TABLE items ADD COLUMN kind TEXT;".into(),
        ));
        list
    }

    /// A second database: no core tables, nothing to adopt.
    const STORE: MigrationSet = MigrationSet {
        core: &[],
        adopt_core: None,
        migrations: store_v1,
        fk_off_marker: "-- fk=off",
        baseline: None,
        after: no_fixups,
    };

    static STORE_V1: MigrationSet = STORE;

    static STORE_V2: MigrationSet = MigrationSet {
        migrations: store_v2,
        ..STORE
    };

    const STORE_POOL: PoolOptions = PoolOptions {
        max_size: Some(2),
        pragmas: "PRAGMA cache_size = -2000;",
        new_database_pragmas: "PRAGMA auto_vacuum = INCREMENTAL;",
    };

    #[tokio::test]
    async fn a_second_database_migrates_with_its_own_set_and_options() {
        let db = temp_db();
        let pool = build_pool_with(&db, STORE_POOL);
        let report = migrate_with(&pool, &STORE_V1, MigrateOptions::default())
            .await
            .unwrap();
        assert_eq!(report.applied, ["0001_items.sql"]);
        assert!(report.backup.is_none());
        let (vacuum, cache, tables): (i64, i64, Vec<String>) = interact(&pool, |c| {
            let vacuum = c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
            let cache = c.query_row("PRAGMA cache_size", [], |r| r.get(0))?;
            let mut stmt =
                c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
            let tables = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok((vacuum, cache, tables))
        })
        .await
        .unwrap();
        assert_eq!(
            vacuum, 2,
            "incremental auto-vacuum, set before the first table"
        );
        assert_eq!(cache, -2000);
        assert_eq!(tables, ["items", "schema_migrations"], "no core tables");
        close(&pool).await;

        // An upgrade without a backup directory takes no copy...
        let pool = build_pool_with(&db, STORE_POOL);
        let report = migrate_with(&pool, &STORE_V2, MigrateOptions::default())
            .await
            .unwrap();
        assert_eq!(report.applied, ["0002_kind.sql"]);
        assert!(report.backup.is_none());
        // ...and an older build refuses it without naming a backup folder.
        let err = migrate_with(&pool, &STORE_V1, MigrateOptions::default())
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(matches!(
            *err.error,
            MigrationError::NewerSchema { backups: None, .. }
        ));
        assert!(message.contains("schema 0002_kind"), "{message}");
        assert!(!message.contains("restore"), "{message}");
        close(&pool).await;
        std::fs::remove_dir_all(db.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn a_second_database_can_take_a_copy_with_its_own_name() {
        let db = temp_db();
        let backups = db.parent().unwrap().join("backups");
        let pool = build_pool_with(&db, STORE_POOL);
        let options = MigrateOptions {
            backup: Some(&backups),
            backup_prefix: Some("store-before-"),
        };
        let first = migrate_with(&pool, &STORE_V1, options).await.unwrap();
        assert!(first.backup.is_none(), "a new database needs no copy");
        let second = migrate_with(&pool, &STORE_V2, options).await.unwrap();
        let copy = second.backup.expect("a copy before 0002");
        let name = copy.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("store-before-0002_kind-"), "{name}");
        let err = migrate_with(&pool, &STORE_V1, options).await.unwrap_err();
        assert!(err.to_string().contains("restore the backup"), "{err}");
        close(&pool).await;
        std::fs::remove_dir_all(db.parent().unwrap()).unwrap();
    }

    /// Upgrading to the release that seals two-factor secrets: the copy taken
    /// first holds them in plaintext and is marked; a later start that finds
    /// them sealed deletes it.
    #[tokio::test]
    async fn the_copy_before_sealing_two_factor_secrets_is_deleted_by_a_later_start() {
        /// The kit before this release: no record for two-factor secrets yet.
        static BEFORE: MigrationSet = MigrationSet {
            core: &[
                migrate::CORE_TABLES,
                migrate::TOKEN_SCOPES,
                migrate::PASSKEYS,
                migrate::SEALED_SECRETS,
            ],
            adopt_core: None,
            migrations: Vec::new,
            fk_off_marker: "-- fk=off",
            baseline: None,
            after: no_fixups,
        };
        // The release that records it (later core migrations came after).
        static AFTER: MigrationSet = MigrationSet {
            core: &[
                migrate::CORE_TABLES,
                migrate::TOKEN_SCOPES,
                migrate::PASSKEYS,
                migrate::SEALED_SECRETS,
                migrate::SEALED_TOTP,
            ],
            adopt_core: None,
            migrations: Vec::new,
            fk_off_marker: "-- fk=off",
            baseline: None,
            after: no_fixups,
        };
        let db = temp_db();
        let backups = db.parent().unwrap().join("backups");
        let options = MigrateOptions {
            backup: Some(&backups),
            backup_prefix: Some("app-before-"),
        };
        let pool = build_pool(&db);
        migrate_with(&pool, &BEFORE, options).await.unwrap();
        // A user with two-factor on, secret in plaintext (the old release).
        interact(&pool, |c| {
            let id = users::insert_user(c, "ada", "h", crate::auth::Role::Admin)?;
            users::set_pending_totp_secret(c, id, "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP")?;
            users::enable_totp(c, id, 0, &[])
        })
        .await
        .unwrap();

        // The upgrade: a copy named after the new migration, marked.
        let report = migrate_with(&pool, &AFTER, options).await.unwrap();
        assert_eq!(report.applied, ["kit/0005_sealed_totp.sql"]);
        let copy = report.backup.expect("a copy before the upgrade");
        let name = copy.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("app-before-kit-0005_sealed_totp-"),
            "{name}"
        );
        assert!(Path::new(&format!("{}{}", copy.display(), ".plaintext-secrets")).exists());

        // This start sealed them (as `Core::new` does) ...
        let key = axum_extra::extract::cookie::Key::generate();
        let k = key.clone();
        let sealed = interact(&pool, move |c| crate::auth::totp::seal_stored(c, &k))
            .await
            .unwrap();
        assert_eq!(sealed, 1);
        // ... but the copy it took itself stays,
        let none = migrate::prune_plaintext_backups(
            &backups,
            "app-before-",
            Some(&copy),
            true,
            migrate::PLAINTEXT_BACKUP_MAX_AGE,
        );
        assert_eq!(none, 0);
        assert!(copy.exists());
        // while the next start deletes it, and its marker.
        let gone = migrate::prune_plaintext_backups(
            &backups,
            "app-before-",
            None,
            true,
            migrate::PLAINTEXT_BACKUP_MAX_AGE,
        );
        assert_eq!(gone, 1);
        assert!(std::fs::read_dir(&backups).unwrap().next().is_none());
        close(&pool).await;
        std::fs::remove_dir_all(db.parent().unwrap()).unwrap();
    }
    #[test]
    fn new_database_pragmas_only_run_on_an_empty_file() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(apply_new_database_pragmas(&conn, "PRAGMA auto_vacuum = FULL;").unwrap());
        conn.execute_batch("CREATE TABLE t (x)").unwrap();
        assert!(!apply_new_database_pragmas(&conn, "PRAGMA auto_vacuum = NONE;").unwrap());
        let mode: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, 1);
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
