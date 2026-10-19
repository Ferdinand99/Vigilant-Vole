use super::*;
use crate::db::models::NewMonitor;
use crate::db::test_conn;

/// A document touching every section.
const SEED: &str = r##"
version: 1
settings:
  site_name: Acme uptime
  base_url: https://status.example.com
  timezone: Europe/Oslo
  retention_days: 120
  badges_public: true
tags:
  - name: production
    color: green
  - name: team
    color: "#ff0000"
notification_channels:
  - name: Ops Discord
    kind: discord
    default: true
    config:
      url: https://discord.com/api/webhooks/1/secret-token
      username: Vole
  - name: Pager
    kind: pagerduty
    config:
      routing_key: R0UT1NGKEY
monitors:
  - name: Backend
    monitor_type: group
  - name: API
    monitor_type: http
    target: https://api.example.com/health
    parent: Backend
    interval_seconds: 30
    retries: 2
    escalation_minutes: 15
    basic_auth_user: bot
    basic_auth_pass: hunter2hunter2
    http_headers:
      Authorization: Bearer abc
      X-Trace: "1"
    tags: [production, {name: team, value: core}]
    notifications: [Ops Discord, {name: Pager, escalation: true}]
  - key: db-port
    name: Database
    monitor_type: tcp
    target: db.internal:5432
    parent: Backend
  - name: Cron job
    monitor_type: push
    push_token: cronTOKEN12345
status_pages:
  - slug: acme
    title: Acme status
    sections:
      - name: Core
        monitors: [API, db-port]
      - name: Jobs
        monitors: [Cron job]
maintenance:
  - title: Nightly backup
    strategy: weekly
    weekdays: "1,3"
    time_from: "02:00"
    time_to: "02:30"
    timezone: Europe/Oslo
    monitors: [Backend]
    status_pages: [acme]
"##;

fn seeded() -> Connection {
    let mut conn = test_conn();
    let doc = parse_document(SEED).unwrap();
    let applied = apply(&mut conn, &doc, Mode::Merge).unwrap().unwrap();
    assert!(applied.plan.errors.is_empty(), "{:?}", applied.plan.errors);
    conn
}

fn monitor_named(conn: &Connection, name: &str) -> Monitor {
    db_monitors::list_monitors(conn).unwrap().into_iter().find(|m| m.name == name).unwrap()
}

