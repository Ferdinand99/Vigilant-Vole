use rusqlite::Connection;

use super::*;
use crate::db::models::{MonitorType, NewMonitor};
use crate::db::{monitors as db_monitors, notifications as db_notifications, tags as db_tags, test_conn};

fn pairs(list: &[(&str, &str)]) -> Pairs {
    Pairs(list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
}

fn values(list: &[(&str, &str)]) -> BulkValues {
    BulkValues::from_pairs(&pairs(list), &[1, 2], &[7, 8])
}

fn monitor(conn: &Connection, name: &str, kind: MonitorType) -> i64 {
    db_monitors::insert_monitor(
        conn,
        &NewMonitor {
            name: name.into(),
            monitor_type: kind,
            target: "https://example.com".into(),
            interval_seconds: 60,
            timeout_seconds: 10,
            ..NewMonitor::default()
        },
    )
    .unwrap()
}

fn channel(conn: &Connection, name: &str) -> i64 {
    db_notifications::insert_channel(conn, name, "discord", "{}", false).unwrap()
}

fn get(conn: &Connection, id: i64) -> NewMonitor {
    db_monitors::get_monitor(conn, id).unwrap().unwrap().to_new()
}

// -- parsing -----------------------------------------------------------------

#[test]
fn tri_state_parsing() {
    assert_eq!(parse_tri("on"), Some(true));
    assert_eq!(parse_tri("true"), Some(true));
    assert_eq!(parse_tri("off"), Some(false));
    assert_eq!(parse_tri(" false "), Some(false));
    assert_eq!(parse_tri(""), None);
    assert_eq!(parse_tri("maybe"), None);

    let v = values(&[("run_on_agents", "on"), ("confirm_with_agents", "bogus")]);
    assert_eq!((v.run_on_agents.as_str(), v.confirm_with_agents.as_str()), ("on", ""));
    let edit = validate(&v).unwrap();
    assert_eq!((edit.run_on_agents, edit.confirm_with_agents), (Some(true), None));
    assert_eq!(validate(&values(&[])).unwrap(), BulkEdit::default());
    assert!(validate(&values(&[])).unwrap().is_empty());
}

#[test]
fn rows_only_read_known_channels_and_tags() {
    let v = values(&[
        ("channel_1", "add"),
        ("channel_1_escalation", "true"),
        ("channel_2", "remove"),
        ("channel_9", "add"),
        ("tag_7", "add"),
        ("tag_value_7", " prod "),
        ("tag_8", "nonsense"),
        ("tag_9", "remove"),
    ]);
    assert_eq!(v.channel_op(&1), "add");
    assert!(v.channel_escalation(&1));
    assert_eq!(v.tag_op(&8), "");
    assert_eq!(v.change_count(), 3);
    let edit = validate(&v).unwrap();
    assert_eq!(edit.channels, ChannelEdit::Change { add: vec![(1, true)], remove: vec![2] });
    assert_eq!(edit.tags.add, vec![(7, "prod".to_string())]);
    assert!(edit.tags.remove.is_empty());
}

#[test]
fn replace_mode_and_counts() {
    let v = values(&[("channels_mode", "replace"), ("replace_1", "escalation"), ("replace_2", "off"), ("channel_1", "add")]);
    assert!(v.replaces_channels());
    assert_eq!(v.change_count(), 1);
    assert_eq!(validate(&v).unwrap().channels, ChannelEdit::Replace(vec![(1, true)]));
    // Replacing with nothing removes every channel (still a change).
    let v = values(&[("channels_mode", "replace")]);
    assert_eq!(validate(&v).unwrap().channels, ChannelEdit::Replace(vec![]));

    let v = values(&[
        ("run_on_agents", "off"),
        ("interval_seconds", "120"),
        ("retries", "2"),
        ("tags_clear", "true"),
        ("new_tag_name", "eu"),
    ]);
    assert_eq!(v.change_count(), 5);
    assert_eq!(summary(5, 43), "Changes 5 settings on 43 monitors");
    assert_eq!(summary(1, 1), "Changes 1 setting on 1 monitor");
    assert_eq!(summary(0, 3), crate::i18n::t("bulk.summary_none"));
    assert_eq!(summary(2, 0), crate::i18n::t("bulk.summary_no_monitors"));
}

// -- validation --------------------------------------------------------------

#[test]
fn numbers_use_the_monitor_forms_rules() {
    let errors = validate(&values(&[("interval_seconds", "5"), ("retries", "x"), ("timeout_seconds", "900")])).unwrap_err();
    assert!(errors["interval_seconds"].contains("between 10 and"), "{errors:?}");
    assert!(errors["retries"].contains("whole number"), "{errors:?}");
    assert!(errors["timeout_seconds"].contains("between 1 and"), "{errors:?}");

    let errors = validate(&values(&[("interval_seconds", "30"), ("timeout_seconds", "60")])).unwrap_err();
    assert_eq!(errors.keys().copied().collect::<Vec<_>>(), vec!["timeout_seconds"]);

    let edit = validate(&values(&[("interval_seconds", "120"), ("retry_interval_seconds", "30"), ("timeout_seconds", "20"), ("retries", "0")])).unwrap();
    assert_eq!((edit.interval, edit.retry_interval, edit.timeout, edit.retries), (Some(120), Some(30), Some(20), Some(0)));
}

#[test]
fn tag_names_and_values_are_checked() {
    let long = "x".repeat(db_tags::MAX_NAME_LEN + 1);
    let errors = validate(&values(&[("new_tag_name", &long)])).unwrap_err();
    assert!(errors.contains_key("new_tag_name"));
    let long_value = "v".repeat(db_tags::MAX_VALUE_LEN + 1);
    let errors = validate(&values(&[("tag_7", "add"), ("tag_value_7", &long_value)])).unwrap_err();
    assert!(errors.contains_key("tags"));
    let edit = validate(&values(&[("new_tag_name", " eu "), ("new_tag_color", "#00ff00"), ("new_tag_value", "west")])).unwrap();
    assert_eq!(edit.tags.create, vec![NewTag { name: "eu".into(), color: "green".into(), value: "west".into() }]);
}

// -- per-monitor rules -------------------------------------------------------

#[test]
fn agent_settings_skip_types_agents_cant_run() {
    let edit = BulkEdit { run_on_agents: Some(true), confirm_with_agents: Some(true), ..BulkEdit::default() };
    for kind in [MonitorType::Push, MonitorType::Group, MonitorType::RealBrowser] {
        let mut m = NewMonitor { monitor_type: kind, ..NewMonitor::default() };
        assert_eq!(apply_settings(&mut m, &edit), vec![Skip::Agents], "{kind}");
        assert!(!m.run_on_agents && !m.confirm_with_agents);
    }
    let mut m = NewMonitor { monitor_type: MonitorType::Tcp, ..NewMonitor::default() };
    assert!(apply_settings(&mut m, &edit).is_empty());
    assert!(m.run_on_agents && m.confirm_with_agents);
}

#[test]
fn timing_follows_the_form_per_type() {
    let edit = BulkEdit { interval: Some(120), retries: Some(3), timeout: Some(20), retry_interval: Some(30), ..BulkEdit::default() };
    let mut group = NewMonitor { monitor_type: MonitorType::Group, ..NewMonitor::default() };
    let before = group.clone();
    assert_eq!(apply_settings(&mut group, &edit), vec![Skip::Timing]);
    assert_eq!(group, before);

    // Push: interval and retries apply, timeout and retry interval don't.
    let mut push = NewMonitor { monitor_type: MonitorType::Push, ..NewMonitor::default() };
    assert_eq!(apply_settings(&mut push, &edit), vec![Skip::Timing]);
    assert_eq!((push.interval_seconds, push.retries), (120, 3));
    assert_eq!(push.retry_interval_seconds, None);

    let mut http = NewMonitor { monitor_type: MonitorType::Http, ..NewMonitor::default() };
    assert!(apply_settings(&mut http, &edit).is_empty());
    assert_eq!((http.interval_seconds, http.retries, http.timeout_seconds, http.retry_interval_seconds), (120, 3, 20, Some(30)));

    // An interval shorter than the monitor's own timeout is skipped whole.
    let short = BulkEdit { interval: Some(15), retries: Some(1), ..BulkEdit::default() };
    let mut slow = NewMonitor { monitor_type: MonitorType::Http, timeout_seconds: 30, interval_seconds: 60, ..NewMonitor::default() };
    assert_eq!(apply_settings(&mut slow, &short), vec![Skip::TimeoutOverInterval]);
    assert_eq!((slow.interval_seconds, slow.retries), (60, 0));
}

#[test]
fn channel_changes_and_escalation() {
    let current = [(1, false), (2, true)];
    let mut skips = Vec::new();
    let add = ChannelEdit::Change { add: vec![(3, true), (1, true)], remove: vec![2] };
    assert_eq!(apply_channels(&current, &add, true, &mut skips), vec![(1, true), (3, true)]);
    assert!(skips.is_empty());
    // Without an escalation delay the escalation links are skipped (once).
    assert_eq!(apply_channels(&current, &add, false, &mut skips), vec![(1, false)]);
    assert_eq!(skips, vec![Skip::EscalationDelay]);

    let mut skips = Vec::new();
    let replace = ChannelEdit::Replace(vec![(4, false), (5, true)]);
    assert_eq!(apply_channels(&current, &replace, true, &mut skips), vec![(4, false), (5, true)]);
    assert_eq!(apply_channels(&current, &ChannelEdit::Replace(vec![]), true, &mut skips), vec![]);
    assert_eq!(apply_channels(&current, &ChannelEdit::Unchanged, true, &mut skips), current.to_vec());
}

#[test]
fn tag_changes() {
    let current = vec![(1, "a".to_string()), (2, String::new())];
    let edit = TagEdit { remove: vec![2], add: vec![(1, "b".into()), (3, String::new())], ..TagEdit::default() };
    assert_eq!(apply_tags(&current, &edit, &[(9, "new".into())]), vec![(1, "b".into()), (3, String::new()), (9, "new".into())]);
    let clear = TagEdit { clear: true, add: vec![(5, String::new())], ..TagEdit::default() };
    assert_eq!(apply_tags(&current, &clear, &[]), vec![(5, String::new())]);
}

// -- the transaction ---------------------------------------------------------

#[test]
fn apply_updates_links_and_reports_skips() {
    let mut conn = test_conn();
    let http = monitor(&conn, "web", MonitorType::Http);
    let push = monitor(&conn, "cron", MonitorType::Push);
    let group = monitor(&conn, "prod", MonitorType::Group);
    let (ops, mail) = (channel(&conn, "Ops"), channel(&conn, "Mail"));
    db_notifications::set_monitor_channels(&conn, http, &[(mail, false)]).unwrap();
    let old = db_tags::insert_tag(&conn, "old", "red").unwrap();
    db_tags::set_monitor_tags(&conn, http, &[(old, String::new())]).unwrap();

    let edit = BulkEdit {
        run_on_agents: Some(true),
        interval: Some(120),
        channels: ChannelEdit::Change { add: vec![(ops, false)], remove: vec![mail] },
        tags: TagEdit {
            remove: vec![old],
            create: vec![NewTag { name: "eu".into(), color: "teal".into(), value: "west".into() }],
            ..TagEdit::default()
        },
        ..BulkEdit::default()
    };
    let out = apply(&mut conn, &[http, push, group, push, 999], &edit).unwrap();
    assert_eq!(out.updated, vec![http, push, group]);
    assert_eq!(out.skipped, vec![(push, Skip::Agents), (group, Skip::Agents), (group, Skip::Timing)]);
    assert_eq!(out.skip_counts(), vec![(Skip::Agents, 2), (Skip::Timing, 1)]);
    assert!(out.message().starts_with("Updated 3 monitors · skipped 2 ("), "{}", out.message());

    let m = get(&conn, http);
    assert!(m.run_on_agents);
    assert_eq!(m.interval_seconds, 120);
    assert_eq!(get(&conn, push).interval_seconds, 120);
    assert!(!get(&conn, push).run_on_agents);
    assert_eq!(db_notifications::monitor_channel_links(&conn, http).unwrap(), vec![(ops, false)]);
    assert_eq!(db_notifications::monitor_channel_links(&conn, group).unwrap(), vec![(ops, false)]);
    let eu = db_tags::find_by_name(&conn, "eu").unwrap().unwrap();
    assert_eq!(eu.color, "teal");
    let tags = db_tags::tags_for_monitor(&conn, http).unwrap();
    assert_eq!(tags.iter().map(|t| (t.name.as_str(), t.value.as_str())).collect::<Vec<_>>(), vec![("eu", "west")]);

    // Same edit again: nothing left to change.
    let again = apply(&mut conn, &[http], &edit).unwrap();
    assert!(again.updated.is_empty());
    assert_eq!(again.message(), crate::i18n::t("bulk.nothing_changed"));
}

#[test]
fn replace_and_clear() {
    let mut conn = test_conn();
    let a = monitor(&conn, "a", MonitorType::Http);
    let (x, y) = (channel(&conn, "X"), channel(&conn, "Y"));
    db_notifications::set_monitor_channels(&conn, a, &[(x, false)]).unwrap();
    let t = db_tags::insert_tag(&conn, "t", "blue").unwrap();
    db_tags::set_monitor_tags(&conn, a, &[(t, "v".into())]).unwrap();
    let edit = BulkEdit {
        channels: ChannelEdit::Replace(vec![(y, true)]),
        tags: TagEdit { clear: true, ..TagEdit::default() },
        ..BulkEdit::default()
    };
    // No escalation delay: the escalation link is skipped, X still goes.
    let out = apply(&mut conn, &[a], &edit).unwrap();
    assert_eq!(out.skipped, vec![(a, Skip::EscalationDelay)]);
    assert!(db_notifications::monitor_channel_links(&conn, a).unwrap().is_empty());
    assert!(db_tags::tags_for_monitor(&conn, a).unwrap().is_empty());

    let mut m = get(&conn, a);
    m.escalation_minutes = 15;
    db_monitors::update_monitor(&conn, a, &m).unwrap();
    let out = apply(&mut conn, &[a], &edit).unwrap();
    assert!(out.skipped.is_empty());
    assert_eq!(db_notifications::monitor_channel_links(&conn, a).unwrap(), vec![(y, true)]);
}

#[test]
fn a_failure_rolls_everything_back() {
    let mut conn = test_conn();
    let first = monitor(&conn, "first", MonitorType::Http);
    let second = monitor(&conn, "second", MonitorType::Http);
    conn.execute_batch(&format!(
        "CREATE TRIGGER boom BEFORE UPDATE ON monitors WHEN NEW.id = {second} BEGIN SELECT RAISE(ABORT, 'boom'); END;"
    ))
    .unwrap();
    let edit = BulkEdit {
        interval: Some(300),
        tags: TagEdit { create: vec![NewTag { name: "new".into(), color: "red".into(), value: String::new() }], ..TagEdit::default() },
        ..BulkEdit::default()
    };
    assert!(apply(&mut conn, &[first, second], &edit).is_err());
    assert_eq!(get(&conn, first).interval_seconds, 60, "the first monitor's change was rolled back");
    assert!(db_tags::find_by_name(&conn, "new").unwrap().is_none(), "the created tag was rolled back");
}
