//! Scheduler integration tests against a real (temporary file) SQLite pool.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

use super::*;
use crate::db::models::{ExpiryInfo, ExpiryKind, NewMonitor};

static NEXT_DB: AtomicU32 = AtomicU32::new(0);

struct TestDb {
    pool: Pool,
    path: PathBuf,
}

impl Drop for TestDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }
}

async fn test_db() -> TestDb {
    let path = std::env::temp_dir().join(format!(
        "vv-engine-test-{}-{}.db",
        std::process::id(),
        NEXT_DB.fetch_add(1, AtomicOrdering::Relaxed)
    ));
    let pool = db::build_pool(&path);
    db::run_migrations(&pool, &std::env::temp_dir()).await;
    TestDb { pool, path }
}

fn scheduler(db: &TestDb) -> (Scheduler, broadcast::Receiver<()>) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let (tx, rx) = broadcast::channel(64);
    (
        Scheduler::new(
            db.pool.clone(),
            tx,
            reqwest::Client::new(),
            crate::notification::tests_support::test_key(),
        ),
        rx,
    )
}

async fn insert(db: &TestDb, new: NewMonitor) -> i64 {
    db::interact(&db.pool, move |c| db_monitors::insert_monitor(c, &new))
        .await
        .unwrap()
}

fn push_monitor(name: &str) -> NewMonitor {
    NewMonitor {
        name: name.into(),
        monitor_type: MonitorType::Push,
        push_token: Some(format!("tok-{name}")),
        ..NewMonitor::default()
    }
}

/// Local heartbeats of a monitor, oldest first.
async fn statuses(db: &TestDb, id: i64) -> Vec<String> {
    db::interact(&db.pool, move |c| {
        let mut stmt = c.prepare(
            "SELECT status FROM heartbeats WHERE monitor_id = ?1 AND agent_id IS NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([id], |r| r.get(0))?;
        rows.collect()
    })
    .await
    .unwrap()
}

async fn monitor_events(db: &TestDb, id: i64) -> Vec<events::MonitorEvent> {
    db::interact(&db.pool, move |c| events::list_for_monitor(c, id, 50))
        .await
        .unwrap()
}

#[test]
fn a_channel_with_nobody_to_send_to_counts_neither_as_sent_nor_failed() {
    use crate::notification::Outcome;
    assert!(delivery_counts(&Ok(Outcome::Sent)));
    assert!(delivery_counts(&Err("failed".to_string())));
    assert!(!delivery_counts(&Ok(Outcome::NothingToSend)));
}

#[tokio::test]
async fn push_results_drive_state_and_outage_events() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(&db, push_monitor("p")).await;

    s.ingest_external(id, CheckOutcome::up(5), None).await;
    s.ingest_external(id, CheckOutcome::down("boom"), None)
        .await;
    let open = monitor_events(&db, id).await;
    assert_eq!(open.len(), 1);
    assert!(open[0].is_open() && open[0].kind == MonitorEventKind::Down);

    s.ingest_external(id, CheckOutcome::up(5), None).await;
    assert_eq!(statuses(&db, id).await, ["up", "down", "up"]);
    let closed = monitor_events(&db, id).await;
    assert!(!closed[0].is_open());

    let snap = s.status_snapshot(id).await.unwrap();
    assert_eq!(snap.status, Some(HeartbeatStatus::Up));
    assert_eq!(snap.consecutive_failures, 0);
}

#[tokio::test]
async fn retries_apply_to_external_results() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(
        &db,
        NewMonitor {
            retries: 1,
            ..push_monitor("p")
        },
    )
    .await;
    s.ingest_external(id, CheckOutcome::down("x"), None).await;
    s.ingest_external(id, CheckOutcome::down("x"), None).await;
    assert_eq!(statuses(&db, id).await, ["pending", "down"]);
}

#[tokio::test]
async fn agent_results_are_recorded_but_do_not_drive_state() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(
        &db,
        NewMonitor {
            name: "h".into(),
            target: "http://127.0.0.1:1".into(),
            upside_down: true,
            ..NewMonitor::default()
        },
    )
    .await;
    db::interact(&db.pool, |c| {
        c.execute(
            "INSERT INTO agents (id, name, prefix, token_hash) VALUES (7, 'eu', 'p', 'h')",
            [],
        )
    })
    .await
    .unwrap();

    s.ingest_external(id, CheckOutcome::up(3), Some(7)).await;

    let rows: Vec<(Option<i64>, String)> = db::interact(&db.pool, move |c| {
        let mut stmt =
            c.prepare("SELECT agent_id, status FROM heartbeats WHERE monitor_id = ?1")?;
        let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    })
    .await
    .unwrap();
    // Upside-down applied to the agent's raw result.
    assert_eq!(rows, [(Some(7), "down".to_string())]);
    assert!(s.status_snapshot(id).await.is_none());
    assert!(monitor_events(&db, id).await.is_empty());
}

