//! Migration runner tests: fresh install, upgrade of a pre-tracking (0001)
//! database with data, and idempotency.

use rusqlite::{Connection, params};

use super::{CONNECTION_PRAGMAS, apply_migrations, test_conn};

const LEGACY_0001: &str = include_str!("../../migrations/0001_init.sql");

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

fn applied(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM schema_migrations ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// A database exactly as the pre-tracking binary left it: 0001 schema, no
/// `schema_migrations`, foreign keys never enabled, plus sample rows
/// (including an orphaned heartbeat the old code could leave behind).
fn legacy_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    // The orphaned heartbeat below needs enforcement off (rusqlite's bundled
    // SQLite turns it on by default; other builds or tools may not have).
    conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    conn.execute_batch(LEGACY_0001).unwrap();
    conn.execute_batch(
        "INSERT INTO users (id, username, password_hash, created_at)
             VALUES (1, 'alice', 'hash-a', '2025-01-01 10:00:00'),
                    (2, 'bob', 'hash-b', '2025-01-02 10:00:00');
         INSERT INTO monitors (id, name, monitor_type, target, interval_seconds, timeout_seconds, retries, active, created_at)
             VALUES (1, 'Site', 'http', 'https://example.com', 30, 5, 2, 1, '2025-01-03 00:00:00'),
                    (2, 'DB', 'tcp', 'db:5432', 60, 10, 0, 0, '2025-01-04 00:00:00'),
                    (3, 'Router', 'ping', '10.0.0.1', 120, 3, 1, 1, '2025-01-05 00:00:00');
         INSERT INTO heartbeats (id, monitor_id, status, response_time_ms, message, checked_at)
             VALUES (1, 1, 'up', 120, NULL, '2025-02-01 00:00:00'),
                    (2, 1, 'down', NULL, 'timed out', '2025-02-01 00:01:00'),
                    (3, 3, 'pending', NULL, NULL, '2025-02-01 00:02:00'),
                    (4, 99, 'up', 5, NULL, '2025-02-01 00:03:00');
         INSERT INTO notification_channels (id, name, kind, config, active)
             VALUES (1, 'Discord', 'discord', '{\"url\":\"https://discord.test\"}', 1),
                    (2, 'Hook', 'webhook', '{\"url\":\"https://hook.test\"}', 0);",
    )
    .unwrap();
    conn
}

fn upgrade(mut conn: Connection) -> (Connection, Vec<String>) {
    conn.execute_batch(CONNECTION_PRAGMAS).unwrap();
    let ran = apply_migrations(&mut conn).unwrap();
    (conn, ran)
}

#[test]
fn fresh_database_applies_every_migration() {
    let conn = test_conn();
    let names = applied(&conn);
    assert!(names.contains(&"0001_init.sql".to_string()));
    assert!(names.contains(&"0002_full_schema.sql".to_string()));
    for table in [
        "users",
        "monitors",
        "heartbeats",
        "heartbeat_daily",
        "monitor_expiry",
        "monitor_events",
        "tags",
        "monitor_tags",
        "notification_channels",
        "monitor_notifications",
        "status_pages",
        "status_page_sections",
        "status_page_monitors",
        "incidents",
        "maintenance_windows",
        "maintenance_monitors",
        "maintenance_status_pages",
        "api_tokens",
        "agents",
        "settings",
    ] {
        assert!(table_exists(&conn, table), "missing table {table}");
    }
}

#[test]
fn rerunning_migrations_is_a_no_op() {
    let mut conn = test_conn();
    assert!(apply_migrations(&mut conn).unwrap().is_empty());
}

#[test]
fn legacy_database_is_detected_and_baseline_not_rerun() {
    let (conn, ran) = upgrade(legacy_db());
    // Every migration after the baseline runs; the baseline itself is only marked applied.
    assert_eq!(
        ran.first().map(String::as_str),
        Some("0002_full_schema.sql")
    );
    assert!(!ran.iter().any(|name| name.starts_with("0001")));
    // Recorded besides what ran: the baseline and the kit's core set
    // (adopted by 0104, not run).
    assert_eq!(applied(&conn).len(), ran.len() + 2);
    assert!(applied(&conn).contains(&"kit/0001_core.sql".to_string()));
    assert!(!ran.contains(&"kit/0001_core.sql".to_string()));
}