#[test]
fn seed_document_creates_everything_with_relations() {
    let conn = seeded();
    let api = monitor_named(&conn, "API");
    let backend = monitor_named(&conn, "Backend");
    assert_eq!(api.parent_id, Some(backend.id));
    assert_eq!(api.interval_seconds, 30);
    assert_eq!(api.source, "config");
    assert_eq!(api.external_id.as_deref(), Some("API"));
    assert_eq!(monitor_named(&conn, "Database").external_id.as_deref(), Some("db-port"));
    assert_eq!(monitor_named(&conn, "Cron job").push_token.as_deref(), Some("cronTOKEN12345"));
    let tags = db_tags::tags_for_monitor(&conn, api.id).unwrap();
    assert_eq!(tags.len(), 2);
    assert!(tags.iter().any(|t| t.name == "team" && t.value == "core" && t.color == "red"));
    let links = db_notifications::monitor_channel_links(&conn, api.id).unwrap();
    assert_eq!(links.len(), 2);
    assert!(links.iter().any(|(_, esc)| *esc));
    let settings = db_settings::load(&conn).unwrap();
    assert_eq!(settings.site_name, "Acme uptime");
    assert_eq!(settings.retention_days, 120);
    let page = db_pages::get_by_slug(&conn, "acme").unwrap().unwrap();
    let sections = db_pages::sections(&conn, page.id).unwrap();
    assert_eq!(sections.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Core", "Jobs"]);
    assert_eq!(sections[0].monitors.len(), 2);
    let w = &db_maint::list(&conn).unwrap()[0];
    assert_eq!(w.weekdays.as_deref(), Some("1,3"));
    assert_eq!(db_maint::monitor_ids(&conn, w.id).unwrap(), vec![backend.id]);
}

#[test]
fn export_import_round_trip_is_lossless() {
    let conn = seeded();
    // A monitor created in the UI (source manual) round-trips too.
    db_monitors::insert_monitor(
        &conn,
        &NewMonitor { name: "Manual".into(), target: "https://manual.example.com".into(), ..NewMonitor::default() },
    )
    .unwrap();
    let first = export(&conn, true).unwrap();
    for format in [Format::Yaml, Format::Json] {
        let text = render(&first, format).unwrap();
        let parsed = parse_document(&text).unwrap();
        let mut fresh = test_conn();
        let applied = apply(&mut fresh, &parsed, Mode::Merge).unwrap().unwrap();
        assert!(applied.plan.errors.is_empty(), "{:?}", applied.plan.errors);
        assert_eq!(export(&fresh, true).unwrap(), first, "{format:?} round trip");
    }
}

#[test]
fn reapplying_an_export_is_a_no_op() {
    let conn = seeded();
    let doc = export(&conn, true).unwrap();
    let plan = plan(&conn, &doc, Mode::Merge).unwrap();
    assert!(plan.is_ok(), "{:?}", plan.errors);
    assert!(!plan.has_changes(), "{:?}", plan.items.iter().filter(|i| i.action != Action::Unchanged).collect::<Vec<_>>());
}

#[test]
fn dry_run_plan_reports_creates_updates_and_changed_fields() {
    let conn = seeded();
    let mut doc = export(&conn, true).unwrap();
    let monitors = doc.monitors.as_mut().unwrap();
    let api = monitors.iter_mut().find(|m| m.name == "API").unwrap();
    api.options.insert("interval_seconds".into(), serde_json::json!(45));
    monitors.push(MonitorDoc {
        key: None,
        name: "New one".into(),
        monitor_type: "ping".into(),
        target: "192.0.2.1".into(),
        parent: None,
        options: Map::new(),
        tags: None,
        notifications: None,
        push_token: None,
    });
    let plan = plan(&conn, &doc, Mode::Merge).unwrap();
    assert_eq!(plan.action_for(KIND_MONITOR, "API"), Some(Action::Update));
    let item = plan.items.iter().find(|i| i.key == "API").unwrap();
    assert_eq!(item.changes, vec!["interval_seconds".to_string()]);
    assert_eq!(plan.action_for(KIND_MONITOR, "New one"), Some(Action::Create));
    assert_eq!(plan.summary().create, 1);
    // Dry run wrote nothing.
    assert_eq!(monitor_named(&conn, "API").interval_seconds, 30);
}

#[test]
fn merge_leaves_unlisted_objects_but_replace_deletes_them() {
    let mut conn = seeded();
    db_monitors::insert_monitor(
        &conn,
        &NewMonitor { name: "Container".into(), monitor_type: MonitorType::Docker, target: "web".into(), source: "docker".into(), external_id: Some("web".into()), ..NewMonitor::default() },
    )
    .unwrap();
    let doc = parse_document("monitors:\n  - name: API\n    monitor_type: http\n    target: https://api.example.com/health\n    notifications: []\n").unwrap();
    let merge = plan(&conn, &doc, Mode::Merge).unwrap();
    assert_eq!(merge.summary().delete, 0);

    let replace = plan(&conn, &doc, Mode::Replace).unwrap();
    for key in ["Backend", "db-port", "Cron job"] {
        assert_eq!(replace.action_for(KIND_MONITOR, key), Some(Action::Delete), "{key}");
    }
    assert_eq!(replace.action_for(KIND_MONITOR, "web"), None, "docker monitors are never deleted");
    // Sections missing from the document are left alone.
    assert!(replace.items.iter().all(|i| i.kind == KIND_MONITOR));

    let applied = apply(&mut conn, &doc, Mode::Replace).unwrap().unwrap();
    assert_eq!(applied.deleted_monitor_ids.len(), 3);
    let names: Vec<String> = db_monitors::list_monitors(&conn).unwrap().into_iter().map(|m| m.name).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"API".to_string()) && names.contains(&"Container".to_string()));
    // Docker monitors are not exported either.
    assert!(export(&conn, false).unwrap().monitors.unwrap().iter().all(|m| m.name != "Container"));
}