#[tokio::test]
async fn results_for_paused_monitors_are_ignored() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(
        &db,
        NewMonitor {
            active: false,
            ..push_monitor("p")
        },
    )
    .await;
    s.ingest_external(id, CheckOutcome::up(1), None).await;
    assert!(statuses(&db, id).await.is_empty());
}

#[tokio::test]
async fn group_status_is_worst_of_children_and_recomputed_on_child_change() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let group = insert(
        &db,
        NewMonitor {
            name: "g".into(),
            monitor_type: MonitorType::Group,
            ..NewMonitor::default()
        },
    )
    .await;
    let a = insert(
        &db,
        NewMonitor {
            parent_id: Some(group),
            ..push_monitor("a")
        },
    )
    .await;
    let b = insert(
        &db,
        NewMonitor {
            parent_id: Some(group),
            ..push_monitor("b")
        },
    )
    .await;

    s.ingest_external(a, CheckOutcome::up(1), None).await;
    s.ingest_external(b, CheckOutcome::up(1), None).await; // same status, debounced
    s.ingest_external(b, CheckOutcome::down("dead"), None).await;
    assert_eq!(statuses(&db, group).await, ["up", "down"]);
    let group_events = monitor_events(&db, group).await;
    assert_eq!(group_events[0].message.as_deref(), Some("1 of 2 down: b"));

    s.ingest_external(b, CheckOutcome::up(1), None).await;
    assert_eq!(statuses(&db, group).await, ["up", "down", "up"]);
}

#[tokio::test]
async fn reloading_a_group_recomputes_it_without_a_false_cycle() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let group = insert(
        &db,
        NewMonitor {
            name: "g".into(),
            monitor_type: MonitorType::Group,
            ..NewMonitor::default()
        },
    )
    .await;
    for name in ["a", "b"] {
        insert(
            &db,
            NewMonitor {
                parent_id: Some(group),
                ..push_monitor(name)
            },
        )
        .await;
    }
    // Children report while the group is paused: nothing recorded for it.
    db::interact(&db.pool, move |c| db_monitors::set_active(c, group, false))
        .await
        .unwrap();
    let kids: Vec<i64> = db::interact(&db.pool, move |c| db_monitors::children(c, group))
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    s.ingest_external(kids[0], CheckOutcome::down("dead"), None)
        .await;
    s.ingest_external(kids[1], CheckOutcome::up(1), None).await;
    assert!(statuses(&db, group).await.is_empty());

    // Resuming the group reloads it; it must aggregate its two children
    // (previously this tripped the cycle guard and recorded nothing).
    db::interact(&db.pool, move |c| db_monitors::set_active(c, group, true))
        .await
        .unwrap();
    s.reload(group).await;
    assert_eq!(statuses(&db, group).await, ["down"]);
}

#[tokio::test]
async fn nested_groups_propagate_upwards() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let group = |name: &str, parent: Option<i64>| NewMonitor {
        name: name.into(),
        monitor_type: MonitorType::Group,
        parent_id: parent,
        ..NewMonitor::default()
    };
    let top = insert(&db, group("top", None)).await;
    let mid = insert(&db, group("mid", Some(top))).await;
    let leaf = insert(
        &db,
        NewMonitor {
            parent_id: Some(mid),
            ..push_monitor("leaf")
        },
    )
    .await;
    s.ingest_external(leaf, CheckOutcome::down("x"), None).await;
    assert_eq!(statuses(&db, mid).await, ["down"]);
    assert_eq!(statuses(&db, top).await, ["down"]);
}

