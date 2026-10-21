//! Status page extras through config-as-code and the Uptime Kuma importer.

use base64::Engine as _;
use rusqlite::Connection;

use super::extras::{self, Analytics, AssetKind, Provider};
use super::{self as pages, NewStatusPage};
use crate::config_io::{self, Mode};
use crate::db::test_conn;

fn png_data_url() -> String {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[7u8; 40]);
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

fn doc(yaml: &str) -> config_io::Document {
    config_io::parse_document(yaml).unwrap()
}

fn page_id(conn: &Connection, slug: &str) -> i64 {
    pages::get_by_slug(conn, slug).unwrap().unwrap().id
}

#[test]
fn config_sets_password_analytics_and_images_idempotently() {
    let mut conn = test_conn();
    let yaml = format!(
        "status_pages:\n  - slug: acme\n    title: Acme\n    password: correct horse\n    analytics:\n      provider: plausible\n      site_id: status.acme.test\n      script_url: https://plausible.io/js/script.js\n    logo: \"{}\"\n",
        png_data_url()
    );
    let applied = config_io::apply(&mut conn, &doc(&yaml), Mode::Merge)
        .unwrap()
        .unwrap();
    assert!(applied.pages_changed);
    let p = pages::get_by_slug(&conn, "acme").unwrap().unwrap();
    assert!(
        p.password_hash
            .as_deref()
            .is_some_and(|h| crate::auth::verify_password(h, "correct horse"))
    );
    assert_eq!(p.analytics.provider, Provider::Plausible);
    assert!(p.logo_sha.is_some() && p.favicon_sha.is_none());

    // Same document again: nothing to do (the password verifies, so it isn't re-hashed).
    let plan = config_io::plan(&conn, &doc(&yaml), Mode::Merge).unwrap();
    assert!(!plan.has_changes(), "{plan:?}");

    // Removing via "", and a changed password, show up in the plan.
    let yaml2 = "status_pages:\n  - slug: acme\n    title: Acme\n    password: battery staple\n    logo: \"\"\n";
    let plan = config_io::plan(&conn, &doc(yaml2), Mode::Merge).unwrap();
    let item = plan.items.iter().find(|i| i.key == "acme").unwrap();
    assert!(
        item.changes.contains(&"password".to_string())
            && item.changes.contains(&"logo".to_string()),
        "{item:?}"
    );
    let version = p.password_version;
    config_io::apply(&mut conn, &doc(yaml2), Mode::Merge)
        .unwrap()
        .unwrap();
    let p = pages::get_by_slug(&conn, "acme").unwrap().unwrap();
    assert_eq!(p.password_version, version + 1);
    assert!(p.logo_sha.is_none());
    // Analytics wasn't in the document: kept.
    assert_eq!(p.analytics.provider, Provider::Plausible);
}

#[test]
fn config_rejects_bad_extras_with_field_errors() {
    let conn = test_conn();
    let yaml = "status_pages:\n  - slug: acme\n    title: Acme\n    password: short\n    analytics:\n      provider: google\n      site_id: \"G-1\\\"><script>\"\n    favicon: \"data:image/svg+xml;base64,PHN2Zz4=\"\n";
    let plan = config_io::plan(&conn, &doc(yaml), Mode::Merge).unwrap();
    let fields: Vec<&str> = plan
        .errors
        .iter()
        .filter_map(|e| e.field.as_deref())
        .collect();
    for f in ["password", "analytics.site_id", "favicon"] {
        assert!(fields.contains(&f), "{f} missing in {fields:?}");
    }
    // A masked password for a page without one can't be applied.
    let masked = format!(
        "status_pages:\n  - slug: acme\n    title: Acme\n    password: \"{}\"\n",
        crate::notification::SECRET_MASK
    );
    let plan = config_io::plan(&conn, &doc(&masked), Mode::Merge).unwrap();
    assert!(
        plan.errors
            .iter()
            .any(|e| e.field.as_deref() == Some("password"))
    );
    // Unknown analytics keys are refused like everywhere else in the document.
    assert!(
        config_io::parse_document(
            "status_pages:\n  - slug: a\n    title: A\n    analytics: {tracking: x}\n"
        )
        .is_err()
    );
}

