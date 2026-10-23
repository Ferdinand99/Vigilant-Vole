//! The migration runner.
//!
//! Each migration runs exactly once, inside its own transaction, and is
//! recorded in `schema_migrations`. The product supplies *what* to run as a
//! [`MigrationSet`] (registered in its [`KitConfig`](super::super::config::KitConfig));
//! the runner owns *how*: tracking, the safety copy taken before upgrading,
//! refusing databases from a newer release, and foreign key handling.
//!
//! Besides the product's own migrations the runner applies the kit's core
//! set ([`CORE`]: users, API tokens, settings), recorded under `kit/…`
//! names, before the product's. A product whose own migrations created those
//! tables before the kit had them adopts the core set instead
//! ([`MigrationSet::adopt_core`]).

use std::path::{Path, PathBuf};

use deadpool_sqlite::Pool;
use rusqlite::{Connection, OptionalExtension, params};

/// The core tables: `users`, `api_tokens` and `settings`.
pub const CORE_TABLES: (&str, &str) = (
    "kit/0001_core.sql",
    include_str!("../../migrations/0001_core.sql"),
);

/// `api_token_scopes`: the product scopes of API tokens
/// ([`KitConfig::token_scopes`](crate::config::KitConfig::token_scopes)).
/// Existing tokens get no rows and keep exactly the access they had. A
/// product that declares no token scopes may leave it out of its
/// [`MigrationSet::core`] (Vigilant Vole does, so its schema stays as it
/// was); one that declares any must run it ([`crate::config::install`]
/// checks).
pub const TOKEN_SCOPES: (&str, &str) = (
    "kit/0002_token_scopes.sql",
    include_str!("../../migrations/0002_token_scopes.sql"),
);

/// The kit's core migrations, `(name, sql)` in order: every product's
/// default for [`MigrationSet::core`]. The names start with `kit/` so they
/// never collide with a product's file names.
pub const CORE: &[(&str, &str)] = &[CORE_TABLES, TOKEN_SCOPES];

/// What a product tells the runner about its migrations.
pub struct MigrationSet {
    /// The kit's core migrations, normally [`CORE`]. They run before the
    /// product's own pending migrations.
    pub core: &'static [(&'static str, &'static str)],
    /// The product migration that records the core migrations as applied,
    /// for a product whose own (older) migrations already created the core
    /// tables. While it is pending, the product's migrations run first and
    /// the core migrations it records are skipped; core migrations it
    /// doesn't record run after it. `None` for a product that starts from
    /// the core set.
    pub adopt_core: Option<&'static str>,
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
    crate::config::get().migrations
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
            let set = set();
            let existing = prepare_tracking_in(conn, set)?;
            refuse_newer_schema(conn, set, &backup_dir.display().to_string())?;
            let pending = pending_migrations(conn, set)?;
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

/// The migration recorded last in `schema_migrations` (insertion order: the
/// `kit/…` core names sort after every product name).
fn latest_applied(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT name FROM schema_migrations ORDER BY rowid DESC LIMIT 1",
        [],
        |r| r.get(0),
    )
    .optional()
}