#[tokio::test]
async fn expiry_is_stored_and_warned_once_per_step() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(&db, push_monitor("p")).await;
    let expiry = |days: i64| ExpiryInfo {
        kind: ExpiryKind::Cert,
        subject: Some("example.com".into()),
        issuer: Some("R3".into()),
        expires_at: fmt_db_time(
            OffsetDateTime::now_utc() + time::Duration::days(days) + time::Duration::hours(1),
        ),
    };
    let expiring = expiry(5);
    s.ingest_external(id, CheckOutcome::up(1).with_expiry(expiring.clone()), None)
        .await;
    s.ingest_external(id, CheckOutcome::up(1).with_expiry(expiring), None)
        .await;
    let warned: Vec<_> = monitor_events(&db, id)
        .await
        .into_iter()
        .filter(|e| e.kind == MonitorEventKind::CertExpiring)
        .collect();
    assert_eq!(warned.len(), 1);
    assert!(
        warned[0]
            .message
            .as_deref()
            .unwrap_or("")
            .starts_with("Certificate example.com expires in 5 days")
    );

    // Renewed: notified_at resets.
    s.ingest_external(id, CheckOutcome::up(1).with_expiry(expiry(90)), None)
        .await;
    let notified: Option<String> = db::interact(&db.pool, move |c| {
        c.query_row(
            "SELECT notified_at FROM monitor_expiry WHERE monitor_id = ?1",
            [id],
            |r| r.get(0),
        )
    })
    .await
    .unwrap();
    assert_eq!(notified, None);
}

#[tokio::test]
async fn expiry_is_persisted_and_warned_on_down_outcomes() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(&db, push_monitor("tls")).await;
    let expiry = ExpiryInfo {
        kind: ExpiryKind::Cert,
        subject: Some("expired.example".into()),
        issuer: None,
        expires_at: fmt_db_time(OffsetDateTime::now_utc() - time::Duration::days(2)),
    };
    let stored_expiry = expiry.expires_at.clone();
    s.ingest_external(
        id,
        CheckOutcome::down("certificate has expired").with_expiry(expiry),
        None,
    )
    .await;

    let (expires_at, notified): (String, Option<String>) = db::interact(&db.pool, move |c| {
        c.query_row(
            "SELECT expires_at, notified_at FROM monitor_expiry WHERE monitor_id = ?1 AND kind = 'cert'",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    })
    .await
    .unwrap();
    assert_eq!(expires_at, stored_expiry);
    assert!(notified.is_some());
    assert_eq!(statuses(&db, id).await, ["down"]);
    assert!(
        monitor_events(&db, id)
            .await
            .iter()
            .any(|e| e.kind == MonitorEventKind::CertExpiring)
    );
}

#[tokio::test]
async fn raw_pending_results_are_recorded_without_changing_state() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(
        &db,
        NewMonitor {
            upside_down: true,
            ..push_monitor("docker")
        },
    )
    .await;
    s.ingest_external(id, CheckOutcome::down("x"), None).await; // inverted → up
    let pending = CheckOutcome {
        status: HeartbeatStatus::Pending,
        response_time_ms: None,
        message: Some("health: starting".into()),
        expiry: None,
    };
    s.ingest_external(id, pending, None).await;
    assert_eq!(statuses(&db, id).await, ["up", "pending"]);
    let snap = s.status_snapshot(id).await.unwrap();
    assert_eq!(snap.consecutive_failures, 0);
    assert!(monitor_events(&db, id).await.is_empty());
}

#[tokio::test]
async fn reload_forgets_the_monitors_oauth_token() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(&db, push_monitor("p")).await;
    let tokens = checkers::http_auth::cache();
    let now = std::time::Instant::now();
    tokens.put(id, "reload-test", "tok", Some(3600), now);
    s.reload(id).await;
    assert_eq!(tokens.get(id, "reload-test", now), None);
}

#[tokio::test]
async fn reload_installs_and_stops_tasks_without_double_spawn() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(&db, push_monitor("p")).await;
    s.reload(id).await;
    s.reload(id).await;
    assert!(s.is_running(id));
    assert_eq!(lock(&s.inner.tasks).len(), 1);

    db::interact(&db.pool, move |c| {
        c.execute("UPDATE monitors SET active = 0 WHERE id = ?1", [id])
    })
    .await
    .unwrap();
    s.reload(id).await;
    assert!(!s.is_running(id));
    assert!(s.status_snapshot(id).await.is_none());
}

#[tokio::test]
async fn deleted_monitor_reload_stops_task() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(&db, push_monitor("p")).await;
    s.reload(id).await;
    db::interact(&db.pool, move |c| db_monitors::delete_monitor(c, id))
        .await
        .unwrap();
    s.reload(id).await;
    assert!(!s.is_running(id));
}