#[test]
fn legacy_upgrade_preserves_users_as_admins() {
    let (conn, _) = upgrade(legacy_db());
    let users: Vec<(i64, String, String, String, String)> = conn
        .prepare("SELECT id, username, password_hash, role, created_at FROM users ORDER BY id")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        users,
        vec![
            (
                1,
                "alice".into(),
                "hash-a".into(),
                "admin".into(),
                "2025-01-01 10:00:00".into()
            ),
            (
                2,
                "bob".into(),
                "hash-b".into(),
                "admin".into(),
                "2025-01-02 10:00:00".into()
            ),
        ]
    );
}

#[test]
fn legacy_upgrade_preserves_monitors_with_new_defaults() {
    let (conn, _) = upgrade(legacy_db());
    let m = crate::db::monitors::get_monitor(&conn, 1).unwrap().unwrap();
    assert_eq!(
        (
            m.name.as_str(),
            m.target.as_str(),
            m.interval_seconds,
            m.timeout_seconds,
            m.retries,
            m.active
        ),
        ("Site", "https://example.com", 30, 5, 2, true)
    );
    assert_eq!(m.created_at, "2025-01-03 00:00:00");
    assert_eq!(m.accepted_status_codes, "200-399");
    assert_eq!(m.source, "manual");
    let paused = crate::db::monitors::get_monitor(&conn, 2).unwrap().unwrap();
    assert!(!paused.active);
}

#[test]
fn legacy_upgrade_keeps_heartbeats_and_drops_orphans() {
    let (conn, _) = upgrade(legacy_db());
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM heartbeats ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ids, vec![1, 2, 3]);
    let msg: Option<String> = conn
        .query_row("SELECT message FROM heartbeats WHERE id = 2", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(msg.as_deref(), Some("timed out"));
}

#[test]
fn legacy_upgrade_links_every_monitor_to_every_channel() {
    let (conn, _) = upgrade(legacy_db());
    let links: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM monitor_notifications WHERE escalation = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(links, 3 * 2);
    let defaults: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notification_channels WHERE is_default = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(defaults, 2);
    let inactive: i64 = conn
        .query_row(
            "SELECT active FROM notification_channels WHERE id = 2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inactive, 0);
}

#[test]
fn foreign_keys_are_enforced_after_upgrade() {
    let (conn, _) = upgrade(legacy_db());
    let fk: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1);
    conn.execute("DELETE FROM monitors WHERE id = 1", [])
        .unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM heartbeats WHERE monitor_id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
    let links: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM monitor_notifications WHERE monitor_id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(links, 0);
}

#[test]
fn upgraded_schema_matches_fresh_schema() {
    let (upgraded, _) = upgrade(legacy_db());
    let fresh = test_conn();
    let schema = |c: &Connection| -> Vec<(String, String)> {
        c.prepare(
            "SELECT name, sql FROM sqlite_master
             WHERE sql IS NOT NULL AND name != 'schema_migrations' ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };
    assert_eq!(schema(&upgraded), schema(&fresh));
}

#[test]
fn failed_migration_rolls_back_and_restores_foreign_keys() {
    let mut conn = test_conn();
    let err = super::apply_one(
        &mut conn,
        "9999_broken.sql",
        "-- vv:foreign_keys=off\nCREATE TABLE half_done (id INTEGER); SELECT * FROM no_such_table;",
    );
    assert!(err.is_err());
    assert!(!table_exists(&conn, "half_done"));
    let fk: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1);
}

#[test]
fn backup_before_migration_writes_a_restorable_copy() {
    let (conn, _) = upgrade(legacy_db());
    let dir = std::env::temp_dir().join(format!("vv-backup-test-{}", std::process::id()));
    super::backup_before_migration(&conn, &dir, "0002_full_schema").unwrap();
    let file = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let copy = Connection::open(&file).unwrap();
    let users: i64 = copy
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .unwrap();
    drop(copy);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(users, 2);
}

