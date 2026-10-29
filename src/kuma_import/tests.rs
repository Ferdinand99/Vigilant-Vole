use std::path::PathBuf;

use super::*;
use crate::config_io::Action;
use crate::db::{monitors as db_monitors, notifications as db_notifications, test_conn};

/// A small database with Uptime Kuma 1.23's table layout (only the columns
/// we read, plus a few we don't, to prove unknown columns are ignored).
const KUMA_SCHEMA: &str = r#"
CREATE TABLE monitor (
    id INTEGER PRIMARY KEY, name VARCHAR(150), active BOOLEAN DEFAULT 1, user_id INTEGER, interval INTEGER DEFAULT 20,
    url TEXT, type VARCHAR(20), weight INTEGER, hostname VARCHAR(255), port INTEGER, created_date DATETIME,
    keyword VARCHAR(255), maxretries INTEGER DEFAULT 0, ignore_tls BOOLEAN DEFAULT 0, upside_down BOOLEAN DEFAULT 0,
    maxredirects INTEGER DEFAULT 10, accepted_statuscodes_json TEXT DEFAULT '["200-299"]', dns_resolve_type VARCHAR(5),
    dns_resolve_server VARCHAR(255), dns_last_result VARCHAR(255), retry_interval INTEGER DEFAULT 0, push_token VARCHAR(20),
    method TEXT DEFAULT 'GET', body TEXT, headers TEXT, basic_auth_user TEXT, basic_auth_pass TEXT, docker_host INTEGER,
    docker_container VARCHAR(255), proxy_id INTEGER, expiry_notification BOOLEAN DEFAULT 1, mqtt_topic TEXT,
    resend_interval INTEGER DEFAULT 0, packet_size INTEGER DEFAULT 56, game VARCHAR(255), parent INTEGER,
    description TEXT, http_body_encoding VARCHAR(25), json_path TEXT, expected_value VARCHAR(255),
    invert_keyword BOOLEAN DEFAULT 0, timeout DOUBLE DEFAULT 0, kafka_producer_brokers TEXT
);
CREATE TABLE tag (id INTEGER PRIMARY KEY, name VARCHAR(255), color VARCHAR(255), created_date DATETIME);
CREATE TABLE monitor_tag (id INTEGER PRIMARY KEY, monitor_id INTEGER, tag_id INTEGER, value TEXT);
CREATE TABLE notification (id INTEGER PRIMARY KEY, name VARCHAR(255), config TEXT, active BOOLEAN DEFAULT 1, user_id INTEGER, is_default BOOLEAN DEFAULT 0);
CREATE TABLE monitor_notification (id INTEGER PRIMARY KEY, monitor_id INTEGER, notification_id INTEGER);
CREATE TABLE status_page (id INTEGER PRIMARY KEY, slug VARCHAR(255), title VARCHAR(255), description TEXT, icon VARCHAR(255),
    theme VARCHAR(30), published BOOLEAN DEFAULT 1, search_engine_index BOOLEAN DEFAULT 1, show_tags BOOLEAN DEFAULT 0,
    password VARCHAR, created_date DATETIME, modified_date DATETIME, footer_text TEXT, custom_css TEXT,
    show_powered_by BOOLEAN DEFAULT 1, google_analytics_tag_id VARCHAR);
CREATE TABLE status_page_cname (id INTEGER PRIMARY KEY, status_page_id INTEGER, domain VARCHAR);
CREATE TABLE "group" (id INTEGER PRIMARY KEY, name VARCHAR(255), created_date DATETIME, public BOOLEAN DEFAULT 0,
    active BOOLEAN DEFAULT 1, weight BOOLEAN DEFAULT 1000, status_page_id INTEGER);
CREATE TABLE monitor_group (id INTEGER PRIMARY KEY, monitor_id INTEGER, group_id INTEGER, weight BOOLEAN DEFAULT 1000, send_url BOOLEAN DEFAULT 0);
CREATE TABLE maintenance (id INTEGER PRIMARY KEY, title VARCHAR(150), description TEXT, user_id INTEGER, active BOOLEAN DEFAULT 1,
    strategy VARCHAR(50) DEFAULT 'single', start_date DATETIME, end_date DATETIME, start_time TIME, end_time TIME,
    weekdays VARCHAR(250) DEFAULT '[]', days_of_month TEXT DEFAULT '[]', interval_day INTEGER, cron TEXT, timezone VARCHAR(255), duration INTEGER);
CREATE TABLE monitor_maintenance (id INTEGER PRIMARY KEY, monitor_id INTEGER, maintenance_id INTEGER);
CREATE TABLE maintenance_status_page (id INTEGER PRIMARY KEY, status_page_id INTEGER, maintenance_id INTEGER);
CREATE TABLE docker_host (id INTEGER PRIMARY KEY, user_id INTEGER, docker_daemon VARCHAR(255), docker_type VARCHAR(255), name VARCHAR(255));
CREATE TABLE proxy (id INTEGER PRIMARY KEY, user_id INTEGER, protocol VARCHAR(10), host VARCHAR(255), port SMALLINT, auth BOOLEAN,
    username VARCHAR(255), password VARCHAR(255), active BOOLEAN DEFAULT 1, "default" BOOLEAN DEFAULT 0, created_date DATETIME);