#[test]
fn exports_without_secrets_mask_them_and_masks_keep_stored_values() {
    let mut conn = seeded();
    let doc = export(&conn, false).unwrap();
    let text = render(&doc, Format::Yaml).unwrap();
    for secret in ["secret-token", "R0UT1NGKEY", "hunter2", "Bearer abc", "cronTOKEN"] {
        assert!(!text.contains(secret), "{secret} leaked:\n{text}");
    }
    let channel = &doc.notification_channels.as_ref().unwrap()[0];
    assert_eq!(channel.config["url"], SECRET_MASK);
    assert_eq!(channel.config["username"], "Vole", "non-secret fields stay readable");
    let api = doc.monitors.as_ref().unwrap().iter().find(|m| m.name == "API").unwrap();
    assert_eq!(api.options["basic_auth_pass"], SECRET_MASK);
    assert_eq!(api.options["http_headers"]["Authorization"], SECRET_MASK);
    assert_eq!(api.options["http_headers"]["X-Trace"], "1");

    // Re-importing the redacted document changes nothing and keeps the secrets.
    let applied = apply(&mut conn, &doc, Mode::Merge).unwrap().unwrap();
    assert!(!applied.plan.has_changes(), "{:?}", applied.plan.items);
    let api = monitor_named(&conn, "API");
    assert_eq!(api.basic_auth_pass.as_deref(), Some("hunter2hunter2"));
    assert!(api.http_headers.unwrap().contains("Bearer abc"));
    assert_eq!(monitor_named(&conn, "Cron job").push_token.as_deref(), Some("cronTOKEN12345"));

    // A fresh instance can't invent the secrets: the plan says so.
    let fresh = test_conn();
    let plan = plan(&fresh, &doc, Mode::Merge).unwrap();
    assert!(plan.errors.iter().any(|e| e.kind == KIND_CHANNEL && e.message.contains("secrets")), "{:?}", plan.errors);
    assert!(plan.errors.iter().any(|e| e.field.as_deref() == Some("basic_auth_pass")));
}

#[test]
fn validation_uses_the_monitor_form_rules() {
    let conn = test_conn();
    let doc = parse_document(
        r#"
monitors:
  - name: Bad URL
    monitor_type: http
    target: example.com
  - name: Typo
    monitor_type: http
    target: https://x.io
    intervall: 5
  - name: Unknown type
    monitor_type: carrier-pigeon
  - name: Orphan
    monitor_type: ping
    target: 192.0.2.1
    parent: Nope
  - name: Tagged
    monitor_type: ping
    target: 192.0.2.1
    tags: [""]
  - name: Not a group
    monitor_type: ping
    target: 192.0.2.2
  - name: Child
    monitor_type: ping
    target: 192.0.2.3
    parent: Not a group
"#,
    )
    .unwrap();
    let plan = plan(&conn, &doc, Mode::Merge).unwrap();
    let field_of = |key: &str| plan.errors.iter().find(|e| e.key == key).and_then(|e| e.field.clone());
    assert_eq!(field_of("Bad URL").as_deref(), Some("target"));
    assert_eq!(field_of("Typo").as_deref(), Some("intervall"));
    assert_eq!(field_of("Unknown type").as_deref(), Some("monitor_type"));
    assert_eq!(field_of("Orphan").as_deref(), Some("parent"));
    assert_eq!(field_of("Tagged").as_deref(), Some("tags"));
    assert_eq!(field_of("Child").as_deref(), Some("parent"), "parents must be groups");
    // Nothing applies while anything is invalid.
    let mut conn = conn;
    assert!(apply(&mut conn, &doc, Mode::Merge).unwrap().is_err());
    assert!(db_monitors::list_monitors(&conn).unwrap().is_empty());
}