/// A database with every embedded migration before `before` applied, as an
/// older release left it.
fn migrated_before(before: &str) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CONNECTION_PRAGMAS).unwrap();
    super::prepare_tracking(&conn).unwrap();
    for (name, sql) in super::embedded_migrations()
        .into_iter()
        .filter(|(name, _)| name.as_str() < before)
    {
        super::apply_one(&mut conn, &name, &sql).unwrap();
    }
    conn
}

#[test]
fn sqlserver_and_mongodb_upgrade_keeps_every_monitor_column() {
    let mut conn = migrated_before("0097");
    conn.execute_batch(
        "INSERT INTO monitors (id, name, monitor_type, target) VALUES (1, 'Group', 'group', '');
         INSERT INTO monitors (id, name, monitor_type, target, parent_id, basic_auth_user, basic_auth_pass, secret,
                               auth_method, oauth_token_url, oauth_client_secret, tls_client_key, external_id, source, created_at)
             VALUES (2, 'API', 'http', 'https://api.test', 1, 'bot', 'pw', NULL,
                     'oauth2', 'https://idp.test/token', 'client-secret', 'KEY', 'x-1', 'config', '2026-01-02 03:04:05'),
                    (3, 'Radius', 'radius', 'radius://r.test', NULL, 'u', 'p', 'shared', 'none', NULL, NULL, NULL, NULL, 'manual', '2026-01-03 00:00:00');
         INSERT INTO heartbeats (monitor_id, status, checked_at) VALUES (2, 'up', '2026-01-04 00:00:00');",
    )
    .unwrap();
    // The old CHECK refuses the new types.
    assert!(
        conn.execute(
            "INSERT INTO monitors (name, monitor_type) VALUES ('x', 'mongodb')",
            []
        )
        .is_err()
    );

    let ran = apply_migrations(&mut conn).unwrap();
    assert!(ran.iter().any(|n| n.starts_with("0097")), "{ran:?}");
    let api = crate::db::monitors::get_monitor(&conn, 2).unwrap().unwrap();
    assert_eq!(
        (
            api.parent_id,
            api.basic_auth_user.as_deref(),
            api.basic_auth_pass.as_deref()
        ),
        (Some(1), Some("bot"), Some("pw"))
    );
    assert_eq!(
        (
            api.auth_method.as_str(),
            api.oauth_token_url.as_deref(),
            api.oauth_client_secret.as_deref()
        ),
        (
            "oauth2",
            Some("https://idp.test/token"),
            Some("client-secret")
        )
    );
    assert_eq!(
        (
            api.tls_client_key.as_deref(),
            api.external_id.as_deref(),
            api.source.as_str()
        ),
        (Some("KEY"), Some("x-1"), "config")
    );
    assert_eq!(api.created_at, "2026-01-02 03:04:05");
    let radius = crate::db::monitors::get_monitor(&conn, 3).unwrap().unwrap();
    assert_eq!(radius.secret.as_deref(), Some("shared"));
    let beats: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM heartbeats WHERE monitor_id = 2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(beats, 1, "heartbeats survive the rebuild");

    // Also the types planned for the same release, and their column.
    for kind in [
        "sqlserver",
        "mongodb",
        "tailscale_ping",
        "game_server",
        "grpc_keyword",
        "real_browser",
    ] {
        conn.execute(
            "INSERT INTO monitors (name, monitor_type, target) VALUES (?1, ?1, 'db.test')",
            params![kind],
        )
        .unwrap();
    }
    let proto: Option<String> = conn
        .query_row("SELECT grpc_proto FROM monitors WHERE id = 3", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(proto, None);
    assert!(
        conn.execute(
            "INSERT INTO monitors (name, monitor_type) VALUES ('x', 'oracle')",
            []
        )
        .is_err()
    );
    // Deleting a monitor still cascades (foreign keys point at the new table).
    conn.execute("DELETE FROM monitors WHERE id = 2", [])
        .unwrap();
    let beats: i64 = conn
        .query_row("SELECT COUNT(*) FROM heartbeats", [], |r| r.get(0))
        .unwrap();
    assert_eq!(beats, 0);
}

#[test]
fn every_monitor_type_is_accepted_by_a_fresh_schema() {
    let conn = test_conn();
    for kind in crate::db::models::MonitorType::all() {
        conn.execute(
            "INSERT INTO monitors (name, monitor_type) VALUES (?1, ?1)",
            params![kind.as_str()],
        )
        .unwrap();
    }
}

#[test]
fn subscriber_tokens_upgrade_hashes_old_links_and_drops_base_url() {
    let mut conn = migrated_before("0103");
    let token = crate::auth::tokens::generate();
    conn.execute(
        "INSERT INTO status_pages (id, slug, title) VALUES (1, 's', 'S')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO status_page_subscribers (page_id, kind, target, token, lang, base_url, confirmed_at)
         VALUES (1, 'email', 'a@example.com', ?1, 'nb', 'http://attacker.example', datetime('now'))",
        params![token],
    )
    .unwrap();
    super::apply_migrations(&mut conn).unwrap();
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('status_page_subscribers')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        !columns.iter().any(|c| c == "base_url" || c == "token"),
        "{columns:?}"
    );
    // The link that was already sent still finds the subscriber; the plain
    // token is gone from the database.
    let sub = crate::db::subscribers::by_token(&conn, &token)
        .unwrap()
        .unwrap();
    assert_eq!((sub.lang.as_str(), sub.is_confirmed()), ("nb", true));
    assert_ne!(sub.token_hash, token);
    // Deleting the page still cascades to its subscribers.
    conn.execute("DELETE FROM status_pages WHERE id = 1", [])
        .unwrap();
    assert_eq!(crate::db::subscribers::count(&conn, 1).unwrap(), 0);
}

