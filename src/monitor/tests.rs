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
    (Scheduler::new(db.pool.clone(), tx, reqwest::Client::new()), rx)
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
        let mut stmt =
            c.prepare("SELECT status FROM heartbeats WHERE monitor_id = ?1 AND agent_id IS NULL ORDER BY id")?;
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

#[tokio::test]
async fn push_results_drive_state_and_outage_events() {
    let db = test_db().await;
    let (s, _rx) = scheduler(&db);
    let id = insert(&db, push_monitor("p")).await;

    s.ingest_external(id, CheckOutcome::up(5), None).await;
    s.ingest_external(id, CheckOutcome::down("boom"), None).await;
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
        let mut stmt = c.prepare("SELECT agent_id, status FROM heartbeats WHERE monitor_id = ?1")?;
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
        insert(&db, NewMonitor { parent_id: Some(group), ..push_monitor(name) }).await;
    }
    // Children report while the group is paused: nothing recorded for it.
    db::interact(&db.pool, move |c| db_monitors::set_active(c, group, false)).await.unwrap();
    let kids: Vec<i64> = db::interact(&db.pool, move |c| db_monitors::children(c, group))
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    s.ingest_external(kids[0], CheckOutcome::down("dead"), None).await;
    s.ingest_external(kids[1], CheckOutcome::up(1), None).await;
    assert!(statuses(&db, group).await.is_empty());

    // Resuming the group reloads it; it must aggregate its two children
    // (previously this tripped the cycle guard and recorded nothing).
    db::interact(&db.pool, move |c| db_monitors::set_active(c, group, true)).await.unwrap();
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
        expires_at: fmt_db_time(OffsetDateTime::now_utc() + time::Duration::days(days) + time::Duration::hours(1)),
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
