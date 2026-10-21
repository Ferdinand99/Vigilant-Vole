//! The migration runner.
//!
//! Each migration runs exactly once, inside its own transaction, and is
//! recorded in `schema_migrations`. The product supplies *what* to run as a
//! [`MigrationSet`] (registered in its [`KitConfig`](super::super::config::KitConfig));
//! the runner owns *how*: tracking, the safety copy taken before upgrading,
//! refusing databases from a newer release, and foreign key handling.

use std::path::{Path, PathBuf};

use deadpool_sqlite::Pool;
use rusqlite::{Connection, OptionalExtension, params};

/// What a product tells the runner about its migrations.
pub struct MigrationSet {
    /// Every migration as `(file name, sql)`, sorted by name.
    pub migrations: fn() -> Vec<(String, String)>,
    /// Marker a migration may contain to run with foreign keys off (the
    /// SQLite "rebuild the table" procedure).
    pub fk_off_marker: &'static str,
    /// For databases that predate migration tracking.
    pub baseline: Option<Baseline>,
    /// Data fix-ups SQL can't do (hashing); each does nothing once done.
    /// Runs after the pending migrations.
    pub after: fn(&mut Connection) -> Result<(), MigrationError>,
}

/// A migration that predates tracking: a database that already has one of
/// `tables` is taken to have applied `name`.
pub struct Baseline {
    pub name: &'static str,
    pub tables: &'static [&'static str],
}

fn set() -> &'static MigrationSet {
    crate::kit::config::get().migrations
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
    #[error("can't open the database: {0}")]
    Unavailable(String),
    /// The database records migrations this build doesn't have: it was
    /// opened by a newer release. Running against it could misread or
    /// damage data, so startup stops instead.
    #[error(
        "the database was created by a newer {product} (schema {schema}); this version only knows schemas up to {known}. \
         Upgrade {product} again, or restore the backup made before that upgrade from {backups}"
    )]
    NewerSchema {
        /// The product's name.
        product: String,
        /// The newest migration the database has, without `.sql`.
        schema: String,
        /// The newest migration this build has, without `.sql`.
        known: String,
        /// Where pre-migration backups are kept.
        backups: String,
    },
}

/// A failed startup migration, with where the database was left and where
/// the copy taken before migrating is.
#[derive(Debug, thiserror::Error)]
#[error("database migration failed: {error}{}", recovery_hint(.at, .backup))]
pub struct UpgradeError {
    #[source]
    pub error: Box<MigrationError>,
    /// The last migration that is applied (each one commits on its own).
    pub at: Option<String>,
    /// The copy written before the first pending migration ran.
    pub backup: Option<PathBuf>,
}

impl UpgradeError {
    fn new(error: MigrationError, at: Option<String>, backup: Option<PathBuf>) -> Self {
        UpgradeError {
            error: Box::new(error),
            at,
            backup,
        }
    }
}

fn recovery_hint(at: &Option<String>, backup: &Option<PathBuf>) -> String {
    let mut hint = String::new();
    if let Some(at) = at {
        hint.push_str(&format!(
            ". Nothing of the failed migration was kept; the database is at {at}"
        ));
    }
    if let Some(backup) = backup {
        hint.push_str(&format!(
            ". The copy made before upgrading is {}",
            backup.display()
        ));
    }
    hint
}

/// What [`migrate`] did.
#[derive(Debug, Default)]
pub struct MigrationReport {
    /// Migrations applied, in order.
    pub applied: Vec<String>,
    /// The pre-migration copy, when one was needed.
    pub backup: Option<PathBuf>,
}

/// Apply all pending embedded migrations. Panics on failure (tests and
/// tools); the server uses [`migrate`] and reports the error instead.
pub async fn run_migrations(pool: &Pool, backup_dir: &Path) {
    if let Err(e) = migrate(pool, backup_dir).await {
        panic!("{e}");
    }
}

/// Apply all pending embedded migrations; the server must not start when
/// this fails.
///
/// Before migrating a database that already holds a schema, a consistent copy
/// is written to `backup_dir` (`VACUUM INTO`); restoring that file is the
/// rollback path, since SQLite migrations here are forward-only. Each
/// migration commits on its own, so a failure leaves the database at the last
/// migration that succeeded (never half-way through one). A database from a
/// newer release is refused before anything is written.
pub async fn migrate(pool: &Pool, backup_dir: &Path) -> Result<MigrationReport, UpgradeError> {
    let backup_dir = backup_dir.to_path_buf();
    let unavailable = |e: String| UpgradeError::new(MigrationError::Unavailable(e), None, None);
    let conn = pool.get().await.map_err(|e| unavailable(e.to_string()))?;
    conn.interact(move |conn| {
        let mut report = MigrationReport::default();
        let started = (|| -> Result<(), MigrationError> {
            // WAL is persistent per database file; set it once here rather than per connection.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            let existing = prepare_tracking(conn)?;
            refuse_newer_schema(conn, &backup_dir.display().to_string())?;
            let pending = pending_migrations(conn)?;
            if existing && !pending.is_empty() {
                let first = pending
                    .first()
                    .map(|(name, _)| name.trim_end_matches(".sql"))
                    .unwrap_or("");
                report.backup = Some(backup_before_migration(conn, &backup_dir, first)?);
            }
            Ok(())
        })();
        if let Err(error) = started {
            return Err(UpgradeError::new(error, None, report.backup));
        }
        match apply_migrations(conn) {
            Ok(applied) => {
                report.applied = applied;
                Ok(report)
            }
            Err(error) => Err(UpgradeError::new(
                error,
                latest_applied(conn).ok().flatten(),
                report.backup,
            )),
        }
    })
    .await
    .map_err(|e| unavailable(e.to_string()))?
}