/// Refuses databases that record migrations this build doesn't embed (a
/// newer release has opened them). `backups` names the backup folder in the
/// message.
///
/// Unknown names are what counts, not how names sort: the core set's
/// `kit/…` names sort after every product migration and are known.
fn refuse_newer_schema(
    conn: &Connection,
    set: &MigrationSet,
    backups: &str,
) -> Result<(), MigrationError> {
    let product = (set.migrations)();
    let known: Vec<String> = core_of(set)
        .into_iter()
        .chain(product.iter().cloned())
        .map(|(name, _)| name)
        .collect();
    // Recording order, so the message names the newest unknown migration.
    let mut stmt = conn.prepare("SELECT name FROM schema_migrations ORDER BY rowid")?;
    let applied = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(newest) = applied.iter().rev().find(|name| !known.contains(name)) else {
        return Ok(());
    };
    Err(MigrationError::NewerSchema {
        product: crate::config::get().product.to_string(),
        schema: newest.trim_end_matches(".sql").to_string(),
        known: product
            .last()
            .map(|(n, _)| n.as_str())
            .or(known.last().map(String::as_str))
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
    let prefix = crate::config::get().backup_prefix;
    // Core names (`kit/0001_core`) must not become directories.
    let label = label.replace(['/', '\\'], "-");
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

/// The core migrations the product runs (normally [`CORE`]), in order.
pub fn core_migrations() -> Vec<(String, String)> {
    core_of(set())
}

fn core_of(set: &MigrationSet) -> Vec<(String, String)> {
    set.core
        .iter()
        .map(|(name, sql)| (name.to_string(), sql.to_string()))
        .collect()
}

/// The names `schema_migrations` holds once every migration is applied:
/// core and product, sorted by name.
pub fn all_migration_names() -> Vec<String> {
    let mut names: Vec<String> = core_migrations()
        .into_iter()
        .chain(embedded_migrations())
        .map(|(name, _)| name)
        .collect();
    names.sort();
    names
}

/// Synchronous migration runner (also used directly by tests).
///
/// Each migration runs exactly once, inside its own transaction, and is
/// recorded in `schema_migrations`. Returns the names applied in this call.
pub fn apply_migrations(conn: &mut Connection) -> Result<Vec<String>, MigrationError> {
    apply_migrations_with(conn, set())
}

/// [`apply_migrations`] with an explicit set instead of the installed
/// product's.
pub fn apply_migrations_with(
    conn: &mut Connection,
    set: &MigrationSet,
) -> Result<Vec<String>, MigrationError> {
    prepare_tracking_in(conn, set)?;
    refuse_newer_schema(conn, set, "DATA_DIR/backups")?;
    let mut applied = Vec::new();
    for (name, sql) in pending_migrations(conn, set)? {
        // An adoption migration records core migrations as applied.
        if is_recorded(conn, &name)? {
            continue;
        }
        apply_one_in(conn, set, &name, &sql)?;
        tracing::info!(migration = %name, "applied database migration");
        applied.push(name);
    }
    (set.after)(conn)?;
    Ok(applied)
}

/// Ensures `schema_migrations` exists and records the legacy baseline.
/// Returns whether the database already had a schema (i.e. is not brand new).
pub fn prepare_tracking(conn: &Connection) -> rusqlite::Result<bool> {
    prepare_tracking_in(conn, set())
}

fn prepare_tracking_in(conn: &Connection, set: &MigrationSet) -> rusqlite::Result<bool> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             name TEXT PRIMARY KEY,
             applied_at TEXT NOT NULL DEFAULT (datetime('now'))
         );",
    )?;
    mark_legacy_baseline(conn, set)?;
    let tracked: i64 =
        conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))?;
    Ok(tracked > 0)
}