"#;

const KUMA_DATA: &str = r##"
INSERT INTO docker_host VALUES (1, 1, '/var/run/docker.sock', 'socket', 'Local');
INSERT INTO proxy VALUES (1, 1, 'http', 'proxy.internal', 3128, 1, 'user', 'p@ss', 1, 0, NULL);
INSERT INTO monitor (id, name, type, url, interval, retry_interval, maxretries, timeout, accepted_statuscodes_json, headers,
    basic_auth_user, basic_auth_pass, ignore_tls, expiry_notification, proxy_id, method, body, http_body_encoding, parent, description)
    VALUES (1, 'Website', 'http', 'https://example.com', 60, 30, 3, 48, '["200-299","301"]', '{"X-Token":"abc"}',
    'bot', 'secret', 1, 1, 1, 'POST', '{"ping":true}', 'json', 5, 'Main site');
INSERT INTO monitor (id, name, type, hostname, port, interval, timeout) VALUES (2, 'Postgres port', 'port', 'db.internal', 5432, 30, 48);
INSERT INTO monitor (id, name, type, push_token, interval) VALUES (3, 'Nightly job', 'push', 'kumaPushTok1', 86400);
INSERT INTO monitor (id, name, type, hostname) VALUES (4, 'Broker', 'mongodb', 'mongo.internal');
INSERT INTO monitor (id, name, type) VALUES (5, 'Backend', 'group');
INSERT INTO monitor (id, name, type, url, keyword, invert_keyword) VALUES (6, 'Shop', 'keyword', 'https://shop.example.com', 'Out of stock', 1);
INSERT INTO monitor (id, name, type, docker_container, docker_host, active) VALUES (7, 'Web container', 'docker', 'web', 1, 0);
INSERT INTO monitor (id, name, type, hostname, dns_resolve_type, dns_resolve_server, port) VALUES (8, 'DNS', 'dns', 'example.com', 'MX', '1.1.1.1', 53);
INSERT INTO monitor (id, name, type, url, json_path, expected_value) VALUES (9, 'JSON', 'json-query', 'https://api.example.com', 'status', 'ok');
INSERT INTO monitor (id, name, type, url) VALUES (10, 'Website', 'http', 'https://example.org');
INSERT INTO tag VALUES (1, 'Production', '#059669', NULL);
INSERT INTO tag VALUES (2, 'Team', '#DC2626', NULL);
INSERT INTO monitor_tag VALUES (1, 1, 1, ''), (2, 1, 2, 'web'), (3, 2, 1, NULL);
INSERT INTO notification VALUES (1, 'Discord ops', '{"type":"discord","discordWebhookUrl":"https://discord.com/api/webhooks/1/abc","discordUsername":"Kuma"}', 1, 1, 1);
INSERT INTO notification VALUES (2, 'Mail', '{"type":"smtp","smtpHost":"smtp.example.com","smtpPort":465,"smtpSecure":true,"smtpUsername":"u","smtpPassword":"pw","smtpFrom":"kuma@example.com","smtpTo":"ops@example.com"}', 1, 1, 0);
INSERT INTO notification VALUES (3, 'Line', '{"type":"line","lineChannelAccessToken":"x"}', 1, 1, 0);
INSERT INTO notification VALUES (4, 'Broken slack', '{"type":"slack"}', 1, 1, 0);
INSERT INTO monitor_notification VALUES (1, 1, 1), (2, 1, 2), (3, 2, 3), (4, 3, 4);
INSERT INTO status_page (id, slug, title, description, theme, published, show_tags, footer_text, show_powered_by)
    VALUES (1, 'Acme', 'Acme status', 'All systems', 'dark', 1, 1, 'Footer', 0);
INSERT INTO status_page_cname VALUES (1, 1, 'status.acme.test');
INSERT INTO "group" (id, name, status_page_id, weight) VALUES (1, 'Web', 1, 2), (2, 'Data', 1, 1);
INSERT INTO monitor_group (id, monitor_id, group_id, weight) VALUES (1, 1, 1, 1), (2, 6, 1, 2), (3, 2, 2, 1), (4, 4, 2, 2);
INSERT INTO maintenance (id, title, strategy, start_time, end_time, weekdays, timezone, active)
    VALUES (1, 'Patch night', 'recurring-weekday', '02:00', '03:30', '[0,3]', 'Europe/Oslo', 1);
INSERT INTO maintenance (id, title, strategy, cron, duration, timezone) VALUES (2, 'Cron thing', 'cron', '0 2 * * 0', 90, 'Europe/Oslo');
INSERT INTO maintenance (id, title, strategy, cron, duration) VALUES (4, 'Bad cron', 'cron', '0 0 0 0 0 0 0', 30);
INSERT INTO maintenance (id, title, strategy, start_time, end_time, start_date, interval_day, timezone)
    VALUES (5, 'Every third day', 'recurring-interval', '01:00', '02:00', '2026-10-01 00:00:00', 3, 'Europe/Oslo');
