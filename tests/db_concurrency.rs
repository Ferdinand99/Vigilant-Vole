//! Database concurrency under load (both found by the load test,
//! tools/loadtest):
//!
//! - The check engine has its own connections: a busy web UI that holds every
//!   connection of the web pool must not stop heartbeats from being recorded
//!   (with one shared pool, checks fell minutes behind while dashboards and
//!   status pages queued).
//! - Pooled transactions take the write lock when they begin, so a
//!   read-then-write transaction (bulk edit) waits for a concurrent heartbeat
//!   write instead of failing with "database is locked".
//! - The WAL shrinks back after a burst of writes (the first retention run
//!   after lowering the retention deletes millions of rows).

use std::time::Duration;

use rusqlite::Connection;

use vigilant_vole::config::Config;
use vigilant_vole::db::{self, models::CheckOutcome, models::MonitorType, models::NewMonitor};
use vigilant_vole::server;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "vv-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

#[tokio::test]
async fn pooled_transactions_lock_out_writers_until_they_commit() {
    let dir = temp_dir("immediate-tx");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    let pool = db::build_pool(&path);
    db::run_migrations(&pool, &dir.join("backups")).await;
    let conn = pool.get().await.unwrap();
    let result = conn
        .interact(move |c| -> rusqlite::Result<()> {
            let tx = c.transaction()?;
            // Read first, as the bulk editor does...
            tx.query_row("SELECT COUNT(*) FROM monitors", [], |r| r.get::<_, i64>(0))?;
            // ...while another connection (the engine) tries to commit.
            let engine = Connection::open(&path)?;
            engine.busy_timeout(Duration::ZERO)?;
            let slipped_in = engine
                .execute("INSERT INTO tags (name) VALUES ('engine')", [])
                .is_ok();
            assert!(!slipped_in, "a deferred transaction let a writer in");
            // The write after the read can't hit a stale snapshot now.
            tx.execute("INSERT INTO tags (name) VALUES ('bulk')", [])?;
            tx.commit()
        })
        .await
        .unwrap();
    assert!(result.is_ok(), "{result:?}");
    drop(conn);
    pool.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_wal_is_cut_back_after_a_burst_of_writes() {
    const LIMIT: u64 = 64 * 1024 * 1024;
    let dir = temp_dir("wal-limit");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    let pool = db::build_pool(&path);
    db::run_migrations(&pool, &dir.join("backups")).await;
    let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
    let conn = pool.get().await.unwrap();
    conn.interact(|c| -> rusqlite::Result<()> {
        c.execute_batch("CREATE TABLE burst (b BLOB)")?;
        let tx = c.transaction()?;
        for _ in 0..80 {
            tx.execute("INSERT INTO burst VALUES (zeroblob(1048576))", [])?;
        }
        tx.commit()
    })
    .await
    .unwrap()
    .unwrap();
    let grown = std::fs::metadata(&wal).unwrap().len();
    assert!(grown > LIMIT, "the burst should outgrow the limit: {grown}");
    conn.interact(|c| -> rusqlite::Result<()> {
        c.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
        // The next write restarts the WAL from the top and truncates it.
        c.execute("DELETE FROM burst", [])?;
        Ok(())
    })
    .await
    .unwrap()
    .unwrap();
    let after = std::fs::metadata(&wal).unwrap().len();
    assert!(after <= LIMIT, "WAL still {after} bytes");
    drop(conn);
    pool.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn heartbeats_are_recorded_while_the_web_pool_is_exhausted() {
    let dir = temp_dir("engine-pool");
    let state = server::prepare(Config {
        data_dir: dir.clone(),
        bind_addr: "127.0.0.1:0".into(),
    })
    .await
    .expect("server prepares");
    let id = db::interact(&state.db, |c| {
        db::monitors::insert_monitor(
            c,
            &NewMonitor {
                name: "nightly backup".into(),
                monitor_type: MonitorType::Push,
                push_token: Some("engine-pool-test-token".into()),
                ..NewMonitor::default()
            },
        )
    })
    .await
    .unwrap();

    // Hold every web connection, like a pile of slow page renders would.
    let mut held = Vec::new();
    for _ in 0..state.db.status().max_size {
        held.push(state.db.get().await.unwrap());
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        state
            .scheduler
            .ingest_external(id, CheckOutcome::up(12), None),
    )
    .await
    .expect("the engine waited for the web pool");
    drop(held);

    let recorded: i64 = db::interact(&state.db, move |c| {
        c.query_row(
            "SELECT COUNT(*) FROM heartbeats WHERE monitor_id = ?1",
            [id],
            |r| r.get(0),
        )
    })
    .await
    .unwrap();
    assert_eq!(recorded, 1);
    state.scheduler.shutdown().await;
    drop(state);
    let _ = std::fs::remove_dir_all(&dir);
}