#[test]
fn database_from_a_newer_release_is_refused() {
    let mut conn = test_conn();
    conn.execute(
        "INSERT INTO schema_migrations (name) VALUES ('9999_from_the_future.sql')",
        [],
    )
    .unwrap();
    let err = apply_migrations(&mut conn).unwrap_err();
    let msg = err.to_string();
    assert!(
        matches!(&err, super::MigrationError::NewerSchema { schema, .. } if schema == "9999_from_the_future"),
        "{err:?}"
    );
    assert!(
        msg.contains("created by a newer Vigilant Vole (schema 9999_from_the_future)")
            && msg.contains("backups"),
        "{msg}"
    );
}

#[tokio::test]
async fn migrate_refuses_a_newer_database_without_touching_it() {
    let dir = std::env::temp_dir().join(format!("vv-newer-db-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("vv.db");
    {
        let mut conn = Connection::open(&path).unwrap();
        apply_migrations(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (name) VALUES ('9999_from_the_future.sql')",
            [],
        )
        .unwrap();
    }
    let pool = super::build_pool(&path);
    let backups = dir.join("backups");
    let err = super::migrate(&pool, &backups).await.unwrap_err();
    drop(pool);
    let msg = err.to_string();
    let backups_written = backups.exists();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(msg.contains("newer Vigilant Vole"), "{msg}");
    assert!(msg.contains(&backups.display().to_string()), "{msg}");
    assert!(err.backup.is_none());
    assert!(!backups_written, "no backup for a refused database");
}

#[test]
fn backups_taken_in_the_same_second_get_distinct_names() {
    let (conn, _) = upgrade(legacy_db());
    let dir = std::env::temp_dir().join(format!("vv-backup-twice-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let first = super::backup_before_migration(&conn, &dir, "0102_x").unwrap();
    let second = super::backup_before_migration(&conn, &dir, "0102_x").unwrap();
    let files = std::fs::read_dir(&dir).unwrap().count();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_ne!(first, second);
    assert_eq!(files, 2);
}

/// `sqlite_master` rows and columns of the kit's core tables.
fn core_objects(conn: &Connection) -> Vec<String> {
    let mut out: Vec<String> = conn
        .prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_master
             WHERE tbl_name IN ('users', 'api_tokens', 'settings') ORDER BY type, name",
        )
        .unwrap()
        .query_map([], |r| {
            Ok(format!(
                "{} {} {} {:?}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for table in ["users", "api_tokens", "settings"] {
        let cols: Vec<String> = conn
            .prepare(&format!("SELECT * FROM pragma_table_xinfo('{table}')"))
            .unwrap()
            .query_map([], |r| {
                Ok(format!(
                    "{table}: {} {} {} {} {:?} {} {}",
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        out.extend(cols);
        let fks: Vec<String> = conn
            .prepare(&format!("SELECT * FROM pragma_foreign_key_list('{table}')"))
            .unwrap()
            .query_map([], |r| {
                Ok(format!(
                    "{table} fk: {:?} {:?} {:?} {:?}",
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(6)?
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        out.extend(fks);
    }
    out
}

/// The kit's core set creates users, api_tokens and settings exactly as our
/// migrations did, so adopting it (0104) instead of running it is right and
/// a product that starts from it has the same tables.
#[test]
fn the_core_set_builds_our_users_tokens_and_settings_tables() {
    let ours = test_conn();
    let core = Connection::open_in_memory().unwrap();
    for (_, sql) in super::MIGRATIONS.core {
        core.execute_batch(sql).unwrap();
    }
    let expected = core_objects(&ours);
    assert!(expected.len() > 20, "{expected:#?}");
    assert_eq!(core_objects(&core), expected);
}

#[test]
fn a_fresh_database_adopts_the_core_set_after_our_migrations() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CONNECTION_PRAGMAS).unwrap();
    let ran = apply_migrations(&mut conn).unwrap();
    assert!(ran.contains(&"0104_adopt_kit_core.sql".to_string()));
    // The adoption records kit/0001 only; the passkeys' core migration
    // (vole-kit 0.4) and the sealed secrets' record (0.5) run after it.
    assert_eq!(
        ran.iter()
            .rev()
            .take(3)
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "kit/0004_sealed_secrets.sql",
            "kit/0003_passkeys.sql",
            "0106_agent_checked_monitors.sql"
        ]
    );
    assert!(!ran.contains(&"kit/0001_core.sql".to_string()), "{ran:?}");
    assert!(applied(&conn).contains(&"kit/0001_core.sql".to_string()));
}

/// What 0.6.x knew: our migrations up to 0103 and no core set. It must
/// refuse a database this release migrated.
#[test]
fn an_older_release_refuses_a_database_with_the_adopted_core_set() {
    fn up_to_0103() -> Vec<(String, String)> {
        super::embedded_migrations()
            .into_iter()
            .filter(|(name, _)| name.as_str() < "0104")
            .collect()
    }
    fn no_fixups(_: &mut Connection) -> Result<(), super::MigrationError> {
        Ok(())
    }
    static RELEASE_0_6: super::MigrationSet = super::MigrationSet {
        core: &[],
        adopt_core: None,
        migrations: up_to_0103,
        fk_off_marker: "-- vv:foreign_keys=off",
        baseline: None,
        after: no_fixups,
    };
    let mut conn = test_conn();
    let err = crate::kit::db::migrate::apply_migrations_with(&mut conn, &RELEASE_0_6).unwrap_err();
    assert!(
        matches!(&err, super::MigrationError::NewerSchema { known, .. } if known == "0103_subscriber_tokens"),
        "{err:?}"
    );
}

/// The newer-schema guard counts unknown names, not how they sort:
/// the `kit/…` rows sort after every one of ours.
#[test]
fn the_core_row_does_not_hide_a_newer_migration() {
    let mut conn = test_conn();
    let names = applied(&conn);
    assert_eq!(
        names.last().map(String::as_str),
        Some("kit/0004_sealed_secrets.sql")
    );
    assert!(apply_migrations(&mut conn).unwrap().is_empty());
    conn.execute(
        "INSERT INTO schema_migrations (name) VALUES ('0110_from_the_future.sql')",
        [],
    )
    .unwrap();
    let err = apply_migrations(&mut conn).unwrap_err();
    assert!(
        matches!(&err, super::MigrationError::NewerSchema { schema, known, .. }
            if schema == "0110_from_the_future" && known == "0106_agent_checked_monitors"),
        "{err:?}"
    );
}