#[test]
fn exports_mask_secrets_and_images_unless_asked() {
    let mut conn = test_conn();
    let id = pages::insert(
        &mut conn,
        &NewStatusPage {
            slug: "acme".into(),
            title: "Acme".into(),
            ..NewStatusPage::default()
        },
    )
    .unwrap();
    let hash = crate::auth::hash_password("correct horse");
    extras::set_password_hash(&conn, id, Some(&hash)).unwrap();
    extras::set_asset(
        &conn,
        id,
        AssetKind::Logo,
        &extras::parse_data_url(&png_data_url()).unwrap(),
    )
    .unwrap();
    let ga = Analytics {
        provider: Provider::Google,
        site_id: "G-ABCDEF12".into(),
        ..Analytics::default()
    };
    extras::set_analytics(&conn, id, &ga).unwrap();

    let plain = config_io::export(&conn, false).unwrap();
    let p = &plain.status_pages.as_ref().unwrap()[0];
    assert_eq!(
        p.password.as_deref(),
        Some(crate::notification::SECRET_MASK)
    );
    assert_eq!(
        (p.password_hash.as_deref(), p.logo.as_deref()),
        (None, None)
    );
    assert_eq!(
        p.analytics.as_ref().map(|a| a.provider.as_str()),
        Some("google")
    );
    let text = config_io::render(&plain, config_io::Format::Yaml).unwrap();
    assert!(!text.contains("argon2") && !text.contains("base64"));

    // The plain export re-applies without changes (mask keeps the password).
    assert!(
        !config_io::plan(&conn, &plain, Mode::Merge)
            .unwrap()
            .has_changes()
    );

    // A full export restores everything on a fresh instance.
    let full = config_io::export(&conn, true).unwrap();
    let fp = &full.status_pages.as_ref().unwrap()[0];
    assert_eq!(fp.password_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(fp.logo.as_deref(), Some(png_data_url().as_str()));
    let mut fresh = test_conn();
    config_io::apply(&mut fresh, &full, Mode::Merge)
        .unwrap()
        .unwrap();
    let restored = pages::get(&fresh, page_id(&fresh, "acme"))
        .unwrap()
        .unwrap();
    assert!(
        restored
            .password_hash
            .as_deref()
            .is_some_and(|h| crate::auth::verify_password(h, "correct horse"))
    );
    assert_eq!(restored.analytics, ga);
    assert!(restored.logo_sha.is_some());
}

/// A minimal Kuma 2.x database with three status pages.
fn kuma_db(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("kuma.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE monitor (id INTEGER PRIMARY KEY, name VARCHAR, type VARCHAR, url TEXT, interval INTEGER, active BOOLEAN DEFAULT 1);
         CREATE TABLE status_page (id INTEGER PRIMARY KEY, slug VARCHAR, title VARCHAR, description TEXT, icon TEXT, theme VARCHAR,
             published BOOLEAN DEFAULT 1, show_tags BOOLEAN DEFAULT 0, password VARCHAR, footer_text TEXT, custom_css TEXT,
             show_powered_by BOOLEAN DEFAULT 1, analytics_id VARCHAR, analytics_script_url VARCHAR, analytics_type VARCHAR,
             auto_refresh_interval INTEGER);
         INSERT INTO status_page (id, slug, title, icon, password, analytics_id, analytics_script_url, analytics_type) VALUES
             (1, 'acme', 'Acme', '{logo}', NULL, '7', 'matomo.acme.test', 'matomo'),
             (2, 'files', 'Files', '/upload/logo2.png?t=1700000000', 'hunter2', 'G-ABCDEF12', NULL, 'google'),
             (3, 'odd', 'Odd', '/icon.svg', NULL, 'x', 'https://r.example/s.js', 'rybbit');",
        logo = png_data_url()
    ))
    .unwrap();
    path
}

#[test]
fn kuma_icons_passwords_and_analytics_map() {
    let dir = std::env::temp_dir().join(format!("vv-kuma-extras-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let import = crate::kuma_import::read_sqlite(&kuma_db(&dir)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let pages = import.doc.status_pages.as_ref().unwrap();
    let by = |slug: &str| pages.iter().find(|p| p.slug == slug).unwrap();

    let acme = by("acme");
    assert_eq!(acme.logo.as_deref(), Some(png_data_url().as_str()));
    assert_eq!(acme.favicon, acme.logo, "Kuma's icon is both");
    let a = acme.analytics.as_ref().unwrap();
    assert_eq!(
        (
            a.provider.as_str(),
            a.site_id.as_str(),
            a.script_url.as_str()
        ),
        ("matomo", "7", "https://matomo.acme.test/")
    );
    assert!(acme.published);

    // A password can't come over: draft + a note; the uploaded file needs a re-upload.
    let files = by("files");
    assert!(!files.published);
    assert_eq!(files.password, None);
    assert_eq!(files.logo, None);
    assert_eq!(
        files.analytics.as_ref().map(|a| a.site_id.as_str()),
        Some("G-ABCDEF12")
    );
    let warnings = import.warnings.join("\n");
    assert!(
        warnings.contains("“Files”: its password can’t be carried over"),
        "{warnings}"
    );
    assert!(
        warnings.contains("/upload/logo2.png") && warnings.contains("Upload it again"),
        "{warnings}"
    );

    // Unsupported providers are dropped with a note; the default icon is ignored.
    let odd = by("odd");
    assert_eq!((odd.analytics.as_ref(), odd.logo.as_ref()), (None, None));
    assert!(
        warnings.contains("rybbit analytics isn’t supported"),
        "{warnings}"
    );

    // The whole import plans cleanly and applies.
    let mut conn = test_conn();
    let mut import = import;
    let plan = crate::kuma_import::plan(&conn, &mut import).unwrap();
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    config_io::apply(&mut conn, &import.doc, Mode::Merge)
        .unwrap()
        .unwrap();
    let acme = pages::get_by_slug(&conn, "acme").unwrap().unwrap();
    assert!(acme.logo_sha.is_some() && acme.favicon_sha.is_some());
    assert_eq!(acme.analytics.provider, Provider::Matomo);
}