#[tokio::test]
async fn push_watchdog_marks_missing_push_down() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(
        &db,
        NewMonitor {
            interval_seconds: 1,
            ..push_monitor("p")
        },
    )
    .await;
    s.reload(id).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        if statuses(&db, id).await.first().map(String::as_str) == Some("down") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "watchdog never recorded a missed push"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    s.shutdown().await;
    assert!(!s.is_running(id));
}

#[tokio::test]
async fn check_now_runs_a_probe_and_records_it() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let id = insert(
        &db,
        NewMonitor {
            name: "tcp".into(),
            monitor_type: MonitorType::Tcp,
            target: format!("127.0.0.1:{port}"),
            timeout_seconds: 2,
            ..NewMonitor::default()
        },
    )
    .await;
    assert_eq!(s.check_now(id).await, Some(HeartbeatStatus::Up));
    assert_eq!(statuses(&db, id).await, ["up"]);
}

#[tokio::test]
async fn check_now_is_none_for_push_and_group() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(&db, push_monitor("p")).await;
    assert_eq!(s.check_now(id).await, None);
}

#[tokio::test]
async fn shutdown_makes_reload_a_no_op() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(&db, push_monitor("p")).await;
    s.shutdown().await;
    s.reload(id).await;
    assert!(!s.is_running(id));
}

#[tokio::test]
async fn update_signals_are_coalesced() {
    let (tx, mut rx) = broadcast::channel(64);
    let c = Arc::new(UpdateCoalescer::new(tx));
    for _ in 0..20 {
        c.notify();
    }
    tokio::time::sleep(UPDATE_MIN_GAP * 2).await;
    let mut received = 0;
    while rx.try_recv().is_ok() {
        received += 1;
    }
    assert_eq!(received, 2, "one leading + one trailing signal");
}

#[test]
fn push_deadline_uses_grace_then_retry_interval() {
    let t = parse_db_time("2026-03-10 12:00:00").unwrap();
    let grace = Duration::from_secs(66);
    let retry = Duration::from_secs(30);
    assert_eq!(push_deadline(t, None, grace, retry), t + grace);
    let miss = t + grace;
    assert_eq!(push_deadline(t, Some(miss), grace, retry), miss + retry);
    // A push after the miss resets to the grace window.
    let push = miss + Duration::from_secs(5);
    assert_eq!(push_deadline(push, Some(miss), grace, retry), push + grace);
}

#[test]
fn checks_keep_their_phase_one_interval_apart() {
    let interval = Duration::from_secs(60);
    let t0 = tokio::time::Instant::now() + Duration::from_secs(5);
    let first = next_due(t0, interval, 7);
    let gap = first - t0;
    assert!(gap >= interval / 2 && gap < interval * 3 / 2, "{gap:?}");
    // On schedule, or started a little late: the next one stays on the slot.
    assert_eq!(next_due(first, interval, 7) - first, interval);
    let late = first + Duration::from_secs(3);
    assert_eq!(next_due(late, interval, 7), first + interval);
}

#[test]
fn monitors_created_together_spread_over_the_interval() {
    let interval = Duration::from_secs(60);
    let t0 = tokio::time::Instant::now() + Duration::from_secs(5);
    // 200 monitors created within the same second check in lockstep at first.
    let mut slots: Vec<u64> = (1..=200)
        .map(|id| (next_due(t0, interval, id) - t0).as_secs())
        .collect();
    slots.sort_unstable();
    slots.dedup();
    assert!(
        slots.len() > 40,
        "second checks should spread over the minute: {} distinct seconds",
        slots.len()
    );
}

#[test]
fn start_jitter_stays_within_window_and_spreads_monitors() {
    let delays: Vec<Duration> = (1..=200).map(|id| start_jitter(id, 60)).collect();
    assert!(delays.iter().all(|d| *d < Duration::from_secs(30)));
    let mut distinct: Vec<u128> = delays.iter().map(|d| d.as_millis() / 100).collect();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(
        distinct.len() > 100,
        "200 monitors should land in many different 100ms slots"
    );
    assert!(start_jitter(5, 10) < Duration::from_secs(10));
}

// -- agent-checked monitors ---------------------------------------------------

/// A scheduler whose agent watchdog fires 2 × interval after the last result.
fn fast_agent_scheduler(db: &TestDb) -> Scheduler {
    scheduler(db)
        .0
        .with_agent_timing(Duration::ZERO, Duration::ZERO)
}