INSERT INTO maintenance (id, title, strategy, start_time, end_time, days_of_month, timezone)
    VALUES (6, 'Month end', 'recurring-day-of-month', '22:00', '23:00', '[1,15,"lastDay1","lastDay2"]', 'Europe/Oslo');
INSERT INTO maintenance (id, title, strategy, start_time, end_time, interval_day)
    VALUES (7, 'Interval without date', 'recurring-interval', '01:00', '02:00', 2);
INSERT INTO maintenance (id, title, strategy, start_date, end_date, timezone)
    VALUES (3, 'Move', 'single', '2026-11-01 10:00:00', '2026-11-01 12:00:00', 'Europe/Oslo');
INSERT INTO monitor_maintenance VALUES (1, 2, 1), (2, 3, 3);
INSERT INTO maintenance_status_page VALUES (1, 1, 1);
"##;

fn kuma_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vv-kuma-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("kuma.db");
    let _ = std::fs::remove_file(&path);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(KUMA_SCHEMA).unwrap();
    conn.execute_batch(KUMA_DATA).unwrap();
    path
}

fn monitor<'a>(import: &'a KumaImport, key: &str) -> &'a MonitorDoc {
    import
        .doc
        .monitors
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.key() == key)
        .unwrap_or_else(|| panic!("no monitor {key}"))
}

#[test]
fn sqlite_monitors_map_to_our_types_and_options() {
    let import = read_file(&kuma_db("types")).unwrap();
    assert_eq!(import.source, "sqlite");
    let web = monitor(&import, "Website");
    assert_eq!(web.monitor_type, "http");
    assert_eq!(web.target, "https://example.com");
    assert_eq!(web.parent.as_deref(), Some("Backend"));
    let o = &web.options;
    assert_eq!(o["interval_seconds"], 60);
    assert_eq!(o["retry_interval_seconds"], 30);
    assert_eq!(o["retries"], 3);
    assert_eq!(o["timeout_seconds"], 48);
    assert_eq!(o["accepted_status_codes"], "200-299,301");
    assert_eq!(o["http_headers"]["X-Token"], "abc");
    assert_eq!(o["basic_auth_pass"], "secret");
    assert_eq!(o["ignore_tls"], true);
    assert_eq!(o["http_method"], "POST");
    assert_eq!(o["http_body_encoding"], "json");
    assert_eq!(o["proxy_url"], "http://user:p%40ss@proxy.internal:3128");
    assert_eq!(web.tags.as_ref().unwrap().len(), 2);
    assert_eq!(
        web.notifications.as_ref().unwrap().len(),
        2,
        "Discord + Mail (Line is unsupported)"
    );

    assert_eq!(monitor(&import, "Postgres port").monitor_type, "tcp");
    assert_eq!(monitor(&import, "Postgres port").target, "db.internal:5432");
    assert_eq!(
        monitor(&import, "Postgres port").options["timeout_seconds"],
        30,
        "timeout capped at the interval"
    );
    assert_eq!(
        monitor(&import, "Nightly job").push_token.as_deref(),
        Some("kumaPushTok1")
    );
    let shop = monitor(&import, "Shop");
    assert_eq!(
        (shop.monitor_type.as_str(), &shop.options["keyword_invert"]),
        ("keyword", &serde_json::json!(true))
    );
    let docker = monitor(&import, "Web container");
    assert_eq!(docker.options["docker_host"], "/var/run/docker.sock");
    assert_eq!(docker.options["active"], false);
    assert_eq!(monitor(&import, "DNS").options["dns_record_type"], "MX");
    assert_eq!(monitor(&import, "JSON").options["json_path"], "$.status");
    // The second "Website" gets a distinct key.
    assert_eq!(monitor(&import, "Website #10").name, "Website");
    // Unsupported types are listed with a reason.
    assert!(
        import
            .skipped
            .iter()
            .any(|s| s.name == "Broker" && s.reason.contains("no connection string"))
    );
}

