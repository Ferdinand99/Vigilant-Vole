//! Upgrade safety: databases saved by every released version
//! (`tests/fixtures/upgrade/<version>/`, made by
//! `tools/upgrade-fixtures/generate.sh`) are upgraded with this build's
//! migrations and must keep every row, end up with exactly the schema of a
//! fresh database, and work: the server starts on them and the seeded
//! credentials, tokens and pages still do what they did.
//!
//! Also covers running the upgrade twice, a migration that fails half-way,
//! and an older build refusing a database from a newer one.

mod common;

use std::collections::BTreeMap;

use common::{
    DataDir, Fixture, Server, applied_migrations, columns, count, embedded_migrations, fixtures,
    open_ro, rows, schema, tables,
};
use rusqlite::Connection;
use vigilant_vole::db;

/// Releases that must have a fixture.
const RELEASES: &[&str] = &["v0.1.0", "v0.2.0", "v0.3.0", "v0.4.0"];

/// Columns a migration rewrites on purpose: `(table, column)`. Every other
/// column of every row must survive the upgrade unchanged.
const REWRITTEN: &[(&str, &str)] = &[];

/// Tables a later migration replaces or drops.
const REPLACED_TABLES: &[&str] = &[];

/// Upgrades a copy of the fixture with the startup migration path.
async fn upgrade(f: &Fixture) -> (DataDir, db::MigrationReport) {
    let dir = DataDir::with_fixture(f);
    let report = migrate(&dir)
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", f.version));
    (dir, report)
}

async fn migrate(dir: &DataDir) -> Result<db::MigrationReport, db::UpgradeError> {
    let pool = db::build_pool(&dir.db());
    let result = db::migrate(&pool, &dir.backups()).await;
    pool.close();
    common::wait_closed(&dir.db()).await;
    result
}

fn fresh_db() -> (DataDir, Connection) {
    let dir = DataDir::new("fresh");
    let mut conn = Connection::open(dir.db()).unwrap();
    db::apply_migrations(&mut conn).unwrap();
    (dir, conn)
}

#[test]
fn every_release_has_a_small_fixture() {
    let found: Vec<String> = fixtures().into_iter().map(|f| f.version).collect();
    for release in RELEASES {
        assert!(
            found.iter().any(|v| v == release),
            "no fixture for {release} (found {found:?})"
        );
    }
    for f in fixtures() {
        let size = std::fs::metadata(&f.db).unwrap().len();
        assert!(
            size < 2 * 1024 * 1024,
            "{}: fixture is {size} bytes",
            f.version
        );
        assert_eq!(
            format!("v{}", f.manifest["version"].as_str().unwrap()),
            f.version,
            "manifest version"
        );
        // The fixture is what the manifest describes.
        let conn = open_ro(&f.db);
        assert_eq!(applied_migrations(&conn), f.migrations(), "{}", f.version);
        for (table, expected) in f.manifest["counts"].as_object().unwrap() {
            assert_eq!(
                count(&conn, table),
                expected.as_i64().unwrap(),
                "{} {table}",
                f.version
            );
        }
    }
}