/// The newest migration recorded in `schema_migrations`.
fn latest_applied(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT MAX(name) FROM schema_migrations", [], |r| r.get(0))
}

/// Refuses databases that record migrations this build doesn't embed (a
/// newer release has opened them). `backups` names the backup folder in the
/// message.
fn refuse_newer_schema(conn: &Connection, backups: &str) -> Result<(), MigrationError> {
    let known: Vec<String> = embedded_migrations()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let mut stmt = conn.prepare("SELECT name FROM schema_migrations ORDER BY name")?;
    let applied = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(newest) = applied.iter().rev().find(|name| !known.contains(name)) else {
        return Ok(());
    };
    Err(MigrationError::NewerSchema {
        product: crate::kit::config::get().product.to_string(),
        schema: newest.trim_end_matches(".sql").to_string(),
        known: known
            .last()
            .map(|n| n.trim_end_matches(".sql").to_string())
            .unwrap_or_default(),
        backups: backups.to_string(),
    })
}

pub fn backup_before_migration(
    conn: &Connection,
    dir: &Path,
    label: &str,
) -> Result<PathBuf, MigrationError> {
    std::fs::create_dir_all(dir).map_err(|e| MigrationError::Backup(e.to_string()))?;
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
    // A start that fails and is retried within the same second (a restart
    // loop) must not trip over the previous attempt's copy.
    let prefix = crate::kit::config::get().backup_prefix;
    let mut path = dir.join(format!("{prefix}{label}-{stamp}.db"));
    let mut attempt = 1;
    while path.exists() {
        attempt += 1;
        path = dir.join(format!("{prefix}{label}-{stamp}-{attempt}.db"));
    }
    let path_str = path.to_str().ok_or_else(|| {
        MigrationError::Backup(format!("non-UTF-8 backup path {}", path.display()))
    })?;
    conn.execute("VACUUM INTO ?1", params![path_str])
        .map_err(|e| MigrationError::Backup(e.to_string()))?;
    tracing::info!(path = %path.display(), "backed up database before migrating");
    Ok(path)
}

/// Every migration of the product, sorted by file name.
pub fn embedded_migrations() -> Vec<(String, String)> {
    (set().migrations)()
}

/// Synchronous migration runner (also used directly by tests).
///
/// Each migration runs exactly once, inside its own transaction, and is
/// recorded in `schema_migrations`. Returns the names applied in this call.
pub fn apply_migrations(conn: &mut Connection) -> Result<Vec<String>, MigrationError> {
    prepare_tracking(conn)?;
    refuse_newer_schema(conn, "DATA_DIR/backups")?;
    let mut applied = Vec::new();
    for (name, sql) in pending_migrations(conn)? {
        apply_one(conn, &name, &sql)?;
        tracing::info!(migration = %name, "applied database migration");
        applied.push(name);
    }
    (set().after)(conn)?;
    Ok(applied)
}

/// Ensures `schema_migrations` exists and records the legacy baseline.
/// Returns whether the database already had a schema (i.e. is not brand new).
pub fn prepare_tracking(conn: &Connection) -> rusqlite::Result<bool> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             name TEXT PRIMARY KEY,
             applied_at TEXT NOT NULL DEFAULT (datetime('now'))
         );",
    )?;
    mark_legacy_baseline(conn)?;
    let tracked: i64 =
        conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;
    Ok(tracked > 0)
}

/// Embedded migrations not yet recorded in `schema_migrations`, in order.
fn pending_migrations(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut pending = Vec::new();
    for (name, sql) in embedded_migrations() {
        let done: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?;
        if done.is_none() {
            pending.push((name, sql));
        }
    }
    Ok(pending)
}

/// Databases created before migration tracking existed already have the
/// baseline schema; record it so it isn't re-run (it is idempotent, but
/// tracking should reflect reality).
fn mark_legacy_baseline(conn: &Connection) -> rusqlite::Result<()> {
    let Some(baseline) = &set().baseline else {
        return Ok(());
    };
    let tracked: i64 =
        conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;
    if tracked > 0 {
        return Ok(());
    }
    let tables = baseline
        .tables
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let has_legacy_schema: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ({tables})"),
        [],
        |r| r.get(0),
    )?;
    if has_legacy_schema > 0 {
        conn.execute(
            "INSERT INTO schema_migrations (name) VALUES (?1)",
            params![baseline.name],
        )?;
        tracing::info!(
            "existing database detected; marked {} as applied",
            baseline.name
        );
    }
    Ok(())
}

pub fn apply_one(conn: &mut Connection, name: &str, sql: &str) -> Result<(), MigrationError> {
    let fk_off = sql.contains(set().fk_off_marker);
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

fn run_in_transaction(
    conn: &mut Connection,
    name: &str,
    sql: &str,
    check_fks: bool,
) -> Result<(), MigrationError> {
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
    tx.execute(
        "INSERT INTO schema_migrations (name) VALUES (?1)",
        params![name],
    )?;
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
