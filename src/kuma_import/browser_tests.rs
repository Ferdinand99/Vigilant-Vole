//! Kuma `real-browser` monitors and `remote_browser` rows.

use rusqlite::Connection;
use serde_json::json;

use super::*;
use crate::db::{remote_browsers as db_rb, test_conn};

const SCHEMA: &str = r#"
CREATE TABLE monitor (id INTEGER PRIMARY KEY, name VARCHAR(150), active BOOLEAN DEFAULT 1, interval INTEGER DEFAULT 60,
    url TEXT, type VARCHAR(20), timeout DOUBLE DEFAULT 48, ignore_tls BOOLEAN DEFAULT 0, keyword VARCHAR(255),
    remote_browser INTEGER);
CREATE TABLE remote_browser (id INTEGER PRIMARY KEY, name VARCHAR(255) NOT NULL, url VARCHAR(255) NOT NULL, user_id INTEGER);
INSERT INTO remote_browser VALUES (1, 'Browserless', 'ws://browserless:3000/playwright?token=k3y', 1);
INSERT INTO remote_browser VALUES (2, 'Chrome', 'ws://chrome:9222/devtools/browser/abc', 1);
INSERT INTO monitor (id, name, type, url, remote_browser, ignore_tls, keyword) VALUES (1, 'Shop', 'real-browser', 'https://shop.example.com', 1, 1, 'stale');
INSERT INTO monitor (id, name, type, url, remote_browser) VALUES (2, 'Blog', 'real-browser', 'https://blog.example.com', 2);
INSERT INTO monitor (id, name, type, url) VALUES (3, 'Local', 'real-browser', 'https://local.example.com');
INSERT INTO monitor (id, name, type, url, remote_browser) VALUES (4, 'Gone', 'real-browser', 'https://gone.example.com', 9);
"#;

fn kuma(name: &str) -> KumaImport {
    let dir = std::env::temp_dir().join(format!("vv-kuma-rb-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("kuma.db");
    let _ = std::fs::remove_file(&path);
    Connection::open(&path).unwrap().execute_batch(SCHEMA).unwrap();
    read_sqlite(&path).unwrap()
}

fn doc<'a>(import: &'a KumaImport, name: &str) -> &'a MonitorDoc {
    import.doc.monitors.as_ref().unwrap().iter().find(|m| m.name == name).unwrap()
}

#[test]
fn real_browser_monitors_and_remote_browsers_map_to_ours() {
    let import = kuma("map");
    let browsers = import.doc.remote_browsers.as_ref().unwrap();
    assert_eq!(browsers.len(), 2);
    // browserless's Playwright path becomes its CDP endpoint, token kept.
    assert_eq!((browsers[0].name.as_str(), browsers[0].url.as_str()), ("Browserless", "ws://browserless:3000/?token=k3y"));
    assert_eq!(browsers[1].url, "ws://chrome:9222/devtools/browser/abc");
    let shop = doc(&import, "Shop");
    assert_eq!((shop.monitor_type.as_str(), shop.target.as_str()), ("real_browser", "https://shop.example.com"));
    assert_eq!(shop.options.get("remote_browser"), Some(&json!("Browserless")));
    assert_eq!(shop.options.get("ignore_tls"), Some(&json!(true)));
    assert!(shop.options.get("keyword").is_none(), "Kuma doesn't check keywords on real-browser monitors");
    assert_eq!(doc(&import, "Blog").options.get("remote_browser"), Some(&json!("Chrome")));
    assert!(doc(&import, "Local").options.get("remote_browser").is_none());
    let warned = |name: &str, text: &str| import.warnings.iter().any(|w| w.contains(name) && w.contains(text));
    assert!(warned("Local", "built-in Chromium"), "{:?}", import.warnings);
    assert!(warned("Gone", "wasn’t in the export"), "{:?}", import.warnings);
    assert!(warned("Browserless", "Playwright"), "{:?}", import.warnings);
}

#[test]
fn the_import_plan_creates_browsers_and_links_monitors() {
    let mut import = kuma("plan");
    let mut conn = test_conn();
    let plan = plan(&conn, &mut import).unwrap();
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(plan.action_for(config_io::KIND_BROWSER, "Browserless"), Some(config_io::Action::Create));
    config_io::apply(&mut conn, &import.doc, config_io::Mode::Merge).unwrap().unwrap();
    let chrome = db_rb::get_by_name(&conn, "Chrome").unwrap().unwrap();
    let browserless = db_rb::get_by_name(&conn, "Browserless").unwrap().unwrap();
    let monitors = crate::db::monitors::list_monitors(&conn).unwrap();
    let by_name = |n: &str| monitors.iter().find(|m| m.name == n).unwrap();
    assert_eq!(by_name("Shop").remote_browser_id, Some(browserless.id));
    assert_eq!(by_name("Blog").remote_browser_id, Some(chrome.id));
    // Kuma's local Chromium: the default (first) browser here.
    assert_eq!(by_name("Local").remote_browser_id, Some(browserless.id));
}

#[test]
fn a_bad_remote_browser_is_skipped_and_its_monitors_kept() {
    let mut import = kuma("skip");
    import.doc.remote_browsers.as_mut().unwrap()[0].url = "ftp://nope".into();
    let conn = test_conn();
    let plan = plan(&conn, &mut import).unwrap();
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert!(import.skipped.iter().any(|s| s.kind == "remote_browser" && s.name == "Browserless"), "{:?}", import.skipped);
    assert!(doc(&import, "Shop").options.get("remote_browser").is_none(), "the reference is dropped with it");
    assert_eq!(plan.action_for(config_io::KIND_MONITOR, "Shop"), Some(config_io::Action::Create));
}