#[tokio::test]
async fn upgrade_applies_every_pending_migration_in_order_after_a_backup() {
    let all = embedded_migrations();
    for f in fixtures() {
        let (dir, report) = upgrade(&f).await;
        let had = f.migrations();
        let pending: Vec<String> = all.iter().filter(|m| !had.contains(m)).cloned().collect();
        assert_eq!(report.applied, pending, "{}: applied in order", f.version);

        let conn = open_ro(&dir.db());
        assert_eq!(
            applied_migrations(&conn),
            all,
            "{}: schema_migrations",
            f.version
        );

        // The pre-migration copy is the release's database, untouched.
        let backups = dir.backup_files();
        if pending.is_empty() {
            assert!(
                backups.is_empty(),
                "{}: nothing to migrate, no backup",
                f.version
            );
            continue;
        }
        assert_eq!(backups.len(), 1, "{}: {backups:?}", f.version);
        assert_eq!(report.backup.as_ref(), Some(&backups[0]));
        let name = backups[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let first = pending[0].trim_end_matches(".sql");
        assert!(
            name.starts_with(&format!("vigilant-vole-before-{first}-")),
            "{name}"
        );
        let copy = open_ro(&backups[0]);
        assert_eq!(
            applied_migrations(&copy),
            had,
            "{}: backup schema",
            f.version
        );
        let diff = common::diff_databases(&copy, &open_ro(&f.db));
        assert!(
            diff.is_empty(),
            "{}: backup differs from the release DB: {diff:?}",
            f.version
        );
    }
}

#[tokio::test]
async fn upgraded_schema_matches_a_fresh_database() {
    let (_fresh_dir, fresh) = fresh_db();
    let expected = schema(&fresh);
    for f in fixtures() {
        let (dir, _) = upgrade(&f).await;
        let upgraded = schema(&open_ro(&dir.db()));
        let missing: Vec<_> = expected.iter().filter(|o| !upgraded.contains(o)).collect();
        let extra: Vec<_> = upgraded.iter().filter(|o| !expected.contains(o)).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{}: schema differs from a fresh database\nmissing or different: {missing:#?}\nunexpected: {extra:#?}",
            f.version
        );
    }
}

#[tokio::test]
async fn upgrade_keeps_every_row_and_column() {
    for f in fixtures() {
        let (dir, _) = upgrade(&f).await;
        let before = open_ro(&f.db);
        let after = open_ro(&dir.db());
        let after_tables = tables(&after);
        let mut problems = Vec::new();
        for table in tables(&before) {
            if table == "schema_migrations" || REPLACED_TABLES.contains(&table.as_str()) {
                continue;
            }
            if !after_tables.contains(&table) {
                problems.push(format!("table {table} is gone"));
                continue;
            }
            let after_cols = columns(&after, &table);
            let cols: Vec<String> = columns(&before, &table)
                .into_iter()
                .filter(|c| !REWRITTEN.contains(&(table.as_str(), c.as_str())))
                .collect();
            if let Some(gone) = cols.iter().find(|c| !after_cols.contains(c)) {
                problems.push(format!("{table}.{gone} is gone"));
                continue;
            }
            let (old, new) = (rows(&before, &table, &cols), rows(&after, &table, &cols));
            if old.len() != new.len() {
                problems.push(format!(
                    "{table}: {} rows before, {} after",
                    old.len(),
                    new.len()
                ));
                continue;
            }
            for (id, values) in &old {
                match new.get(id) {
                    None => problems.push(format!("{table} row {id} is gone")),
                    Some(v) if v != values => {
                        for (i, c) in cols.iter().enumerate() {
                            if v[i] != values[i] {
                                problems.push(format!(
                                    "{table} row {id} {c}: {:?} -> {:?}",
                                    values[i], v[i]
                                ));
                            }
                        }
                    }
                    Some(_) => {}
                }
            }
            if let Some(expected) = f.manifest["counts"][&table].as_i64() {
                assert_eq!(count(&after, &table), expected, "{} {table}", f.version);
            }
        }
        assert!(
            problems.is_empty(),
            "{}: data changed by the upgrade:\n{}",
            f.version,
            problems.join("\n")
        );
    }
}

/// The seeded data, read back through the app's own data layer.
#[tokio::test]
async fn upgraded_data_is_usable() {
    use vigilant_vole::auth::{totp, verify_password};
    use vigilant_vole::db::{
        agents, incidents, maintenance, monitors, notifications, settings, status_pages, tags,
        users,
    };

    for f in fixtures() {
        let v = f.version.clone();
        let (dir, _) = upgrade(&f).await;
        let conn = open_ro(&dir.db());

        // Users, roles, passwords, two-factor authentication.
        let admin_password = f.credential("/admin/password");
        let user_password = f.credential("/user_password");
        for u in f.manifest["values"]["users"].as_array().unwrap() {
            let name = u["username"].as_str().unwrap();
            let user = users::get_user_by_username(&conn, name)
                .unwrap()
                .unwrap_or_else(|| panic!("{v}: user {name}"));
            assert_eq!(
                user.role.as_str(),
                u["role"].as_str().unwrap(),
                "{v}: {name} role"
            );
            assert_eq!(user.active, u["active"] == 1, "{v}: {name} active");
            assert_eq!(user.totp_enabled, u["totp"] == 1, "{v}: {name} 2FA");
            let password = if name == "admin" {
                &admin_password
            } else {
                &user_password
            };
            assert!(
                verify_password(&user.password_hash, password),
                "{v}: {name}'s password no longer verifies"
            );
        }
        let eddie = users::get_user_by_username(&conn, "eddie")
            .unwrap()
            .unwrap();
        assert_eq!(
            eddie.language.as_deref(),
            Some("nb"),
            "{v}: eddie's language"
        );
        let tf = users::get_totp(&conn, eddie.id).unwrap().unwrap();
        assert!(tf.enabled, "{v}");
        assert_eq!(
            tf.secret.as_deref(),
            Some(f.credential("/totp/secret").as_str()),
            "{v}: TOTP secret"
        );
        let recovery = f.credential("/totp/recovery_code");
        assert!(
            totp::find_recovery_code(&tf.recovery_codes, &recovery).is_some(),
            "{v}: recovery code"
        );

        // Monitors: every one loads, with its secrets.
        let all = monitors::list_monitors(&conn).unwrap();
        assert_eq!(all.len() as i64, f.count("monitors"), "{v}");
        for m in f.manifest["values"]["monitors"].as_array().unwrap() {
            let name = m["name"].as_str().unwrap();
            let got = all
                .iter()
                .find(|x| x.name == name)
                .unwrap_or_else(|| panic!("{v}: monitor {name}"));
            assert_eq!(
                got.monitor_type.as_str(),
                m["type"].as_str().unwrap(),
                "{v}: {name}"
            );
            assert_eq!(got.active, m["active"] == 1, "{v}: {name} active");
        }
        let by_name = |name: &str| {
            all.iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("{v}: monitor {name}"))
        };
        let website = by_name("Website");
        assert!(
            website
                .http_headers
                .as_deref()
                .unwrap_or_default()
                .contains("Bearer header-secret-123"),
            "{v}: header secret"
        );
        assert_eq!(
            website.parent_id,
            f.monitor_id("Core services"),
            "{v}: group"
        );
        assert_eq!(
            (
                website.expiry_days,
                website.resend_interval,
                website.escalation_minutes
            ),
            (21, 3, 15),
            "{v}"
        );
        let mut website_tags: Vec<(String, String)> = tags::tags_for_monitor(&conn, website.id)
            .unwrap()
            .into_iter()
            .map(|t| (t.name, t.value))
            .collect();
        website_tags.sort();
        assert_eq!(
            website_tags,
            vec![
                ("env".into(), "prod".into()),
                ("production".into(), String::new())
            ],
            "{v}"
        );
        let mut links = notifications::monitor_channel_links(&conn, website.id).unwrap();
        links.sort();
        let webhook = f.manifest["ids"]["webhook"].as_i64().unwrap();
        let escalation = f.manifest["ids"]["escalation"].as_i64().unwrap();
        assert_eq!(
            links,
            vec![(webhook, false), (escalation, true)],
            "{v}: notification links"
        );

        let basic = by_name("Basic auth API");
        assert_eq!(
            (
                basic.basic_auth_user.as_deref(),
                basic.basic_auth_pass.as_deref(),
                basic.auth_method.as_str()
            ),
            (Some("apiuser"), Some("basic-pass-456"), "basic"),
            "{v}: basic auth"
        );
        if f.monitor_id("OAuth API").is_some() {
            let oauth = by_name("OAuth API");
            assert_eq!(
                oauth.oauth_client_secret.as_deref(),
                Some("oauth-secret-789"),
                "{v}"
            );
            assert_eq!(oauth.oauth_client_auth, "client_secret_post", "{v}");
        }
        if f.monitor_id("mTLS API").is_some() {
            let mtls = by_name("mTLS API");
            assert!(
                mtls.tls_client_key
                    .as_deref()
                    .unwrap_or_default()
                    .contains("PRIVATE KEY"),
                "{v}: client key"
            );
        }
        if f.monitor_id("RADIUS").is_some() {
            assert_eq!(
                by_name("RADIUS").secret.as_deref(),
                Some("radius-shared-secret"),
                "{v}"
            );
        }
        if f.monitor_id("Postgres").is_some() {
            assert_eq!(
                by_name("Postgres").basic_auth_pass.as_deref(),
                Some("pg-pass"),
                "{v}"
            );
        }
        let agent_site = by_name("Agent-checked site");
        assert!(
            agent_site.run_on_agents && agent_site.confirm_with_agents,
            "{v}: agent settings"
        );
        assert_eq!(
            by_name("Nightly backup").push_token.as_deref(),
            Some(f.credential("/push_token").as_str()),
            "{v}"
        );

        // Notification channels, with their secrets.
        let channels = notifications::list_channels(&conn).unwrap();
        assert_eq!(
            channels.len() as i64,
            f.count("notification_channels"),
            "{v}"
        );
        let hook = channels.iter().find(|c| c.id == webhook).unwrap();
        let config: serde_json::Value = serde_json::from_str(&hook.config).unwrap();
        assert_eq!(
            config["url"], f.manifest["values"]["channel_secret"],
            "{v}: webhook URL"
        );
        assert!(hook.is_default && hook.active, "{v}");
        assert!(
            !channels.iter().find(|c| c.id == escalation).unwrap().active,
            "{v}"
        );
        let telegram = channels
            .iter()
            .find(|c| c.kind == "telegram")
            .expect("telegram channel");
        assert!(
            telegram.config.contains("123456:ABC-fixture-secret"),
            "{v}: bot token"
        );

        // Status pages, sections, incidents.
        let page = status_pages::get_by_slug(&conn, "fixture")
            .unwrap()
            .unwrap();
        assert_eq!(page.title, "Fixture status");
        assert_eq!(
            page.custom_domain.as_deref(),
            Some("status.fixture.test"),
            "{v}"
        );
        assert_eq!(
            page.custom_css.as_deref(),
            Some(".hero { color: #123456; }"),
            "{v}"
        );
        assert_eq!(
            page.footer_text.as_deref(),
            Some("Run by the fixture team"),
            "{v}"
        );
        let sections: Vec<String> = status_pages::sections(&conn, page.id)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(sections, ["Website", "Infrastructure", "Databases"], "{v}");
        let mut titles: Vec<String> = incidents::list_for_page(&conn, page.id)
            .unwrap()
            .into_iter()
            .map(|i| i.title)
            .collect();
        titles.sort();
        assert_eq!(
            titles,
            ["Database maintenance overran", "Elevated error rates"],
            "{v}"
        );
        if f.manifest["migrations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "0098_status_page_extras.sql")
        {
            let internal = status_pages::get_by_slug(&conn, "internal")
                .unwrap()
                .unwrap();
            let hash = internal.password_hash.as_deref().expect("page password");
            assert!(
                verify_password(hash, &f.credential("/page_password")),
                "{v}: page password"
            );
        }

        // Maintenance windows.
        let windows = maintenance::list(&conn).unwrap();
        assert_eq!(windows.len(), 4, "{v}");
        let manual = windows
            .iter()
            .find(|w| w.title == "Manual switch work")
            .unwrap();
        assert!(manual.active, "{v}");
        assert_eq!(
            maintenance::monitor_ids(&conn, manual.id).unwrap(),
            vec![f.monitor_id("TCP closed port").unwrap()],
            "{v}"
        );
        assert_eq!(
            maintenance::status_page_ids(&conn, manual.id).unwrap(),
            vec![page.id],
            "{v}"
        );
        let weekly = windows
            .iter()
            .find(|w| w.title == "Weekly patching")
            .unwrap();
        assert_eq!(
            (
                weekly.time_from.as_deref(),
                weekly.time_to.as_deref(),
                weekly.timezone.as_str()
            ),
            (Some("22:00"), Some("23:30"), "Europe/Oslo"),
            "{v}"
        );
        assert!(
            !windows
                .iter()
                .find(|w| w.title == "Nightly backups")
                .unwrap()
                .active,
            "{v}"
        );

        // Agents and settings.
        let agent_list = agents::list_agents(&conn).unwrap();
        let mut names: Vec<String> = agent_list.into_iter().map(|a| a.agent.name).collect();
        names.sort();
        assert_eq!(names, ["edge-bergen", "edge-oslo"], "{v}");
        assert!(
            agents::find_by_token(&conn, &f.credential("/agent_token"))
                .unwrap()
                .is_some(),
            "{v}: agent token"
        );
        let s = settings::load(&conn).unwrap();
        assert_eq!(
            (
                s.site_name.as_str(),
                s.retention_days,
                s.base_url.as_deref(),
                s.timezone.as_str(),
                s.metrics_enabled,
                s.badges_public
            ),
            (
                "Vole Fixture",
                120,
                Some("http://vole.fixture.test"),
                "Europe/Oslo",
                true,
                true
            ),
            "{v}: settings"
        );

        // History.
        for table in [
            "heartbeats",
            "monitor_events",
            "heartbeat_daily",
            "api_tokens",
        ] {
            assert_eq!(count(&conn, table), f.count(table), "{v}: {table}");
        }
        assert!(
            f.count("heartbeats") > 0 && f.count("monitor_events") > 0,
            "{v}: the fixture has history"
        );
    }
}

/// The server starts on each upgraded database and the seeded credentials,
/// tokens and pages work over HTTP.
#[tokio::test]
async fn server_runs_on_every_upgraded_database() {
    for f in fixtures() {
        let v = f.version.clone();
        let dir = DataDir::with_fixture(&f);
        let server = Server::start(&dir)
            .await
            .unwrap_or_else(|e| panic!("{v}: {e}"));
        let mut c = server.client();

        let health = c.get("/healthz").await;
        assert_eq!((health.status, health.text().as_str()), (200, "ok"), "{v}");

        // The admin signs in and sees the dashboard and a monitor.
        let res = c
            .login("admin", &f.credential("/admin/password"), None)
            .await;
        assert_eq!(res.location(), "/", "{v}");
        let dash = c.get("/").await;
        assert_eq!(dash.status, 200, "{v}: dashboard");
        assert!(
            dash.text().contains("Website"),
            "{v}: dashboard lists monitors"
        );
        let website = f.monitor_id("Website").unwrap();
        assert_eq!(
            c.get(&format!("/monitors/{website}")).await.status,
            200,
            "{v}: monitor page"
        );
        for page in [
            "/status-pages",
            "/maintenance",
            "/settings/users",
            "/settings/backup",
            "/settings/agents",
        ] {
            assert_eq!(c.get(page).await.status, 200, "{v}: {page}");
        }

        // Public status page, by slug and by its custom domain.
        let mut public = server.client();
        let page = public.get("/status/fixture").await;
        assert_eq!(page.status, 200, "{v}: status page");
        let html = page.text();
        assert!(
            html.contains("Fixture status") && html.contains("#123456"),
            "{v}: status page content"
        );
        assert!(
            html.contains("Elevated error rates"),
            "{v}: pinned incident"
        );
        let by_domain = public
            .get_with("/", &[("host", "status.fixture.test")])
            .await;
        assert_eq!(by_domain.status, 200, "{v}: custom domain");
        assert!(
            by_domain.text().contains("Fixture status"),
            "{v}: custom domain serves the page"
        );

        // API tokens from the release still work, with their scopes.
        let admin_token = f.credential("/admin_token");
        let me = c
            .send(
                c.request(reqwest::Method::GET, "/api/v1/me")
                    .bearer_auth(&admin_token),
            )
            .await;
        assert_eq!(me.status, 200, "{v}: admin token");
        assert_eq!(
            (
                me.json()["username"].as_str(),
                me.json()["is_admin"].as_bool()
            ),
            (Some("admin"), Some(true)),
            "{v}"
        );
        let viewer_token = f.credential("/viewer_token");
        let me = c
            .send(
                c.request(reqwest::Method::GET, "/api/v1/me")
                    .bearer_auth(&viewer_token),
            )
            .await;
        assert_eq!(
            me.json()["username"].as_str(),
            Some("vera"),
            "{v}: viewer token"
        );
        let write = c
            .send(
                c.request(reqwest::Method::POST, "/api/v1/tags")
                    .bearer_auth(&viewer_token)
                    .json(&serde_json::json!({"name": "nope"})),
            )
            .await;
        assert_eq!(write.status, 403, "{v}: read tokens stay read-only");
        let list = c
            .send(
                c.request(reqwest::Method::GET, "/api/v1/monitors?limit=500")
                    .bearer_auth(&admin_token),
            )
            .await;
        assert_eq!(
            list.json()["data"].as_array().map(Vec::len),
            Some(f.count("monitors") as usize),
            "{v}: API lists every monitor"
        );

        // A push to the seeded push token is accepted and recorded.
        let push_id = f.monitor_id("Nightly backup").unwrap();
        let beats = || {
            open_ro(&dir.db())
                .query_row(
                    "SELECT COUNT(*) FROM heartbeats WHERE monitor_id = ?1",
                    [push_id],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        };
        let before = beats();
        let push = public
            .get(&format!(
                "/api/push/{}?status=up&msg=after-upgrade",
                f.credential("/push_token")
            ))
            .await;
        assert_eq!(push.status, 200, "{v}: push: {}", push.text());
        assert_eq!(beats(), before + 1, "{v}: push recorded");

        // The agent token still authenticates and gets its monitor.
        let agent = public
            .send(
                public
                    .request(reqwest::Method::GET, "/api/agent/v1/monitors")
                    .bearer_auth(f.credential("/agent_token")),
            )
            .await;
        assert_eq!(agent.status, 200, "{v}: agent token");
        let agent_site = f.monitor_id("Agent-checked site").unwrap();
        assert!(
            agent.json()["monitors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == agent_site),
            "{v}: agent monitors"
        );

        // Eddie signs in with his password and a code from his TOTP secret.
        let mut eddie = server.client();
        let res = eddie
            .login(
                "eddie",
                &f.credential("/user_password"),
                Some(&f.credential("/totp/secret")),
            )
            .await;
        assert_eq!(
            (res.status, res.location().as_str()),
            (303, "/"),
            "{v}: 2FA login: {}",
            res.text()
        );
        assert_eq!(eddie.get("/settings/security").await.status, 200, "{v}");

        // Dave stays disabled.
        let mut dave = server.client();
        let res = dave
            .post_form(
                "/login",
                &[
                    ("username", "dave"),
                    ("password", &f.credential("/user_password")),
                ],
            )
            .await;
        assert_ne!(res.status, 303, "{v}: a disabled user signed in");

        server.stop().await;
    }
}

#[tokio::test]
async fn upgrading_twice_changes_nothing() {
    for f in fixtures() {
        let (dir, _) = upgrade(&f).await;
        let snapshot = DataDir::new("twice");
        common::copy_db(&dir.db(), &snapshot.db());
        let backups = dir.backup_files();

        let report = migrate(&dir).await.unwrap();
        assert!(
            report.applied.is_empty(),
            "{}: {:?}",
            f.version,
            report.applied
        );
        assert!(
            report.backup.is_none(),
            "{}: no backup when nothing is pending",
            f.version
        );
        assert_eq!(dir.backup_files(), backups, "{}", f.version);
        let diff = common::diff_databases(&open_ro(&snapshot.db()), &open_ro(&dir.db()));
        assert!(
            diff.is_empty(),
            "{}: second run changed data: {diff:?}",
            f.version
        );
    }
}

/// A migration that fails half-way leaves nothing of itself behind, keeps the
/// ones before it, says which one failed and where the backup is, and the
/// next start (once the cause is fixed) finishes the upgrade.
#[tokio::test]
async fn failed_migration_is_rolled_back_and_retried() {
    const FAILING: &str = "0102_status_page_subscribers.sql";
    let f = fixtures()
        .into_iter()
        .rev()
        .find(|f| !f.migrations().iter().any(|m| m == FAILING))
        .expect("a fixture older than 0102");
    let dir = DataDir::with_fixture(&f);
    {
        // 0102 first adds columns to status_pages, then creates this table:
        // a table already in the way makes it fail after the first statements ran.
        let conn = Connection::open(dir.db()).unwrap();
        conn.execute_batch("CREATE TABLE status_page_subscribers (id INTEGER PRIMARY KEY);")
            .unwrap();
    }

    let err = migrate(&dir).await.expect_err("0102 must fail");
    let msg = err.to_string();
    assert!(msg.contains(FAILING), "{msg}");
    let backup = err.backup.clone().expect("a backup was made first");
    assert!(msg.contains(&backup.display().to_string()), "{msg}");
    let before: Vec<String> = embedded_migrations()
        .into_iter()
        .filter(|m| m.as_str() < FAILING)
        .collect();
    assert_eq!(
        err.at.as_deref(),
        before.last().map(String::as_str),
        "{msg}"
    );
    assert!(
        msg.contains(&format!("the database is at {}", before.last().unwrap())),
        "{msg}"
    );

    {
        let conn = open_ro(&dir.db());
        assert_eq!(
            applied_migrations(&conn),
            before,
            "earlier migrations stay applied"
        );
        assert!(
            !columns(&conn, "status_pages").contains(&"subscribe_enabled".to_string()),
            "the failed migration's first statements were rolled back"
        );
        // The backup is the release's database.
        assert_eq!(applied_migrations(&open_ro(&backup)), f.migrations());
    }

    // The server refuses to start on it, with the same explanation.
    let refused = Server::start(&dir).await.err().expect("startup must fail");
    assert!(refused.to_string().contains(FAILING), "{refused}");

    // Fix the cause; the next start finishes the upgrade with the data intact.
    {
        let conn = Connection::open(dir.db()).unwrap();
        conn.execute_batch("DROP TABLE status_page_subscribers;")
            .unwrap();
    }
    let report = migrate(&dir).await.unwrap();
    assert_eq!(report.applied.first().map(String::as_str), Some(FAILING));
    let conn = open_ro(&dir.db());
    assert_eq!(applied_migrations(&conn), embedded_migrations());
    assert_eq!(count(&conn, "monitors"), f.count("monitors"));
    assert_eq!(count(&conn, "heartbeats"), f.count("heartbeats"));
}

/// An older build must not run against a database a newer build migrated.
#[tokio::test]
async fn database_from_a_newer_version_is_refused() {
    let f = fixtures().pop().unwrap();
    let (dir, _) = upgrade(&f).await;
    {
        let conn = Connection::open(dir.db()).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (name) VALUES ('9999_from_the_future.sql')",
            [],
        )
        .unwrap();
    }
    let snapshot: BTreeMap<String, i64> = {
        let conn = open_ro(&dir.db());
        tables(&conn)
            .into_iter()
            .map(|t| {
                let n = count(&conn, &t);
                (t, n)
            })
            .collect()
    };
    let backups = dir.backup_files();

    let err = Server::start(&dir).await.err().expect("startup must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("created by a newer Vigilant Vole (schema 9999_from_the_future)"),
        "{msg}"
    );
    assert!(msg.contains(&dir.backups().display().to_string()), "{msg}");
    assert_eq!(dir.backup_files(), backups, "nothing written");
    let conn = open_ro(&dir.db());
    let after: BTreeMap<String, i64> = tables(&conn)
        .into_iter()
        .map(|t| {
            let n = count(&conn, &t);
            (t, n)
        })
        .collect();
    assert_eq!(after, snapshot, "the database is untouched");
}
