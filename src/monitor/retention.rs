//! Hourly retention job. Owned by the engine agent.
//!
//! Each run (first ~1 minute after start, then hourly):
//! 1. rolls heartbeats up into `heartbeat_daily` (see [`crate::db::rollups`]),
//! 2. deletes raw heartbeats from whole days older than `retention_days`, in
//!    small batches so the write lock is never held for long,
//! 3. prunes closed `monitor_events` older than max(retention, 365) days,
//! 4. runs `PRAGMA optimize` (+ `incremental_vacuum` when the database uses
//!    `auto_vacuum = INCREMENTAL`; never a full VACUUM).

use std::time::{Duration, Instant};

use deadpool_sqlite::Pool;
use rusqlite::{Connection, params};

use crate::api::AppState;
use crate::db::{self, DbError, events, rollups, settings};

const FIRST_RUN_DELAY: Duration = Duration::from_secs(60);
const RUN_EVERY: Duration = Duration::from_secs(3600);
/// Heartbeats deleted per statement.
const DELETE_BATCH: i64 = 5_000;
/// Pause between delete batches so checks can write in between.
const BATCH_PAUSE: Duration = Duration::from_millis(25);
/// Monitor events are kept at least this long (they're small and make the
/// incident history useful).
const MIN_EVENT_RETENTION_DAYS: i64 = 365;
/// Pages released per run when incremental auto-vacuum is enabled.
const INCREMENTAL_VACUUM_PAGES: i64 = 2_000;

/// What one retention run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionReport {
    pub rollup_rows: usize,
    pub heartbeats_deleted: usize,
    pub events_deleted: usize,
}

/// Called once at server start (see `server::run`).
pub fn spawn(state: AppState) {
    let pool = state.db.clone();
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_RUN_DELAY).await;
        let mut ticker = tokio::time::interval(RUN_EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(err) = run_once(&pool).await {
                tracing::error!(%err, "retention run failed");
            }
        }
    });
}