#[test]
fn undeclared_tags_are_created_with_the_default_colour() {
    let mut conn = test_conn();
    let doc = parse_document("monitors:
  - name: P
    monitor_type: ping
    target: 192.0.2.1
    tags: [{name: env, value: lab}]
").unwrap();
    let applied = apply(&mut conn, &doc, Mode::Merge).unwrap().unwrap();
    assert_eq!(applied.plan.action_for(KIND_TAG, "env"), Some(Action::Create));
    let tags = db_tags::tags_for_monitor(&conn, monitor_named(&conn, "P").id).unwrap();
    assert_eq!((tags[0].name.as_str(), tags[0].color.as_str(), tags[0].value.as_str()), ("env", db_tags::DEFAULT_COLOR, "lab"));
}

#[test]
fn group_cycles_and_duplicate_keys_are_rejected() {
    let conn = test_conn();
    let doc = parse_document(
        r#"
monitors:
  - name: A
    monitor_type: group
    parent: B
  - name: B
    monitor_type: group
    parent: A
  - name: Twin
    monitor_type: group
  - name: twin
    monitor_type: group
"#,
    )
    .unwrap();
    let plan = plan(&conn, &doc, Mode::Merge).unwrap();
    assert!(plan.errors.iter().any(|e| e.key == "A" && e.message.contains("contain itself")));
    assert!(plan.errors.iter().any(|e| e.key.eq_ignore_ascii_case("twin") && e.message.contains("twice")));
}

#[test]
fn children_are_created_after_parents_listed_later() {
    let mut conn = test_conn();
    let doc = parse_document(
        "monitors:\n  - name: Child\n    monitor_type: ping\n    target: 192.0.2.1\n    parent: Group\n  - name: Group\n    monitor_type: group\n",
    )
    .unwrap();
    apply(&mut conn, &doc, Mode::Merge).unwrap().unwrap();
    assert_eq!(monitor_named(&conn, "Child").parent_id, Some(monitor_named(&conn, "Group").id));
}

#[test]
fn settings_status_pages_and_maintenance_are_validated() {
    let conn = test_conn();
    let doc = parse_document(
        r#"
settings:
  timezone: Mars/Olympus
  retention_days: 0
status_pages:
  - slug: api
    title: Reserved slug
  - slug: ok
    title: Fine
    custom_css: "</style><script>"
maintenance:
  - title: Broken
    strategy: weekly
    time_from: "25:00"
"#,
    )
    .unwrap();
    let plan = plan(&conn, &doc, Mode::Merge).unwrap();
    let has = |kind: &str, key: &str| plan.errors.iter().any(|e| e.kind == kind && e.key == key);
    assert!(has(KIND_SETTING, "timezone"));
    assert!(has(KIND_SETTING, "retention_days"));
    assert!(has(KIND_PAGE, "api"));
    assert!(has(KIND_PAGE, "ok"));
    assert!(has(KIND_MAINTENANCE, "Broken"));
}

#[test]
fn documents_parse_from_yaml_and_json_and_reject_unknown_sections() {
    assert!(parse_document("{\"monitors\": []}").unwrap().monitors.unwrap().is_empty());
    assert!(parse_document("monitors: []\n").unwrap().monitors.is_some());
    assert!(parse_document("monitorz: []\n").unwrap_err().contains("monitorz"));
    assert!(parse_document("version: 99\n").unwrap_err().contains("version"));
    assert!(parse_document("   ").is_err());
}

#[test]
fn sensitive_header_names() {
    for h in ["Authorization", "X-Api-Key", "x-auth-token", "Cookie", "X-Webhook-Secret"] {
        assert!(is_sensitive_header(h), "{h}");
    }
    for h in ["Accept", "X-Trace", "User-Agent"] {
        assert!(!is_sensitive_header(h), "{h}");
    }
}

#[test]
fn backup_validation_accepts_our_database_and_rejects_others() {
    let dir = std::env::temp_dir().join(format!("vv-config-io-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ours = dir.join("ours.db");
    let _ = std::fs::remove_file(&ours);
    seeded().execute("VACUUM INTO ?1", [ours.to_string_lossy().as_ref()]).unwrap();
    assert_eq!(validate_backup_db(&ours).unwrap(), 4);

    let other = dir.join("other.db");
    let _ = std::fs::remove_file(&other);
    Connection::open(&other).unwrap().execute_batch("CREATE TABLE monitor (id INTEGER)").unwrap();
    assert!(validate_backup_db(&other).unwrap_err().contains("isn’t a Vigilant Vole backup"));

    let text = dir.join("text.db");
    std::fs::write(&text, "hello").unwrap();
    assert!(validate_backup_db(&text).is_err());

    // Startup swap: the pending file replaces the database, the old one is kept.
    let data = dir.join("data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("vigilant-vole.db"), "old").unwrap();
    std::fs::copy(&ours, data.join(RESTORE_PENDING_FILE)).unwrap();
    apply_pending_restore(&data);
    assert!(!data.join(RESTORE_PENDING_FILE).exists());
    assert_eq!(validate_backup_db(&data.join("vigilant-vole.db")).unwrap(), 4);
    assert_eq!(std::fs::read_dir(data.join("backups")).unwrap().count(), 1);

    // An invalid pending file is set aside and the database kept.
    std::fs::write(data.join(RESTORE_PENDING_FILE), "garbage").unwrap();
    apply_pending_restore(&data);
    assert!(!data.join(RESTORE_PENDING_FILE).exists());
    assert_eq!(validate_backup_db(&data.join("vigilant-vole.db")).unwrap(), 4);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn discovery_setting_changes_are_detected() {
    let conn = test_conn();
    let doc = parse_document("settings:\n  discovery_enabled: true\n").unwrap();
    assert!(discovery_changed(&plan(&conn, &doc, Mode::Merge).unwrap()));
    let doc = parse_document("settings:\n  site_name: Other\n  discovery_enabled: false\n").unwrap();
    assert!(!discovery_changed(&plan(&conn, &doc, Mode::Merge).unwrap()));
}
