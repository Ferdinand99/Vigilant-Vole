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
    assert_eq!(applied(&conn).len(), ran.len() + 1);
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