#[test]
fn tags_notifications_pages_and_maintenance_map() {
    let import = read_file(&kuma_db("rest")).unwrap();
    let tags = import.doc.tags.as_ref().unwrap();
    assert_eq!(
        tags.iter()
            .map(|t| (t.name.as_str(), t.color.as_str()))
            .collect::<Vec<_>>(),
        [("Production", "mint"), ("Team", "red")]
    );
    let channels = import.doc.notification_channels.as_ref().unwrap();
    let mail = channels.iter().find(|c| c.name == "Mail").unwrap();
    assert_eq!(mail.kind, "email");
    assert_eq!(mail.config["security"], "tls");
    assert_eq!(mail.config["port"], 465);
    let discord = channels.iter().find(|c| c.name == "Discord ops").unwrap();
    assert!(discord.is_default);
    assert_eq!(
        discord.config["url"],
        "https://discord.com/api/webhooks/1/abc"
    );
    assert!(
        import
            .skipped
            .iter()
            .any(|s| s.kind == "notification" && s.name == "Line")
    );

    let page = &import.doc.status_pages.as_ref().unwrap()[0];
    assert_eq!(page.slug, "acme", "slugs are lowercased");
    assert_eq!(page.theme, "dark");
    assert_eq!(page.custom_domain.as_deref(), Some("status.acme.test"));
    assert!(!page.show_powered_by);
    let sections = page.sections.as_ref().unwrap();
    assert_eq!(sections[0].name, "Data", "groups ordered by weight");
    assert_eq!(
        sections[0].monitors,
        vec!["Postgres port".to_string()],
        "skipped monitors are dropped"
    );
    assert_eq!(
        sections[1].monitors,
        vec!["Website".to_string(), "Shop".to_string()]
    );

    let windows = import.doc.maintenance.as_ref().unwrap();
    let patch = windows.iter().find(|w| w.title == "Patch night").unwrap();
    assert_eq!(patch.strategy, "weekly");
    assert_eq!(patch.weekdays.as_deref(), Some("7,3"));
    assert_eq!(patch.time_from.as_deref(), Some("02:00"));
    assert_eq!(patch.monitors, vec!["Postgres port".to_string()]);
    assert_eq!(patch.status_pages, vec!["acme".to_string()]);
    let once = windows.iter().find(|w| w.title == "Move").unwrap();
    assert_eq!(
        once.start_at.as_deref(),
        Some("2026-11-01 09:00:00"),
        "local Oslo time converted to UTC"
    );
    // Cron, every-N-days and day-of-month schedules come across.
    let cron = windows.iter().find(|w| w.title == "Cron thing").unwrap();
    assert_eq!(
        (
            cron.strategy.as_str(),
            cron.cron.as_deref(),
            cron.duration_minutes
        ),
        ("cron", Some("0 2 * * 0"), Some(90))
    );
    assert_eq!(cron.timezone, "Europe/Oslo");
    let third = windows
        .iter()
        .find(|w| w.title == "Every third day")
        .unwrap();
    assert_eq!(
        (third.strategy.as_str(), third.interval_days),
        ("interval", Some(3))
    );
    assert_eq!(
        third.start_at.as_deref(),
        Some("2026-09-30 22:00:00"),
        "the first date is Oslo local midnight"
    );
    assert_eq!(
        (third.time_from.as_deref(), third.time_to.as_deref()),
        (Some("01:00"), Some("02:00"))
    );
    let month = windows.iter().find(|w| w.title == "Month end").unwrap();
    assert_eq!(
        (month.strategy.as_str(), month.month_days.as_deref()),
        ("monthly", Some("1,15,-1,-2"))
    );
    // What can't be mapped is still listed with a reason.
    assert!(
        import
            .skipped
            .iter()
            .any(|s| s.name == "Bad cron" && s.reason.contains("cron")),
        "{:?}",
        import.skipped
    );
    assert!(
        import
            .skipped
            .iter()
            .any(|s| s.name == "Interval without date"),
        "{:?}",
        import.skipped
    );
    assert!(
        windows
            .iter()
            .all(|w| w.title != "Bad cron" && w.title != "Interval without date")
    );
}

#[test]
fn planning_skips_invalid_entries_and_applies_cleanly() {
    let mut import = read_file(&kuma_db("apply")).unwrap();
    let mut conn = test_conn();
    let plan = plan(
        &conn,
        &crate::notification::tests_support::test_key(),
        &mut import,
    )
    .unwrap();
    assert!(plan.is_ok(), "{:?}", plan.errors);
    // The Slack channel without a webhook URL fails validation and is skipped, not fatal.
    assert!(
        import.skipped.iter().any(|s| s.name == "Broken slack"),
        "{:?}",
        import.skipped
    );
    assert_eq!(
        plan.action_for(config_io::KIND_MONITOR, "Website"),
        Some(Action::Create)
    );

    let applied = config_io::apply(
        &mut conn,
        &crate::notification::tests_support::test_key(),
        &import.doc,
        Mode::Merge,
    )
    .unwrap()
    .unwrap();
    assert!(!applied.monitor_ids.is_empty());
    let monitors = db_monitors::list_monitors(&conn).unwrap();
    let push = monitors
        .iter()
        .find(|m| m.name == "Nightly job")
        .unwrap_or_else(|| panic!("{:?}", import.skipped));
    assert_eq!(
        push.push_token.as_deref(),
        Some("kumaPushTok1"),
        "push URLs keep working"
    );
    let web = monitors
        .iter()
        .find(|m| m.target == "https://example.com")
        .unwrap();
    assert_eq!(
        db_notifications::monitor_channel_links(&conn, web.id)
            .unwrap()
            .len(),
        2
    );
    assert!(
        monitors
            .iter()
            .any(|m| m.name == "Web container" && !m.active)
    );

    // Importing again changes nothing.
    let mut again = read_file(&kuma_db("again")).unwrap();
    let second = super::plan(
        &conn,
        &crate::notification::tests_support::test_key(),
        &mut again,
    )
    .unwrap();
    assert!(
        !second.has_changes(),
        "{:?}",
        second
            .items
            .iter()
            .filter(|i| i.action != Action::Unchanged)
            .collect::<Vec<_>>()
    );
}

