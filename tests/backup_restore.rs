//! Backup and restore drills on a running instance with real data (the
//! newest release fixture, upgraded):
//!
//! 1. Settings → Backup: download the database snapshot, upload it as the
//!    restore for the next start, restart. The data is exactly the snapshot
//!    again and the database it replaced is kept in `backups/`.
//! 2. Config as code: export everything (with secrets), import it into a
//!    fresh instance; a second dry run then plans no changes at all.
//!
//! The manual version of these drills is in docs/upgrading.md.

mod common;

use common::{DataDir, Fixture, Server, count, diff_databases, fixtures, open_ro};
use reqwest::Method;
use serde_json::Value;

fn newest() -> Fixture {
    fixtures().pop().expect("an upgrade fixture")
}

/// `multipart/form-data` with one file field (reqwest is built without its
/// multipart feature).
fn multipart_file(field: &str, name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "vv-drill-boundary-7d2f1c";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\n\
         Content-Type: application/vnd.sqlite3\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// A copy of `db` with the channel and two-factor secrets opened with `key`:
/// what a database backup holds.
fn opened_copy(
    db: &std::path::Path,
    key: &axum_extra::extract::cookie::Key,
    to: &std::path::Path,
) -> rusqlite::Connection {
    common::copy_db(db, to);
    let conn = rusqlite::Connection::open(to).unwrap();
    vigilant_vole::kit::auth::totp::open_stored(&conn, key).unwrap();
    vigilant_vole::notification::sealed::open_stored(&conn, key).unwrap();
    conn
}

/// The stored configs of every channel, joined (to look for a secret).
fn channel_configs(db: &std::path::Path) -> String {
    let conn = open_ro(db);
    let mut stmt = conn
        .prepare("SELECT config FROM notification_channels ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join(
            "
",
        )
}

fn monitor_names(db: &std::path::Path) -> Vec<String> {
    let conn = open_ro(db);
    let mut stmt = conn
        .prepare("SELECT name FROM monitors ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn database_backup_restores_after_a_restart() {
    let f = newest();
    let dir = DataDir::with_fixture(&f);
    let server = Server::start(&dir).await.unwrap();
    let token = f.credential("/admin_token");
    let mut admin = server.client();
    admin
        .login("admin", &f.credential("/admin/password"), None)
        .await;

    // Settings → Backup → Download database.
    let page = admin.get("/settings/backup").await;
    assert_eq!(page.status, 200);
    let res = admin.get("/settings/backup/database").await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(
        res.headers["content-type"], "application/vnd.sqlite3",
        "snapshot content type"
    );
    assert!(
        res.headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment; filename=\"vigilant-vole-backup-"),
    );
    let downloads = DataDir::new("downloads");
    let snapshot = downloads.path().join("backup.db");
    std::fs::write(&snapshot, &res.body).unwrap();
    assert!(
        dir.path()
            .join("tmp")
            .read_dir()
            .map(|d| d.count())
            .unwrap_or(0)
            == 0,
        "the server-side snapshot file is cleaned up"
    );

    // The snapshot is the live database at that moment, with the channel
    // secrets opened (they are sealed with this instance's key, which the
    // snapshot doesn't hold), so it restores on any instance.
    let secret = f.manifest["values"]["channel_secret"].as_str().unwrap();
    let key = server.state.session_key.clone();
    let live = opened_copy(&dir.db(), &key, &downloads.path().join("live-opened.db"));
    let diff = diff_databases(&open_ro(&snapshot), &live);
    assert!(
        diff.is_empty(),
        "snapshot differs from the live database: {diff:?}"
    );
    assert_eq!(
        diff_databases(&open_ro(&snapshot), &open_ro(&dir.db())),
        vec![
            "notification_channels: rows differ".to_string(),
            "users: rows differ".to_string()
        ],
        "only the sealed secrets differ"
    );
    // The fixture's two-factor user: sealed in the live database, opened in
    // the snapshot.
    let totp_secret = f.manifest["credentials"]["totp"]["secret"]
        .as_str()
        .unwrap();
    let stored_totp = |db: &std::path::Path| -> Vec<String> {
        let conn = open_ro(db);
        let mut stmt = conn
            .prepare("SELECT totp_secret FROM users WHERE totp_secret IS NOT NULL")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(stored_totp(&snapshot), [totp_secret]);
    let sealed = stored_totp(&dir.db());
    assert!(
        sealed.len() == 1 && sealed[0].starts_with("sealed:v1:"),
        "{sealed:?}"
    );
    assert!(channel_configs(&snapshot).contains(secret));
    assert!(!channel_configs(&dir.db()).contains(secret));
    for (table, expected) in f.manifest["counts"].as_object().unwrap() {
        if table == "schema_migrations" {
            continue;
        }
        assert_eq!(
            count(&open_ro(&snapshot), table),
            expected.as_i64().unwrap(),
            "snapshot {table}"
        );
    }

    // Life goes on after the backup: a monitor is deleted, a tag added.
    let website = f.monitor_id("Website").unwrap();
    let res = admin
        .send(
            admin
                .request(Method::DELETE, &format!("/api/v1/monitors/{website}"))
                .bearer_auth(&token),
        )
        .await;
    assert_eq!(res.status, 204, "{}", res.text());
    let res = admin
        .send(
            admin
                .request(Method::POST, "/api/v1/tags")
                .bearer_auth(&token)
                .json(&serde_json::json!({"name": "after-backup", "color": "red"})),
        )
        .await;
    assert_eq!(res.status, 201, "{}", res.text());

    // Upload the snapshot as the restore for the next start.
    let (content_type, body) =
        multipart_file("file", "backup.db", &std::fs::read(&snapshot).unwrap());
    let res = admin
        .send(
            admin
                .request(Method::POST, "/settings/backup/restore")
                .header("hx-request", "true")
                .header("content-type", content_type)
                .body(body),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert!(
        res.headers.get("hx-trigger").is_none(),
        "restore upload refused: {:?}",
        res.headers.get("hx-trigger")
    );
    assert!(
        dir.path().join("restore-pending.db").exists(),
        "restore is pending"
    );
    assert!(
        admin
            .get("/settings/backup")
            .await
            .text()
            .contains("restore"),
        "page mentions the pending restore"
    );

    // Restart.
    server.stop_and_release().await;
    let server = Server::start(&dir).await.unwrap();

    // The data is exactly the snapshot again, its secrets sealed again.
    let key = server.state.session_key.clone();
    let restored = opened_copy(
        &dir.db(),
        &key,
        &downloads.path().join("restored-opened.db"),
    );
    let diff = diff_databases(&open_ro(&snapshot), &restored);
    assert!(
        diff.is_empty(),
        "restored database differs from the backup: {diff:?}"
    );
    assert!(!channel_configs(&dir.db()).contains(secret));
    assert!(monitor_names(&dir.db()).contains(&"Website".to_string()));
    assert!(!dir.path().join("restore-pending.db").exists());

    // The database it replaced is kept, with the post-backup changes.
    let replaced: Vec<_> = dir
        .backup_files()
        .into_iter()
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("vigilant-vole-before-restore-")
        })
        .collect();
    assert_eq!(replaced.len(), 1, "{:?}", dir.backup_files());
    assert!(!monitor_names(&replaced[0]).contains(&"Website".to_string()));
    let tag: i64 = open_ro(&replaced[0])
        .query_row(
            "SELECT COUNT(*) FROM tags WHERE name = 'after-backup'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        tag, 1,
        "the replaced database is the one from before the restore"
    );

    // And the restored instance works.
    let mut again = server.client();
    let res = again
        .login("admin", &f.credential("/admin/password"), None)
        .await;
    assert_eq!(res.location(), "/");
    assert_eq!(again.get(&format!("/monitors/{website}")).await.status, 200);
    server.stop().await;
}

/// A backup taken by an older release is restored and upgraded on the
/// restart that swaps it in.
#[tokio::test]
async fn backup_from_the_oldest_release_restores_and_upgrades() {
    let all = fixtures();
    let (oldest, newest) = (&all[0], all.last().unwrap());
    let dir = DataDir::with_fixture(newest);
    let server = Server::start(&dir).await.unwrap();
    let mut admin = server.client();
    admin
        .login("admin", &newest.credential("/admin/password"), None)
        .await;
    let (content_type, body) =
        multipart_file("file", "old.db", &std::fs::read(&oldest.db).unwrap());
    let res = admin
        .send(
            admin
                .request(Method::POST, "/settings/backup/restore")
                .header("hx-request", "true")
                .header("content-type", content_type)
                .body(body),
        )
        .await;
    assert_eq!(res.status, 200);
    assert!(
        res.headers.get("hx-trigger").is_none(),
        "{} backup refused: {:?}",
        oldest.version,
        res.headers.get("hx-trigger")
    );
    server.stop_and_release().await;

    let server = Server::start(&dir).await.unwrap();
    let conn = open_ro(&dir.db());
    assert_eq!(
        common::applied_migrations(&conn),
        common::recorded_migrations()
    );
    for table in [
        "users",
        "monitors",
        "heartbeats",
        "notification_channels",
        "incidents",
    ] {
        assert_eq!(count(&conn, table), oldest.count(table), "{table}");
    }
    let mut c = server.client();
    let res = c
        .login("admin", &oldest.credential("/admin/password"), None)
        .await;
    assert_eq!(res.location(), "/");
    server.stop().await;
}

#[tokio::test]
async fn invalid_restore_uploads_are_refused() {
    let f = newest();
    let dir = DataDir::with_fixture(&f);
    let server = Server::start(&dir).await.unwrap();
    let mut admin = server.client();
    admin
        .login("admin", &f.credential("/admin/password"), None)
        .await;
    let (content_type, body) = multipart_file("file", "notes.db", b"definitely not sqlite");
    let res = admin
        .send(
            admin
                .request(Method::POST, "/settings/backup/restore")
                .header("hx-request", "true")
                .header("content-type", content_type)
                .body(body),
        )
        .await;
    assert_eq!(res.status, 200);
    assert!(res.headers.get("hx-trigger").is_some(), "an error toast");
    assert!(!dir.path().join("restore-pending.db").exists());
    server.stop().await;
}

/// The plan's items that aren't "unchanged", for failure messages.
fn changes(plan: &Value) -> Vec<String> {
    plan["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["action"] != "unchanged")
        .map(|i| {
            format!(
                "{} {} {} {}",
                i["action"], i["kind"], i["key"], i["changes"]
            )
        })
        .collect()
}

#[tokio::test]
async fn config_export_imports_into_a_fresh_instance_unchanged() {
    let f = newest();
    let source_dir = DataDir::with_fixture(&f);
    let source = Server::start(&source_dir).await.unwrap();
    let mut c = source.client();
    let token = f.credential("/admin_token");
    let export = c
        .send(
            c.request(Method::GET, "/api/v1/config?format=json&secrets=1")
                .bearer_auth(&token),
        )
        .await;
    assert_eq!(export.status, 200, "{}", export.text());
    let document = export.text();
    let doc: Value = serde_json::from_str(&document).unwrap();
    assert_eq!(
        doc["monitors"].as_array().map(Vec::len),
        Some(f.count("monitors") as usize)
    );

    // A fresh instance: setup, then import.
    let fresh_dir = DataDir::new("fresh-import");
    let fresh = Server::start(&fresh_dir).await.unwrap();
    let mut admin = fresh.client();
    let res = admin
        .post_form(
            "/setup",
            &[
                ("username", "owner"),
                ("password", "Fresh-Instance-Pass-1!"),
                ("confirm", "Fresh-Instance-Pass-1!"),
                ("site_name", "Fresh"),
                ("timezone", "UTC"),
            ],
        )
        .await;
    assert_eq!(res.status, 303, "setup: {}", res.text());

    let import = |c: &common::Client, dry_run: bool| {
        let path = if dry_run {
            "/api/v1/config?dry_run=1"
        } else {
            "/api/v1/config"
        };
        c.request(Method::POST, path)
            .header("content-type", "application/json")
            .body(document.clone())
    };
    let req = import(&admin, false);
    let applied = admin.send(req).await;
    assert_eq!(applied.status, 200, "{}", applied.text());
    let plan = applied.json();
    assert_eq!(plan["applied"], true, "{plan}");
    assert_eq!(plan["errors"].as_array().map(Vec::len), Some(0), "{plan}");

    // Importing the same document again changes nothing.
    let req = import(&admin, true);
    let again = admin.send(req).await;
    assert_eq!(again.status, 200, "{}", again.text());
    let plan = again.json();
    let summary = &plan["summary"];
    assert_eq!(
        (
            summary["create"].as_i64(),
            summary["update"].as_i64(),
            summary["delete"].as_i64()
        ),
        (Some(0), Some(0), Some(0)),
        "second import still plans changes: {:#?}",
        changes(&plan)
    );
    assert!(
        summary["unchanged"].as_i64().unwrap() > f.count("monitors"),
        "{summary}"
    );

    // The fresh instance exports the same document.
    let fresh_export = admin.get("/api/v1/config?format=json&secrets=1").await;
    let fresh_doc: Value = serde_json::from_str(&fresh_export.text()).unwrap();
    // A monitor's channels are listed by channel id, and ids differ between
    // instances (the import creates channels in its own order): compare
    // those lists as sets.
    let normalize = |doc: &mut Value| {
        for m in doc["monitors"].as_array_mut().into_iter().flatten() {
            for list in ["notifications", "tags"] {
                if let Some(items) = m[list].as_array_mut() {
                    items.sort_by_key(|v| v.to_string());
                }
            }
        }
    };
    let (mut doc, mut fresh_doc) = (doc, fresh_doc);
    normalize(&mut doc);
    normalize(&mut fresh_doc);
    let mut differences = Vec::new();
    for section in [
        "settings",
        "tags",
        "notification_channels",
        "monitors",
        "status_pages",
        "maintenance",
        "remote_browsers",
    ] {
        let (a, b) = (&doc[section], &fresh_doc[section]);
        match (a.as_array(), b.as_array()) {
            (Some(a), Some(b)) => {
                if a.len() != b.len() {
                    differences.push(format!(
                        "{section}: {} items exported, {} after the round trip",
                        a.len(),
                        b.len()
                    ));
                }
                for (x, y) in a.iter().zip(b) {
                    if x != y {
                        differences.push(format!("{section}:\n  before {x}\n  after  {y}"));
                    }
                }
            }
            _ if a != b => differences.push(format!("{section}:\n  before {a}\n  after  {b}")),
            _ => {}
        }
    }
    assert!(
        differences.is_empty(),
        "the round trip changed the configuration:\n{}",
        differences.join("\n")
    );

    source.stop().await;
    fresh.stop().await;
}