async fn insert_agent(db: &TestDb, id: i64, name: &str) {
    let name = name.to_string();
    db::interact(&db.pool, move |c| {
        c.execute(
            "INSERT INTO agents (id, name, prefix, token_hash) VALUES (?1, ?2, 'p', 'h' || ?1)",
            params![id, name],
        )
    })
    .await
    .unwrap();
}

/// A TCP monitor on a port nothing listens on (a local check would be down),
/// checked by agent `agent_id`.
fn agent_checked(agent_id: i64) -> NewMonitor {
    NewMonitor {
        name: "home-lan".into(),
        monitor_type: MonitorType::Tcp,
        target: "127.0.0.1:1".into(),
        interval_seconds: 1,
        timeout_seconds: 1,
        check_from_agent_id: Some(agent_id),
        ..NewMonitor::default()
    }
}

/// `(agent_id, status, message)` of every heartbeat, oldest first.
async fn all_heartbeats(db: &TestDb, id: i64) -> Vec<(Option<i64>, String, Option<String>)> {
    db::interact(&db.pool, move |c| {
        let mut stmt = c.prepare(
            "SELECT agent_id, status, message FROM heartbeats WHERE monitor_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    })
    .await
    .unwrap()
}

/// Waits until the monitor's latest local heartbeat matches.
async fn wait_for_local(db: &TestDb, id: i64, secs: u64, want: impl Fn(&str, &str) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let rows = all_heartbeats(db, id).await;
        if let Some((_, status, message)) = rows.iter().rev().find(|(a, _, _)| a.is_none())
            && want(status, message.as_deref().unwrap_or(""))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no matching heartbeat; got {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn agent_checked_monitors_are_never_checked_locally() {
    let db = test_db().await;
    let s = fast_agent_scheduler(&db);
    insert_agent(&db, 7, "home").await;
    let id = insert(
        &db,
        NewMonitor {
            interval_seconds: 3600,
            ..agent_checked(7)
        },
    )
    .await;
    s.reload(id).await;
    assert!(s.is_running(id), "the agent watchdog runs");
    assert_eq!(s.check_now(id).await, None, "no on-demand local check");
    tokio::time::sleep(Duration::from_millis(1_800)).await;
    assert!(
        all_heartbeats(&db, id).await.is_empty(),
        "nothing ran locally"
    );
    s.shutdown().await;
}

#[tokio::test]
async fn the_checking_agent_drives_status_history_and_notifications() {
    use axum::{Router, routing::post};
    use std::sync::Mutex;

    // A webhook that records the events it receives.
    let received: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = received.clone();
    let app = Router::new().route(
        "/hook",
        post(move |body: String| {
            let sink = sink.clone();
            async move {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                sink.lock()
                    .unwrap()
                    .push(v["event"].as_str().unwrap_or_default().to_string());
                "ok"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let db = test_db().await;
    let s = fast_agent_scheduler(&db);
    insert_agent(&db, 7, "home").await;
    insert_agent(&db, 8, "other").await;
    let id = insert(
        &db,
        NewMonitor {
            interval_seconds: 3600,
            ..agent_checked(7)
        },
    )
    .await;
    let key = crate::notification::tests_support::test_key();
    let config =
        crate::notification::seal_config(&key, "webhook", &serde_json::json!({ "url": url }));
    db::interact(&db.pool, move |c| {
        let ch = db_notifications::insert_channel(c, "hook", "webhook", &config, false)?;
        db_notifications::set_monitor_channels(c, id, &[(ch, false)])
    })
    .await
    .unwrap();

    s.ingest_external(id, CheckOutcome::up(4), Some(7)).await;
    s.ingest_external(id, CheckOutcome::down("connection refused"), Some(7))
        .await;
    // Another agent's result is only recorded under its own id.
    s.ingest_external(id, CheckOutcome::up(2), Some(8)).await;
    s.ingest_external(id, CheckOutcome::up(4), Some(7)).await;

    assert_eq!(statuses(&db, id).await, ["up", "down", "up"]);
    let agent_rows: Vec<Option<i64>> = all_heartbeats(&db, id)
        .await
        .into_iter()
        .filter_map(|(a, _, _)| a)
        .map(Some)
        .collect();
    assert_eq!(agent_rows, [Some(7), Some(7), Some(8), Some(7)]);
    let events = monitor_events(&db, id).await;
    assert_eq!(events.len(), 1);
    assert!(events[0].kind == MonitorEventKind::Down && !events[0].is_open());
    assert_eq!(
        s.status_snapshot(id).await.unwrap().status,
        Some(HeartbeatStatus::Up)
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while received.lock().unwrap().len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "notifications never arrived"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(*received.lock().unwrap(), ["down", "up"]);
}

#[tokio::test]
async fn a_silent_agent_marks_its_monitors_down_until_it_reports_again() {
    let db = test_db().await;
    let s = fast_agent_scheduler(&db);
    insert_agent(&db, 7, "home").await;
    let id = insert(&db, agent_checked(7)).await;
    s.reload(id).await;
    s.ingest_external(id, CheckOutcome::up(4), Some(7)).await;

    // interval 1 s: nothing for > 2 s.
    wait_for_local(&db, id, 6, |status, message| {
        status == "down" && message.starts_with("Agent home has not reported for ")
    })
    .await;
    let (_, _, message) = all_heartbeats(&db, id).await.pop().unwrap();
    assert!(message.unwrap().ends_with(" s"));

    s.ingest_external(id, CheckOutcome::up(4), Some(7)).await;
    assert_eq!(
        s.status_snapshot(id).await.unwrap().status,
        Some(HeartbeatStatus::Up)
    );
    assert_eq!(
        statuses(&db, id).await.last().map(String::as_str),
        Some("up")
    );
    s.shutdown().await;
}

#[tokio::test]
async fn a_silent_agent_is_down_even_for_upside_down_monitors() {
    let db = test_db().await;
    let s = fast_agent_scheduler(&db);
    insert_agent(&db, 7, "home").await;
    let id = insert(
        &db,
        NewMonitor {
            upside_down: true,
            ..agent_checked(7)
        },
    )
    .await;
    s.reload(id).await;
    wait_for_local(&db, id, 6, |status, message| {
        status == "down" && message.starts_with("Agent home has not reported")
    })
    .await;
    s.shutdown().await;
}

#[tokio::test]
async fn a_disabled_agent_marks_its_monitors_down() {
    let db = test_db().await;
    let s = fast_agent_scheduler(&db);
    insert_agent(&db, 7, "home").await;
    let id = insert(
        &db,
        NewMonitor {
            interval_seconds: 3600,
            ..agent_checked(7)
        },
    )
    .await;
    db::interact(&db.pool, |c| crate::db::agents::set_active(c, 7, false))
        .await
        .unwrap();
    s.reload(id).await;
    wait_for_local(&db, id, 3, |status, message| {
        status == "down" && message == "Agent home is disabled"
    })
    .await;
    s.shutdown().await;
}

#[tokio::test]
async fn a_deleted_agent_pauses_its_monitors() {
    let db = test_db().await;
    let s = fast_agent_scheduler(&db);
    insert_agent(&db, 7, "home").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let id = insert(
        &db,
        NewMonitor {
            target: format!("127.0.0.1:{port}"),
            interval_seconds: 3600,
            ..agent_checked(7)
        },
    )
    .await;
    s.reload(id).await;
    assert!(s.is_running(id));

    let deleted = db::interact(&db.pool, |c| {
        crate::db::agents::delete_agent_pausing_monitors(c, 7)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        (deleted.checked.clone(), deleted.paused),
        (vec![id], vec![id])
    );
    s.reload(id).await;
    let m = db::interact(&db.pool, move |c| db_monitors::get_monitor(c, id))
        .await
        .unwrap()
        .unwrap();
    assert!(!m.active, "paused");
    assert_eq!(m.check_from_agent_id, None, "the foreign key clears it");
    assert!(!s.is_running(id), "nothing runs for it");
    assert_eq!(s.check_now(id).await, None);
    tokio::time::sleep(Duration::from_millis(1_600)).await;
    assert!(statuses(&db, id).await.is_empty(), "no local check ran");
    let events = monitor_events(&db, id).await;
    assert!(
        events
            .iter()
            .any(|e| e.kind == MonitorEventKind::Paused && e.is_open())
    );

    // Resumed later: this server checks it.
    db::interact(&db.pool, move |c| db_monitors::set_active(c, id, true))
        .await
        .unwrap();
    s.reload(id).await;
    wait_for_local(&db, id, 5, |status, _| status == "up").await;
    assert_eq!(s.check_now(id).await, Some(HeartbeatStatus::Up));
    s.shutdown().await;
}