fn is_recorded(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE name = ?1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

/// Migrations not yet recorded in `schema_migrations`, in the order they
/// run: the core set, then the product's; while the product's adoption
/// migration is pending, the product's first (see
/// [`MigrationSet::adopt_core`]).
fn pending_migrations(
    conn: &Connection,
    set: &MigrationSet,
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut core = Vec::new();
    for (name, sql) in core_of(set) {
        if !is_recorded(conn, &name)? {
            core.push((name, sql));
        }
    }
    let mut product = Vec::new();
    for (name, sql) in (set.migrations)() {
        if !is_recorded(conn, &name)? {
            product.push((name, sql));
        }
    }
    let adopting = set
        .adopt_core
        .is_some_and(|adopt| product.iter().any(|(name, _)| name == adopt));
    Ok(if adopting {
        product.into_iter().chain(core).collect()
    } else {
        core.into_iter().chain(product).collect()
    })
}

/// Databases created before migration tracking existed already have the
/// baseline schema; record it so it isn't re-run (it is idempotent, but
/// tracking should reflect reality).
fn mark_legacy_baseline(conn: &Connection, set: &MigrationSet) -> rusqlite::Result<()> {
    let Some(baseline) = &set.baseline else {
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
    apply_one_in(conn, set(), name, sql)
}

fn apply_one_in(
    conn: &mut Connection,
    set: &MigrationSet,
    name: &str,
    sql: &str,
) -> Result<(), MigrationError> {
    let fk_off = sql.contains(set.fk_off_marker);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn no_fixups(_: &mut Connection) -> Result<(), MigrationError> {
        Ok(())
    }

    fn names(conn: &Connection, order: &str) -> Vec<String> {
        conn.prepare(&format!(
            "SELECT name FROM schema_migrations ORDER BY {order}"
        ))
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    }

    fn owned(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, s)| (n.to_string(), s.to_string()))
            .collect()
    }

    /// A new product: the core set, then its own table.
    static NEW_PRODUCT: MigrationSet = MigrationSet {
        core: CORE,
        adopt_core: None,
        migrations: || {
            owned(&[(
                "0001_things.sql",
                "CREATE TABLE things (id INTEGER PRIMARY KEY, owner INTEGER REFERENCES users(id));",
            )])
        },
        fk_off_marker: "-- fk=off",
        baseline: None,
        after: no_fixups,
    };

    const ADOPT: &str =
        "INSERT OR IGNORE INTO schema_migrations (name) VALUES ('kit/0001_core.sql');";

    /// A product whose own first migration made the core tables, and which
    /// adopts the core set with its second.
    static OLD_PRODUCT: MigrationSet = MigrationSet {
        core: &[CORE_TABLES],
        adopt_core: Some("0002_adopt.sql"),
        migrations: || owned(&[("0001_own.sql", CORE_TABLES.1), ("0002_adopt.sql", ADOPT)]),
        fk_off_marker: "-- fk=off",
        baseline: None,
        after: no_fixups,
    };

    /// The same product once the kit has a second core migration that the
    /// adoption doesn't record.
    static OLD_PRODUCT_LATER: MigrationSet = MigrationSet {
        core: &[
            CORE_TABLES,
            (
                "kit/0002_more.sql",
                "ALTER TABLE users ADD COLUMN extra TEXT;",
            ),
        ],
        adopt_core: Some("0002_adopt.sql"),
        migrations: || owned(&[("0001_own.sql", CORE_TABLES.1), ("0002_adopt.sql", ADOPT)]),
        fk_off_marker: "-- fk=off",
        baseline: None,
        after: no_fixups,
    };

    #[test]
    fn a_new_product_runs_the_core_set_first() {
        let mut conn = Connection::open_in_memory().unwrap();
        let applied = apply_migrations_with(&mut conn, &NEW_PRODUCT).unwrap();
        assert_eq!(
            applied,
            [
                "kit/0001_core.sql",
                "kit/0002_token_scopes.sql",
                "0001_things.sql"
            ]
        );
        for table in [
            "users",
            "api_tokens",
            "api_token_scopes",
            "settings",
            "things",
        ] {
            assert!(table_exists(&conn, table), "{table}");
        }
        assert!(
            apply_migrations_with(&mut conn, &NEW_PRODUCT)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_adopting_product_records_the_core_set_instead_of_running_it() {
        // Fresh: its own migrations, the adoption records core.
        let mut conn = Connection::open_in_memory().unwrap();
        let applied = apply_migrations_with(&mut conn, &OLD_PRODUCT).unwrap();
        assert_eq!(applied, ["0001_own.sql", "0002_adopt.sql"]);
        assert_eq!(
            names(&conn, "name"),
            ["0001_own.sql", "0002_adopt.sql", "kit/0001_core.sql"]
        );
        assert_eq!(
            latest_applied(&conn).unwrap().as_deref(),
            Some("0002_adopt.sql")
        );

        // An existing database from before the adoption.
        let mut conn = Connection::open_in_memory().unwrap();
        prepare_tracking_in(&conn, &OLD_PRODUCT).unwrap();
        apply_one_in(&mut conn, &OLD_PRODUCT, "0001_own.sql", CORE_TABLES.1).unwrap();
        let applied = apply_migrations_with(&mut conn, &OLD_PRODUCT).unwrap();
        assert_eq!(applied, ["0002_adopt.sql"]);
        assert!(
            apply_migrations_with(&mut conn, &OLD_PRODUCT)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn core_migrations_the_adoption_does_not_record_still_run() {
        let mut conn = Connection::open_in_memory().unwrap();
        let applied = apply_migrations_with(&mut conn, &OLD_PRODUCT_LATER).unwrap();
        assert_eq!(
            applied,
            ["0001_own.sql", "0002_adopt.sql", "kit/0002_more.sql"]
        );

        // Adopted before the kit grew kit/0002: it runs first next time.
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations_with(&mut conn, &OLD_PRODUCT).unwrap();
        let applied = apply_migrations_with(&mut conn, &OLD_PRODUCT_LATER).unwrap();
        assert_eq!(applied, ["kit/0002_more.sql"]);
        conn.execute("UPDATE users SET extra = 'x'", []).unwrap();
    }

    fn newer(conn: &Connection, set: &MigrationSet) -> Option<(String, String)> {
        match refuse_newer_schema(conn, set, "backups") {
            Ok(()) => None,
            Err(MigrationError::NewerSchema { schema, known, .. }) => Some((schema, known)),
            Err(e) => panic!("{e}"),
        }
    }

    /// A database a product without token scopes migrated (Vigilant Vole)
    /// takes the token scope table when the product adds it to its set.
    #[test]
    fn the_token_scope_table_can_be_added_later() {
        static WITHOUT: MigrationSet = MigrationSet {
            core: &[CORE_TABLES],
            adopt_core: None,
            migrations: NEW_PRODUCT.migrations,
            fk_off_marker: "-- fk=off",
            baseline: None,
            after: no_fixups,
        };
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations_with(&mut conn, &WITHOUT).unwrap();
        assert!(!table_exists(&conn, "api_token_scopes"));
        assert_eq!(newer(&conn, &NEW_PRODUCT), None);
        let applied = apply_migrations_with(&mut conn, &NEW_PRODUCT).unwrap();
        assert_eq!(applied, ["kit/0002_token_scopes.sql"]);
        assert!(table_exists(&conn, "api_token_scopes"));
        // Going back: the set without it refuses the database.
        assert!(newer(&conn, &WITHOUT).is_some());
    }

    #[test]
    fn core_names_sorting_last_do_not_look_newer() {
        // `kit/0001_core.sql` sorts after every product name.
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations_with(&mut conn, &OLD_PRODUCT).unwrap();
        assert_eq!(newer(&conn, &OLD_PRODUCT), None);
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations_with(&mut conn, &NEW_PRODUCT).unwrap();
        assert_eq!(newer(&conn, &NEW_PRODUCT), None);

        // A newer product migration is named, even though the core row
        // sorts after it.
        conn.execute(
            "INSERT INTO schema_migrations (name) VALUES ('0200_future.sql')",
            [],
        )
        .unwrap();
        assert_eq!(
            newer(&conn, &NEW_PRODUCT),
            Some(("0200_future".into(), "0001_things".into()))
        );

        // So is a newer core migration.
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations_with(&mut conn, &OLD_PRODUCT).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (name) VALUES ('kit/0002_more.sql')",
            [],
        )
        .unwrap();
        assert_eq!(
            newer(&conn, &OLD_PRODUCT),
            Some(("kit/0002_more".into(), "0002_adopt".into()))
        );
        assert_eq!(newer(&conn, &OLD_PRODUCT_LATER), None);
        assert!(matches!(
            apply_migrations_with(&mut conn, &OLD_PRODUCT),
            Err(MigrationError::NewerSchema { .. })
        ));
    }

    #[test]
    fn a_core_name_makes_a_flat_backup_file_name() {
        let dir = std::env::temp_dir().join(format!("vole-kit-backup-{}", std::process::id()));
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (x)").unwrap();
        let path = backup_before_migration(&conn, &dir, "kit/0002_more").unwrap();
        let parent = path.parent().map(Path::to_path_buf);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(parent, Some(dir));
        assert!(name.contains("kit-0002_more-"), "{name}");
    }
}