#[test]
fn legacy_json_backup_is_read() {
    let json = r##"{
      "version": "1.23.0",
      "notificationList": [{"id": 1, "name": "Hook", "config": "{\"type\":\"webhook\",\"webhookURL\":\"https://hooks.example.com/x\",\"webhookContentType\":\"json\"}", "active": 1, "isDefault": false}],
      "monitorList": [
        {"id": 1, "name": "Site", "type": "http", "url": "https://example.com", "interval": 120, "retryInterval": 120, "maxretries": 1,
         "accepted_statuscodes": ["200-299"], "notificationIDList": {"1": true}, "ignoreTls": false, "upsideDown": true,
         "tags": [{"id": 3, "monitor_id": 1, "tag_id": 7, "value": "eu", "name": "region", "color": "#2563EB"}]},
        {"id": 2, "name": "Pinger", "type": "ping", "hostname": "192.0.2.1", "interval": 60}
      ]
    }"##;
    let import = read_json(json).unwrap();
    let site = monitor(&import, "Site");
    assert_eq!(site.options["upside_down"], true);
    assert!(
        !site.options.contains_key("retry_interval_seconds"),
        "same as interval → default"
    );
    assert_eq!(site.notifications.as_ref().unwrap()[0].name(), "Hook");
    assert_eq!(
        site.tags.as_ref().unwrap()[0],
        TagRef::Full {
            name: "region".into(),
            value: "eu".into()
        }
    );
    assert_eq!(import.doc.tags.as_ref().unwrap()[0].color, "blue");
    assert_eq!(monitor(&import, "Pinger").target, "192.0.2.1");
    assert!(read_json("{\"hello\": 1}").is_err());
    assert!(read_json("not json").is_err());
}