/// Cutoffs for a run at `now`: (heartbeats before this day are deleted,
/// closed events that started before this timestamp are deleted).
fn cutoffs(conn: &Connection, now: &str, retention_days: i64) -> rusqlite::Result<(String, String)> {
    let retention_days = retention_days.max(1);
    let event_days = retention_days.max(MIN_EVENT_RETENTION_DAYS);
    conn.query_row(
        "SELECT date(?1, ?2), datetime(?1, ?3)",
        params![now, format!("-{retention_days} days"), format!("-{event_days} days")],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

fn optimize(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA optimize;")?;
    let auto_vacuum: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    if auto_vacuum == 2 {
        conn.execute_batch(&format!("PRAGMA incremental_vacuum({INCREMENTAL_VACUUM_PAGES});"))?;
    }
    Ok(())
}

/// One full retention run against the pool (batched, yields between batches).
pub async fn run_once(pool: &Pool) -> Result<RetentionReport, DbError> {
    let started = Instant::now();
    let (now, retention_days) = db::interact(pool, |c| {
        let now: String = c.query_row("SELECT datetime('now')", [], |r| r.get(0))?;
        Ok((now, settings::load(c)?.retention_days))
    })
    .await?;
    let mut report = RetentionReport::default();

    let rollup_now = now.clone();
    report.rollup_rows = db::interact(pool, move |c| rollups::rollup(c, &rollup_now)).await?;

    let cutoff_now = now.clone();
    let (heartbeat_cutoff, event_cutoff) = db::interact(pool, move |c| cutoffs(c, &cutoff_now, retention_days)).await?;
    loop {
        let cutoff = heartbeat_cutoff.clone();
        let deleted = db::interact(pool, move |c| {
            rollups::delete_heartbeats_before(c, &cutoff, DELETE_BATCH)
        })
        .await?;
        report.heartbeats_deleted += deleted;
        if i64::try_from(deleted).unwrap_or(i64::MAX) < DELETE_BATCH {
            break;
        }
        tokio::time::sleep(BATCH_PAUSE).await;
    }

    report.events_deleted = db::interact(pool, move |c| events::prune_closed_before(c, &event_cutoff)).await?;
    db::interact(pool, |c| optimize(c)).await?;

    tracing::info!(
        rollup_rows = report.rollup_rows,
        heartbeats_deleted = report.heartbeats_deleted,
        events_deleted = report.events_deleted,
        retention_days,
        heartbeat_cutoff = %heartbeat_cutoff,
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "retention run finished"
    );
    Ok(report)
}

/// The same steps as [`run_once`] on one connection without pauses, relative
/// to an explicit `now` (UTC `YYYY-MM-DD HH:MM:SS`). For tests and tooling.
pub fn run_blocking(conn: &Connection, now: &str) -> rusqlite::Result<RetentionReport> {
    let retention_days = settings::load(conn)?.retention_days;
    let mut report = RetentionReport {
        rollup_rows: rollups::rollup(conn, now)?,
        ..RetentionReport::default()
    };
    let (heartbeat_cutoff, event_cutoff) = cutoffs(conn, now, retention_days)?;
    loop {
        let deleted = rollups::delete_heartbeats_before(conn, &heartbeat_cutoff, DELETE_BATCH)?;
        report.heartbeats_deleted += deleted;
        if deleted == 0 {
            break;
        }
    }
    report.events_deleted = events::prune_closed_before(conn, &event_cutoff)?;
    optimize(conn)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::events::MonitorEventKind;
    use crate::db::models::NewMonitor;
    use crate::db::{monitors, test_conn};

    const NOW: &str = "2026-06-15 12:00:00";

    fn setup() -> (Connection, i64) {
        let conn = test_conn();
        let id = monitors::insert_monitor(
            &conn,
            &NewMonitor {
                name: "m".into(),
                ..NewMonitor::default()
            },
        )
        .unwrap();
        settings::set(&conn, settings::RETENTION_DAYS, "30").unwrap();
        (conn, id)
    }

    fn beat(conn: &Connection, id: i64, status: &str, at: &str) {
        conn.execute(
            "INSERT INTO heartbeats (monitor_id, status, checked_at) VALUES (?1, ?2, ?3)",
            params![id, status, at],
        )
        .unwrap();
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn deletes_whole_days_older_than_retention_and_keeps_rollups() {
        let (conn, id) = setup();
        beat(&conn, id, "up", "2026-05-01 10:00:00"); // 45 days old
        beat(&conn, id, "down", "2026-05-15 23:59:59"); // day before cutoff day
        beat(&conn, id, "up", "2026-05-16 00:00:00"); // cutoff day: kept whole
        beat(&conn, id, "up", "2026-06-15 11:00:00");
        let report = run_blocking(&conn, NOW).unwrap();
        assert_eq!(report.heartbeats_deleted, 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM heartbeats"), 2);
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM heartbeat_daily WHERE day IN ('2026-05-01', '2026-05-15')"
            ),
            2
        );
    }

    #[test]
    fn long_range_uptime_survives_retention() {
        let (conn, id) = setup();
        beat(&conn, id, "up", "2026-04-01 10:00:00");
        beat(&conn, id, "down", "2026-06-15 10:00:00");
        run_blocking(&conn, NOW).unwrap();
        assert_eq!(rollups::uptime_pct_at(&conn, id, 90, NOW).unwrap(), Some(50.0));
    }

    #[test]
    fn repeated_runs_are_idempotent() {
        let (conn, id) = setup();
        beat(&conn, id, "up", "2026-04-01 10:00:00");
        run_blocking(&conn, NOW).unwrap();
        let second = run_blocking(&conn, NOW).unwrap();
        assert_eq!(second.heartbeats_deleted, 0);
        assert_eq!(
            count(&conn, "SELECT up_count FROM heartbeat_daily WHERE day = '2026-04-01'"),
            1
        );
    }

    #[test]
    fn prunes_old_closed_events_but_keeps_open_ones() {
        let (conn, id) = setup();
        events::log_point(&conn, id, MonitorEventKind::Resumed, None, Some("2025-01-01 00:00:00")).unwrap();
        events::log_point(&conn, id, MonitorEventKind::Resumed, None, Some("2026-01-01 00:00:00")).unwrap();
        events::open(&conn, id, MonitorEventKind::Down, None, Some("2025-01-01 00:00:00")).unwrap();
        let report = run_blocking(&conn, NOW).unwrap();
        assert_eq!(report.events_deleted, 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM monitor_events"), 2);
    }

    #[test]
    fn event_retention_follows_longer_heartbeat_retention() {
        let (conn, id) = setup();
        settings::set(&conn, settings::RETENTION_DAYS, "730").unwrap();
        events::log_point(&conn, id, MonitorEventKind::Resumed, None, Some("2025-01-01 00:00:00")).unwrap();
        assert_eq!(run_blocking(&conn, NOW).unwrap().events_deleted, 0);
    }
}
