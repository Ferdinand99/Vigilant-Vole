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
INSERT INTO monitor (id, name, type, hostname) VALUES (4, 'Broker', 'mqtt', 'mqtt.internal');
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
INSERT INTO maintenance (id, title, strategy, cron) VALUES (2, 'Cron thing', 'cron', '0 2 * * *');
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
    import.doc.monitors.as_ref().unwrap().iter().find(|m| m.key() == key).unwrap_or_else(|| panic!("no monitor {key}"))
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
    assert_eq!(web.notifications.as_ref().unwrap().len(), 2, "Discord + Mail (Line is unsupported)");

    assert_eq!(monitor(&import, "Postgres port").monitor_type, "tcp");
    assert_eq!(monitor(&import, "Postgres port").target, "db.internal:5432");
    assert_eq!(monitor(&import, "Postgres port").options["timeout_seconds"], 30, "timeout capped at the interval");
    assert_eq!(monitor(&import, "Nightly job").push_token.as_deref(), Some("kumaPushTok1"));
    let shop = monitor(&import, "Shop");
    assert_eq!((shop.monitor_type.as_str(), &shop.options["keyword_invert"]), ("keyword", &serde_json::json!(true)));
    let docker = monitor(&import, "Web container");
    assert_eq!(docker.options["docker_host"], "/var/run/docker.sock");
    assert_eq!(docker.options["active"], false);
    assert_eq!(monitor(&import, "DNS").options["dns_record_type"], "MX");
    assert_eq!(monitor(&import, "JSON").options["json_path"], "$.status");
    // The second "Website" gets a distinct key.
    assert_eq!(monitor(&import, "Website #10").name, "Website");
    // Unsupported types are listed with a reason.
    assert!(import.skipped.iter().any(|s| s.name == "Broker" && s.reason.contains("MQTT")));
}

#[test]
fn tags_notifications_pages_and_maintenance_map() {
    let import = read_file(&kuma_db("rest")).unwrap();
    let tags = import.doc.tags.as_ref().unwrap();
    assert_eq!(tags.iter().map(|t| (t.name.as_str(), t.color.as_str())).collect::<Vec<_>>(), [("Production", "mint"), ("Team", "red")]);
    let channels = import.doc.notification_channels.as_ref().unwrap();
    let mail = channels.iter().find(|c| c.name == "Mail").unwrap();
    assert_eq!(mail.kind, "email");
    assert_eq!(mail.config["security"], "tls");
    assert_eq!(mail.config["port"], 465);
    let discord = channels.iter().find(|c| c.name == "Discord ops").unwrap();
    assert!(discord.is_default);
    assert_eq!(discord.config["url"], "https://discord.com/api/webhooks/1/abc");
    assert!(import.skipped.iter().any(|s| s.kind == "notification" && s.name == "Line"));

    let page = &import.doc.status_pages.as_ref().unwrap()[0];
    assert_eq!(page.slug, "acme", "slugs are lowercased");
    assert_eq!(page.theme, "dark");
    assert_eq!(page.custom_domain.as_deref(), Some("status.acme.test"));
    assert!(!page.show_powered_by);
    let sections = page.sections.as_ref().unwrap();
    assert_eq!(sections[0].name, "Data", "groups ordered by weight");
    assert_eq!(sections[0].monitors, vec!["Postgres port".to_string()], "skipped monitors are dropped");
    assert_eq!(sections[1].monitors, vec!["Website".to_string(), "Shop".to_string()]);

    let windows = import.doc.maintenance.as_ref().unwrap();
    let patch = windows.iter().find(|w| w.title == "Patch night").unwrap();
    assert_eq!(patch.strategy, "weekly");
    assert_eq!(patch.weekdays.as_deref(), Some("7,3"));
    assert_eq!(patch.time_from.as_deref(), Some("02:00"));
    assert_eq!(patch.monitors, vec!["Postgres port".to_string()]);
    assert_eq!(patch.status_pages, vec!["acme".to_string()]);
    let once = windows.iter().find(|w| w.title == "Move").unwrap();
    assert_eq!(once.start_at.as_deref(), Some("2026-11-01 09:00:00"), "local Oslo time converted to UTC");
    assert!(import.skipped.iter().any(|s| s.name == "Cron thing"));
}

#[test]
fn planning_skips_invalid_entries_and_applies_cleanly() {
    let mut import = read_file(&kuma_db("apply")).unwrap();
    let mut conn = test_conn();
    let plan = plan(&conn, &mut import).unwrap();
    assert!(plan.is_ok(), "{:?}", plan.errors);
    // The Slack channel without a webhook URL fails validation and is skipped, not fatal.
    assert!(import.skipped.iter().any(|s| s.name == "Broken slack"), "{:?}", import.skipped);
    assert_eq!(plan.action_for(config_io::KIND_MONITOR, "Website"), Some(Action::Create));

    let applied = config_io::apply(&mut conn, &import.doc, Mode::Merge).unwrap().unwrap();
    assert!(!applied.monitor_ids.is_empty());
    let monitors = db_monitors::list_monitors(&conn).unwrap();
    let push = monitors.iter().find(|m| m.name == "Nightly job").unwrap_or_else(|| panic!("{:?}", import.skipped));
    assert_eq!(push.push_token.as_deref(), Some("kumaPushTok1"), "push URLs keep working");
    let web = monitors.iter().find(|m| m.target == "https://example.com").unwrap();
    assert_eq!(db_notifications::monitor_channel_links(&conn, web.id).unwrap().len(), 2);
    assert!(monitors.iter().any(|m| m.name == "Web container" && !m.active));

    // Importing again changes nothing.
    let mut again = read_file(&kuma_db("again")).unwrap();
    let second = super::plan(&conn, &mut again).unwrap();
    assert!(!second.has_changes(), "{:?}", second.items.iter().filter(|i| i.action != Action::Unchanged).collect::<Vec<_>>());
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
    assert!(!site.options.contains_key("retry_interval_seconds"), "same as interval → default");
    assert_eq!(site.notifications.as_ref().unwrap()[0].name(), "Hook");
    assert_eq!(site.tags.as_ref().unwrap()[0], TagRef::Full { name: "region".into(), value: "eu".into() });
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