#[test]
fn files_are_sniffed_by_content() {
    assert!(is_sqlite(b"SQLite format 3\0rest"));
    assert!(!is_sqlite(b"{\"monitorList\":[]}"));
    let dir = std::env::temp_dir().join(format!("vv-kuma-sniff-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("backup.json");
    std::fs::write(&path, r#"{"monitorList": [], "notificationList": []}"#).unwrap();
    assert_eq!(read_file(&path).unwrap().source, "json");
    std::fs::write(&path, [0u8, 159, 146, 150]).unwrap();
    assert!(read_file(&path).is_err());
}

#[test]
fn an_oversized_backup_is_refused_before_it_is_converted() {
    let list = vec!["{\"id\":1,\"name\":\"x\",\"type\":\"push\"}"; MAX_ROWS + 1].join(",");
    let err = read_json(&format!("{{\"monitorList\":[{list}]}}"))
        .err()
        .unwrap();
    assert!(err.contains("too large"), "{err}");
    let list = ["{\"id\":1,\"name\":\"x\",\"type\":\"push\"}"; 3].join(",");
    assert!(read_json(&format!("{{\"monitorList\":[{list}]}}")).is_ok());

    let dir = std::env::temp_dir().join(format!("vv-kuma-big-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.json");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_JSON_BYTES + 1).unwrap();
    let err = read_file(&path).err().unwrap();
    assert!(err.contains("larger than"), "{err}");

    let db = dir.join("many.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(KUMA_SCHEMA).unwrap();
    conn.execute_batch(&format!(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i <= {MAX_ROWS})
         INSERT INTO monitor (id, name, type) SELECT i, 'm' || i, 'push' FROM n;"
    ))
    .unwrap();
    drop(conn);
    let err = read_sqlite(&db).err().unwrap();
    assert!(err.contains("monitor table"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kuma_auth_methods_map_to_ours() {
    let json = r##"{
      "version": "2.0.0",
      "monitorList": [
        {"id": 1, "name": "Basic", "type": "http", "url": "https://a.example.com", "authMethod": "basic",
         "basic_auth_user": "bot", "basic_auth_pass": "pw"},
        {"id": 2, "name": "Legacy basic", "type": "http", "url": "https://b.example.com", "basic_auth_user": "old", "basic_auth_pass": "pw"},
        {"id": 3, "name": "OAuth", "type": "http", "url": "https://c.example.com", "authMethod": "oauth2-cc",
         "oauth_auth_method": "client_secret_post", "oauth_token_url": "https://idp.example.com/token",
         "oauth_client_id": "vole", "oauth_client_secret": "shh", "oauth_scopes": "read write", "oauth_audience": "api"},
        {"id": 4, "name": "OAuth basic", "type": "keyword", "keyword": "ok", "url": "https://d.example.com", "authMethod": "oauth2-cc",
         "oauth_auth_method": "client_secret_basic", "oauth_token_url": "https://idp.example.com/token", "oauth_client_id": "v", "oauth_client_secret": "s"},
        {"id": 5, "name": "Mtls", "type": "http", "url": "https://e.example.com", "authMethod": "mtls",
         "tlsCa": "CA-PEM", "tlsCert": "CERT-PEM", "tlsKey": "KEY-PEM"},
        {"id": 6, "name": "Ntlm", "type": "http", "url": "https://f.example.com", "authMethod": "ntlm",
         "basic_auth_user": "DOMAIN\\user", "basic_auth_pass": "pw"},
        {"id": 7, "name": "No auth", "type": "http", "url": "https://g.example.com"}
      ]
    }"##;
    let import = read_json(json).unwrap();
    let o = |name: &str| monitor(&import, name).options.clone();

    let basic = o("Basic");
    assert_eq!(
        (
            &basic["auth_method"],
            &basic["basic_auth_user"],
            &basic["basic_auth_pass"]
        ),
        (&"basic".into(), &"bot".into(), &"pw".into())
    );
    assert_eq!(o("Legacy basic")["auth_method"], "basic");

    let oauth = o("OAuth");
    assert_eq!(oauth["auth_method"], "oauth2");
    assert_eq!(oauth["oauth_token_url"], "https://idp.example.com/token");
    assert_eq!(
        (&oauth["oauth_client_id"], &oauth["oauth_client_secret"]),
        (&"vole".into(), &"shh".into())
    );
    assert_eq!(
        (&oauth["oauth_scope"], &oauth["oauth_audience"]),
        (&"read write".into(), &"api".into())
    );
    assert_eq!(oauth["oauth_client_auth"], "client_secret_post");
    assert_eq!(o("OAuth basic")["oauth_client_auth"], "client_secret_basic");

    let mtls = o("Mtls");
    assert_eq!(mtls["auth_method"], "mtls");
    assert_eq!(
        (
            &mtls["tls_client_cert"],
            &mtls["tls_client_key"],
            &mtls["tls_ca_cert"]
        ),
        (&"CERT-PEM".into(), &"KEY-PEM".into(), &"CA-PEM".into())
    );

    // NTLM has no equivalent: no credentials are carried over, and it is reported.
    let ntlm = o("Ntlm");
    assert!(
        !ntlm.contains_key("auth_method")
            && !ntlm.contains_key("basic_auth_user")
            && !ntlm.contains_key("basic_auth_pass")
    );
    assert!(
        import
            .warnings
            .iter()
            .any(|w| w.contains("Ntlm") && w.contains("NTLM")),
        "{:?}",
        import.warnings
    );
    assert!(!o("No auth").contains_key("auth_method"));
    assert!(
        !import
            .warnings
            .iter()
            .any(|w| w.contains("shh") || w.contains("KEY-PEM"))
    );
}

#[test]
fn service_monitors_map_to_ours() {
    let json = r##"{
      "version": "2.0.0",
      "monitorList": [
        {"id": 1, "name": "PG", "type": "postgres", "databaseConnectionString": "postgres://vole:p%40ss@db.local:5433/app?sslmode=require", "databaseQuery": "SELECT count(*) FROM jobs"},
        {"id": 2, "name": "My", "type": "mysql", "databaseConnectionString": "mysql://root:pw@maria.local:3306/shop"},
        {"id": 3, "name": "Cache", "type": "redis", "databaseConnectionString": "rediss://:secret@cache.local:6380/2"},
        {"id": 4, "name": "Bad conn", "type": "postgres", "databaseConnectionString": "Server=x;Database=y"},
        {"id": 5, "name": "Broker", "type": "mqtt", "hostname": "mqtts://broker.local", "port": 8883, "mqttTopic": "home/status", "mqttSuccessMessage": "online", "mqttUsername": "u", "mqttPassword": "p"},
        {"id": 6, "name": "Ws broker", "type": "mqtt", "hostname": "ws://broker.local", "port": 9001},
        {"id": 7, "name": "Kafka", "type": "kafka-producer", "kafkaProducerBrokers": ["k1:9092", "k2:9092"], "kafkaProducerTopic": "orders",
         "kafkaProducerSaslOptions": {"mechanism": "plain"}},
        {"id": 8, "name": "Radius", "type": "radius", "hostname": "radius.local", "port": 1812, "radiusUsername": "u", "radiusPassword": "p", "radiusSecret": "s3"},
        {"id": 9, "name": "Router", "type": "snmp", "hostname": "router.local", "port": 161, "snmpOid": ".1.3.6.1.2.1.1.3.0", "snmpVersion": "2c", "radiusPassword": "public", "expectedValue": "up"},
        {"id": 10, "name": "V3", "type": "snmp", "hostname": "router.local", "snmpVersion": "3"},
        {"id": 11, "name": "Mail", "type": "smtp", "hostname": "mail.local", "smtpSecurity": "secure"},
        {"id": 12, "name": "MQ", "type": "rabbitmq", "rabbitmqNodes": ["https://mq.local:15671/"], "rabbitmqUsername": "guest", "rabbitmqPassword": "guest"},
        {"id": 13, "name": "Mongo", "type": "mongodb", "databaseConnectionString": "mongodb://x"}
      ]
    }"##;
    let import = read_json(json).unwrap();
    let doc = |name: &str| monitor(&import, name);

    let pg = doc("PG");
    assert_eq!(
        (pg.monitor_type.as_str(), pg.target.as_str()),
        ("postgres", "postgres://db.local:5433/app?sslmode=require")
    );
    assert_eq!(
        (
            &pg.options["basic_auth_user"],
            &pg.options["basic_auth_pass"]
        ),
        (&"vole".into(), &"p@ss".into()),
        "credentials leave the address and are decoded"
    );
    assert_eq!(doc("My").target, "mysql://maria.local:3306/shop");
    let cache = doc("Cache");
    assert_eq!(
        (cache.monitor_type.as_str(), cache.target.as_str()),
        ("redis", "rediss://cache.local:6380/2")
    );
    assert!(
        !cache.options.contains_key("basic_auth_user")
            && cache.options["basic_auth_pass"] == "secret"
    );

    let mqtt = doc("Broker");
    assert_eq!(mqtt.target, "mqtts://broker.local:8883/home/status");
    assert_eq!(
        (&mqtt.options["keyword"], &mqtt.options["basic_auth_user"]),
        (&"online".into(), &"u".into())
    );

    let kafka = doc("Kafka");
    assert_eq!(kafka.target, "kafka://k1:9092/orders");
    let radius = doc("Radius");
    assert_eq!(
        (radius.target.as_str(), &radius.options["secret"]),
        ("radius://radius.local:1812", &"s3".into())
    );
    let snmp = doc("Router");
    assert_eq!(
        snmp.target,
        "snmp://router.local:161/1.3.6.1.2.1.1.3.0?version=2c"
    );
    assert_eq!(
        (&snmp.options["secret"], &snmp.options["keyword"]),
        (&"public".into(), &"up".into())
    );
    assert_eq!(doc("Mail").target, "mail.local:465");
    let mq = doc("MQ");
    assert_eq!(
        (mq.monitor_type.as_str(), mq.target.as_str()),
        ("http", "https://mq.local:15671/api/health/checks/alarms/")
    );
    assert_eq!(mq.options["auth_method"], "basic");

    let skipped: Vec<&str> = import.skipped.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(doc("Mongo").target, "mongodb://x");
    for name in ["Bad conn", "Ws broker", "V3"] {
        assert!(skipped.contains(&name), "{name} is skipped: {skipped:?}");
    }
    let warned = |name: &str| {
        import
            .warnings
            .iter()
            .any(|w| w.starts_with(&format!("“{name}”")))
    };
    assert!(
        warned("PG") && warned("Kafka") && warned("MQ"),
        "{:?}",
        import.warnings
    );
}

#[test]
fn mqtt_topics_survive_and_secrets_keep_their_spaces() {
    let json = r##"{
      "version": "2.0.0",
      "monitorList": [
        {"id": 1, "name": "All", "type": "mqtt", "hostname": "broker.local", "port": 1883, "mqttTopic": "home/#", "mqttPassword": " p w ", "mqttUsername": "u"},
        {"id": 2, "name": "One", "type": "mqtt", "hostname": "broker.local", "port": 1883, "mqttTopic": "a/+/b"},
        {"id": 3, "name": "Odd", "type": "mqtt", "hostname": "broker.local", "port": 1883, "mqttTopic": "sensors/100%?x"},
        {"id": 4, "name": "Radius", "type": "radius", "hostname": "radius.local", "port": 1812, "radiusUsername": "u", "radiusPassword": " rp ", "radiusSecret": " s3 "},
        {"id": 5, "name": "Router", "type": "snmp", "hostname": "router.local", "snmpOid": "1.3.6.1.2.1.1.3.0", "radiusPassword": " public "},
        {"id": 6, "name": "MQ", "type": "rabbitmq", "rabbitmqNodes": ["https://mq.local:15671/"], "rabbitmqUsername": "guest", "rabbitmqPassword": " guest "}
      ],
      "notificationList": [
        {"id": 1, "name": "Mail", "config": "{\"type\":\"smtp\",\"smtpHost\":\"mail.local\",\"smtpPort\":587,\"smtpTo\":\"a@b.c\",\"smtpUsername\":\"u\",\"smtpPassword\":\" mail pw \"}"}
      ]
    }"##;
    let import = read_json(json).unwrap();
    let doc = |name: &str| monitor(&import, name);
    // The topic reads back exactly as Kuma had it.
    for (name, topic) in [
        ("All", "home/#"),
        ("One", "a/+/b"),
        ("Odd", "sensors/100%?x"),
    ] {
        let target = doc(name).target.clone();
        assert_eq!(
            crate::monitor::checkers::mqtt::topic(&target).as_deref(),
            Some(topic),
            "{target}"
        );
        assert!(
            crate::api::monitors::form::validate_target(
                crate::db::models::MonitorType::Mqtt,
                &target
            )
            .is_ok(),
            "{target}"
        );
    }
    // Secrets are copied as stored, spaces included.
    assert_eq!(doc("All").options["basic_auth_pass"], " p w ");
    let radius = doc("Radius");
    assert_eq!(
        (
            &radius.options["basic_auth_pass"],
            &radius.options["secret"]
        ),
        (&" rp ".into(), &" s3 ".into())
    );
    assert_eq!(doc("Router").options["secret"], " public ");
    assert_eq!(doc("MQ").options["basic_auth_pass"], " guest ");
    let mail = import
        .doc
        .notification_channels
        .as_ref()
        .unwrap()
        .iter()
        .find(|c| c.name == "Mail")
        .unwrap();
    assert_eq!(mail.config["password"], " mail pw ", "{:?}", mail.config);
}

#[test]
fn sqlserver_and_mongodb_connection_strings_map_to_targets_and_logins() {
    let json = r##"{
      "version": "2.0.0",
      "monitorList": [
        {"id": 1, "name": "SQL", "type": "sqlserver", "databaseConnectionString": "Server=db.local,1434;Database=app;User Id=sa;Password=\" p;w \";Encrypt=true;TrustServerCertificate=Yes;Connection Timeout=30", "databaseQuery": "SELECT COUNT(*) FROM orders"},
        {"id": 2, "name": "SQL plain", "type": "sqlserver", "databaseConnectionString": "Data Source=tcp:sql.local\\SQLEXPRESS;Initial Catalog=My Db;UID=app;PWD={pa}}ss};Encrypt=False"},
        {"id": 3, "name": "SQL windows", "type": "sqlserver", "databaseConnectionString": "Server=sql.local;Integrated Security=SSPI"},
        {"id": 4, "name": "Mongo", "type": "mongodb", "databaseConnectionString": "mongodb://vole:p%40ss%20@m1.local:27017,m2.local:27018/app?replicaSet=rs0&authSource=admin"},
        {"id": 5, "name": "Atlas", "type": "mongodb", "databaseConnectionString": "mongodb+srv://reader:pw@cluster0.example.net/?retryWrites=true"},
        {"id": 6, "name": "Not mongo", "type": "mongodb", "databaseConnectionString": "postgres://db.local/app"},
        {"id": 7, "name": "SQL none", "type": "sqlserver"}
      ]
    }"##;
    let import = read_json(json).unwrap();
    let doc = |name: &str| monitor(&import, name);

    let sql = doc("SQL");
    assert_eq!(
        (sql.monitor_type.as_str(), sql.target.as_str()),
        (
            "sqlserver",
            "sqlserver://db.local:1434/app?encrypt=true&trustServerCertificate=true"
        )
    );
    assert_eq!(
        (
            &sql.options["basic_auth_user"],
            &sql.options["basic_auth_pass"]
        ),
        (&"sa".into(), &" p;w ".into()),
        "quoted passwords keep their spaces"
    );
    let plain = doc("SQL plain");
    assert_eq!(plain.target, "sqlserver://sql.local/My%20Db?encrypt=false");
    assert_eq!(
        (
            &plain.options["basic_auth_user"],
            &plain.options["basic_auth_pass"]
        ),
        (&"app".into(), &"pa}ss".into())
    );

    let mongo = doc("Mongo");
    assert_eq!(
        (mongo.monitor_type.as_str(), mongo.target.as_str()),
        (
            "mongodb",
            "mongodb://m1.local:27017,m2.local:27018/app?replicaSet=rs0&authSource=admin"
        )
    );
    assert_eq!(
        (
            &mongo.options["basic_auth_user"],
            &mongo.options["basic_auth_pass"]
        ),
        (&"vole".into(), &"p@ss ".into())
    );
    let atlas = doc("Atlas");
    assert_eq!(
        atlas.target,
        "mongodb+srv://cluster0.example.net/?retryWrites=true"
    );
    assert_eq!(atlas.options["basic_auth_user"], "reader");

    // Every imported target passes the form's validation.
    use crate::db::models::MonitorType as Kind;
    for (name, kind) in [
        ("SQL", Kind::SqlServer),
        ("SQL plain", Kind::SqlServer),
        ("Mongo", Kind::Mongodb),
        ("Atlas", Kind::Mongodb),
    ] {
        assert!(
            crate::api::monitors::form::validate_target(kind, &doc(name).target).is_ok(),
            "{name}"
        );
    }
    let reason = |name: &str| {
        import
            .skipped
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.reason.clone())
            .unwrap_or_default()
    };
    assert!(reason("SQL windows").contains("Windows authentication"));
    assert!(reason("Not mongo").contains("postgres://"));
    assert!(reason("SQL none").contains("no connection string"));
    assert!(
        import
            .warnings
            .iter()
            .any(|w| w.starts_with("“SQL”") && w.contains("SELECT COUNT")),
        "{:?}",
        import.warnings
    );
    assert!(
        import
            .warnings
            .iter()
            .any(|w| w.starts_with("“SQL plain”") && w.contains("SQLEXPRESS")),
        "{:?}",
        import.warnings
    );
}
