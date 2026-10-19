//! Daily heartbeat rollups (`heartbeat_daily`) and long-range uptime math.
//! Owned by the engine agent.
//!
//! Raw heartbeats are deleted after `retention_days`, so anything that looks
//! further back (status page 90-day bars, 30/90/365-day uptime) combines
//! rollups for complete past days with live aggregation of raw heartbeats for
//! recent days.
//!
//! **Watermark.** The hourly job ([`rollup`]) re-aggregates every day from the
//! stored watermark (inclusive) through today and then sets the watermark to
//! yesterday. Days `<= watermark` are therefore complete in `heartbeat_daily`;
//! days `> watermark` are read live from `heartbeats`. Retention only ever
//! deletes whole days strictly before `today - retention_days`, which is never
//! after the watermark, so no day is re-aggregated from partial data.
//!
//! Only local heartbeats (`agent_id IS NULL`) count; remote-agent results are
//! confirmation data, not the monitor's own status. Days are UTC.
//!
//! Uptime % = `up / (up + down)`; `pending` (retrying / unconfirmed) and
//! `maintenance` are excluded from the denominator. No up/down data → `None`.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::settings;

/// `settings` key holding the last complete rolled-up day (`YYYY-MM-DD`).
pub const WATERMARK_KEY: &str = "engine_rollup_watermark";

/// Upper bound for `days` arguments (10 years) so a bad query parameter can't
/// make us build a giant vector.
const MAX_DAYS: i64 = 3660;

/// One day of a monitor's history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayStat {
    /// `YYYY-MM-DD` (UTC).
    pub day: String,
    pub up: i64,
    pub down: i64,
    pub pending: i64,
    pub maintenance: i64,
    pub avg_ms: Option<f64>,
    /// `None` when the day has no up/down heartbeats (no data / only maintenance).
    pub uptime_pct: Option<f64>,
}

impl DayStat {
    /// Total heartbeats that day.
    pub fn total(&self) -> i64 {
        self.up + self.down + self.pending + self.maintenance
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Counts {
    up: i64,
    down: i64,
    pending: i64,
    maintenance: i64,
    avg_ms: Option<f64>,
}

fn pct(up: i64, down: i64) -> Option<f64> {
    let total = up + down;
    (total > 0).then(|| 100.0 * up as f64 / total as f64)
}

/// The last complete rolled-up day, if the rollup job has ever run.
pub fn watermark(conn: &Connection) -> rusqlite::Result<Option<String>> {
    settings::get(conn, WATERMARK_KEY)
}

/// Re-aggregates heartbeats into `heartbeat_daily` from the watermark
/// (inclusive; from the oldest heartbeat on first run) through `now`'s day,
/// then moves the watermark to the day before `now`. Idempotent. `now` is a
/// UTC `YYYY-MM-DD HH:MM:SS` timestamp. Returns the number of rows upserted.
pub fn rollup(conn: &Connection, now: &str) -> rusqlite::Result<usize> {
    let from: Option<String> = match watermark(conn)? {
        Some(day) => Some(day),
        None => conn
            .query_row(
                "SELECT date(MIN(checked_at)) FROM heartbeats WHERE agent_id IS NULL",
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten(),
    };
    let mut upserted = 0;
    if let Some(from) = from {
        upserted = conn.execute(
            "INSERT INTO heartbeat_daily
                 (monitor_id, day, up_count, down_count, pending_count, maintenance_count, avg_response_ms)
             SELECT monitor_id, date(checked_at),
                    SUM(status = 'up'), SUM(status = 'down'), SUM(status = 'pending'), SUM(status = 'maintenance'),
                    AVG(response_time_ms)
             FROM heartbeats
             WHERE agent_id IS NULL AND checked_at >= ?1 AND checked_at < date(?2, '+1 day')
             GROUP BY monitor_id, date(checked_at)
             ON CONFLICT (monitor_id, day) DO UPDATE SET
                 up_count = excluded.up_count,
                 down_count = excluded.down_count,
                 pending_count = excluded.pending_count,
                 maintenance_count = excluded.maintenance_count,
                 avg_response_ms = excluded.avg_response_ms",
            params![from, now],
        )?;
    }
    let yesterday: String = conn.query_row("SELECT date(?1, '-1 day')", params![now], |r| r.get(0))?;
    settings::set(conn, WATERMARK_KEY, &yesterday)?;
    Ok(upserted)
}

/// Deletes up to `batch` local or agent heartbeats checked before `cutoff`
/// (a UTC day `YYYY-MM-DD` or timestamp). Call repeatedly until it returns 0;
/// small batches keep each write lock short.
pub fn delete_heartbeats_before(conn: &Connection, cutoff: &str, batch: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM heartbeats WHERE id IN (
             SELECT id FROM heartbeats WHERE checked_at < ?1 LIMIT ?2
         )",
        params![cutoff, batch],
    )
}

/// Per-day stats for the last `days` days (oldest → newest, always exactly
/// `days` entries, the last one being today so far). Days without data have
/// zero counts and `uptime_pct: None`.
pub fn daily_uptime(conn: &Connection, monitor_id: i64, days: i64) -> rusqlite::Result<Vec<DayStat>> {
    daily_uptime_at(conn, monitor_id, days, &now_text(conn)?)
}

/// [`daily_uptime`] relative to an explicit `now` (UTC `YYYY-MM-DD HH:MM:SS`).
pub fn daily_uptime_at(conn: &Connection, monitor_id: i64, days: i64, now: &str) -> rusqlite::Result<Vec<DayStat>> {
    let days = days.clamp(1, MAX_DAYS);
    let span = format!("-{} days", days - 1);
    let day_list: Vec<String> = {
        let mut stmt = conn.prepare(
            "WITH RECURSIVE d(day) AS (
                 SELECT date(?1, ?2)
                 UNION ALL SELECT date(day, '+1 day') FROM d WHERE day < date(?1)
             ) SELECT day FROM d",
        )?;
        let rows = stmt.query_map(params![now, span], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let Some(first) = day_list.first().cloned() else {
        return Ok(Vec::new());
    };
    let wm = watermark(conn)?;

    let mut by_day: HashMap<String, Counts> = HashMap::new();
    if let Some(wm) = &wm {
        let mut stmt = conn.prepare(
            "SELECT day, up_count, down_count, pending_count, maintenance_count, avg_response_ms
             FROM heartbeat_daily WHERE monitor_id = ?1 AND day >= ?2 AND day <= ?3",
        )?;
        let rows = stmt.query_map(params![monitor_id, first, wm], counts_row)?;
        for row in rows {
            let (day, c) = row?;
            by_day.insert(day, c);
        }
    }
    {
        let live_from: String = match &wm {
            Some(wm) => conn.query_row("SELECT max(?1, date(?2, '+1 day'))", params![first, wm], |r| r.get(0))?,
            None => first,
        };
        let mut stmt = conn.prepare(
            "SELECT date(checked_at) AS day,
                    SUM(status = 'up'), SUM(status = 'down'), SUM(status = 'pending'), SUM(status = 'maintenance'),
                    AVG(response_time_ms)
             FROM heartbeats
             WHERE monitor_id = ?1 AND agent_id IS NULL AND checked_at >= ?2 AND checked_at < date(?3, '+1 day')
             GROUP BY date(checked_at)",
        )?;
        let rows = stmt.query_map(params![monitor_id, live_from, now], counts_row)?;
        for row in rows {
            let (day, c) = row?;
            by_day.insert(day, c);
        }
    }

    Ok(day_list
        .into_iter()
        .map(|day| {
            let c = by_day.get(&day).copied().unwrap_or_default();
            DayStat {
                day,
                up: c.up,
                down: c.down,
                pending: c.pending,
                maintenance: c.maintenance,
                avg_ms: c.avg_ms,
                uptime_pct: pct(c.up, c.down),
            }
        })
        .collect())
}

fn counts_row(r: &rusqlite::Row) -> rusqlite::Result<(String, Counts)> {
    Ok((
        r.get(0)?,
        Counts {
            up: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            down: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            pending: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            maintenance: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            avg_ms: r.get(5)?,
        },
    ))
}

/// Uptime % over the trailing `days × 24h`, combining rollups (complete days
/// inside the window) and raw heartbeats (the partial first day and every day
/// after the watermark). Works for 1/7/30/90/365 days even after retention
/// has deleted the raw data. `None` when there is no up/down data.
pub fn uptime_pct(conn: &Connection, monitor_id: i64, days: i64) -> rusqlite::Result<Option<f64>> {
    uptime_pct_at(conn, monitor_id, days, &now_text(conn)?)
}

/// [`uptime_pct`] relative to an explicit `now` (UTC `YYYY-MM-DD HH:MM:SS`).
pub fn uptime_pct_at(conn: &Connection, monitor_id: i64, days: i64, now: &str) -> rusqlite::Result<Option<f64>> {
    let days = days.clamp(1, MAX_DAYS);
    let span = format!("-{days} days");
    let (window_start, first_full_day): (String, String) = conn.query_row(
        "SELECT datetime(?1, ?2), date(datetime(?1, ?2), '+1 day')",
        params![now, span],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let wm = watermark(conn)?;

    let (mut up, mut down) = (0_i64, 0_i64);
    if let Some(wm) = &wm {
        let (u, d): (Option<i64>, Option<i64>) = conn.query_row(
            "SELECT SUM(up_count), SUM(down_count) FROM heartbeat_daily
             WHERE monitor_id = ?1 AND day >= ?2 AND day <= ?3",
            params![monitor_id, first_full_day, wm],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        up += u.unwrap_or(0);
        down += d.unwrap_or(0);
    }
    // Raw part: everything in the window that the rolled-up range above did
    // not cover (the partial first day and every day after the watermark).
    let (u, d): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT SUM(status = 'up'), SUM(status = 'down') FROM heartbeats
         WHERE monitor_id = ?1 AND agent_id IS NULL AND checked_at >= ?2 AND checked_at <= ?3
           AND (?4 IS NULL OR checked_at < ?5 OR checked_at >= date(?4, '+1 day'))",
        params![monitor_id, window_start, now, wm, first_full_day],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    up += u.unwrap_or(0);
    down += d.unwrap_or(0);
    Ok(pct(up, down))
}

fn now_text(conn: &Connection) -> rusqlite::Result<String> {
    conn.query_row("SELECT datetime('now')", [], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewMonitor;
    use crate::db::{monitors, test_conn};

    const NOW: &str = "2026-03-10 12:00:00";

    fn monitor(conn: &Connection) -> i64 {
        monitors::insert_monitor(
            conn,
            &NewMonitor {
                name: "m".into(),
                ..NewMonitor::default()
            },
        )
        .unwrap()
    }

    fn beat(conn: &Connection, id: i64, status: &str, ms: Option<i64>, at: &str) {
        conn.execute(
            "INSERT INTO heartbeats (monitor_id, status, response_time_ms, checked_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, status, ms, at],
        )
        .unwrap();
    }

    fn agent_beat(conn: &Connection, id: i64, status: &str, at: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, prefix, token_hash) VALUES (1, 'a', 'p', 'h')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO heartbeats (monitor_id, agent_id, status, checked_at) VALUES (?1, 1, ?2, ?3)",
            params![id, status, at],
        )
        .unwrap();
    }

    fn daily_row(conn: &Connection, id: i64, day: &str) -> Option<(i64, i64, i64, i64, Option<f64>)> {
        conn.query_row(
            "SELECT up_count, down_count, pending_count, maintenance_count, avg_response_ms
             FROM heartbeat_daily WHERE monitor_id = ?1 AND day = ?2",
            params![id, day],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .unwrap()
    }

    #[test]
    fn rollup_aggregates_local_heartbeats_per_day() {
        let conn = test_conn();
        let id = monitor(&conn);
        beat(&conn, id, "up", Some(100), "2026-03-08 01:00:00");
        beat(&conn, id, "up", Some(300), "2026-03-08 02:00:00");
        beat(&conn, id, "down", None, "2026-03-08 03:00:00");
        beat(&conn, id, "pending", None, "2026-03-08 04:00:00");
        beat(&conn, id, "maintenance", None, "2026-03-08 05:00:00");
        agent_beat(&conn, id, "down", "2026-03-08 06:00:00");
        rollup(&conn, NOW).unwrap();
        assert_eq!(daily_row(&conn, id, "2026-03-08"), Some((2, 1, 1, 1, Some(200.0))));
    }

    #[test]
    fn rollup_is_idempotent_and_sets_watermark_to_yesterday() {
        let conn = test_conn();
        let id = monitor(&conn);
        beat(&conn, id, "up", Some(10), "2026-03-09 01:00:00");
        rollup(&conn, NOW).unwrap();
        rollup(&conn, NOW).unwrap();
        assert_eq!(daily_row(&conn, id, "2026-03-09").map(|r| r.0), Some(1));
        assert_eq!(watermark(&conn).unwrap().as_deref(), Some("2026-03-09"));
    }

    #[test]
    fn rollup_rerolls_watermark_day_to_pick_up_late_heartbeats() {
        let conn = test_conn();
        let id = monitor(&conn);
        beat(&conn, id, "up", None, "2026-03-09 01:00:00");
        rollup(&conn, "2026-03-09 23:30:00").unwrap(); // watermark = 03-08, 03-09 partial
        beat(&conn, id, "down", None, "2026-03-09 23:50:00");
        rollup(&conn, "2026-03-10 00:30:00").unwrap();
        assert_eq!(daily_row(&conn, id, "2026-03-09").map(|r| (r.0, r.1)), Some((1, 1)));
    }

    #[test]
    fn rollup_survives_raw_deletion() {
        let conn = test_conn();
        let id = monitor(&conn);
        beat(&conn, id, "up", None, "2026-01-01 10:00:00");
        rollup(&conn, NOW).unwrap();
        while delete_heartbeats_before(&conn, "2026-02-01", 1).unwrap() > 0 {}
        rollup(&conn, NOW).unwrap();
        assert_eq!(daily_row(&conn, id, "2026-01-01").map(|r| r.0), Some(1));
    }

    #[test]
    fn delete_in_batches_removes_only_old_rows() {
        let conn = test_conn();
        let id = monitor(&conn);
        for h in 0..5 {
            beat(&conn, id, "up", None, &format!("2026-01-01 0{h}:00:00"));
        }
        beat(&conn, id, "up", None, "2026-03-01 00:00:00");
        assert_eq!(delete_heartbeats_before(&conn, "2026-02-01", 2).unwrap(), 2);
        let mut total = 2;
        loop {
            let n = delete_heartbeats_before(&conn, "2026-02-01", 2).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, 5);
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM heartbeats", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1);
    }

    #[test]
    fn daily_uptime_returns_exactly_n_days_oldest_first_with_gaps() {
        let conn = test_conn();
        let id = monitor(&conn);
        beat(&conn, id, "up", Some(50), "2026-03-10 08:00:00");
        let stats = daily_uptime_at(&conn, id, 3, NOW).unwrap();
        let days: Vec<_> = stats.iter().map(|s| s.day.as_str()).collect();
        assert_eq!(days, ["2026-03-08", "2026-03-09", "2026-03-10"]);
        assert_eq!(stats[0].uptime_pct, None);
        assert_eq!(stats[2].uptime_pct, Some(100.0));
        assert_eq!(stats[2].avg_ms, Some(50.0));
    }

    #[test]
    fn daily_uptime_combines_rollups_and_live_data() {
        let conn = test_conn();
        let id = monitor(&conn);
        beat(&conn, id, "up", None, "2026-03-01 08:00:00");
        beat(&conn, id, "down", None, "2026-03-01 09:00:00");
        rollup(&conn, "2026-03-05 00:00:00").unwrap(); // watermark 03-04
        while delete_heartbeats_before(&conn, "2026-03-04", 100).unwrap() > 0 {}
        beat(&conn, id, "up", None, "2026-03-10 08:00:00");
        let stats = daily_uptime_at(&conn, id, 10, NOW).unwrap();
        assert_eq!(stats[0].day, "2026-03-01");
        assert_eq!(stats[0].uptime_pct, Some(50.0));
        assert_eq!(stats[9].uptime_pct, Some(100.0));
    }

    #[test]
    fn daily_uptime_ignores_stale_partial_rollup_of_today() {
        let conn = test_conn();
        let id = monitor(&conn);
        beat(&conn, id, "up", None, "2026-03-10 01:00:00");
        rollup(&conn, "2026-03-10 02:00:00").unwrap(); // today partial rolled
        beat(&conn, id, "down", None, "2026-03-10 03:00:00");
        let today = daily_uptime_at(&conn, id, 1, NOW).unwrap().remove(0);
        assert_eq!((today.up, today.down), (1, 1));
    }

    #[test]
    fn uptime_excludes_pending_and_maintenance() {
        let conn = test_conn();
        let id = monitor(&conn);
        beat(&conn, id, "up", None, "2026-03-10 08:00:00");
        beat(&conn, id, "up", None, "2026-03-10 08:01:00");
        beat(&conn, id, "up", None, "2026-03-10 08:02:00");
        beat(&conn, id, "down", None, "2026-03-10 08:03:00");
        beat(&conn, id, "pending", None, "2026-03-10 08:04:00");
        beat(&conn, id, "maintenance", None, "2026-03-10 08:05:00");
        assert_eq!(uptime_pct_at(&conn, id, 1, NOW).unwrap(), Some(75.0));
    }

    #[test]
    fn uptime_is_none_without_data() {
        let conn = test_conn();
        let id = monitor(&conn);
        assert_eq!(uptime_pct_at(&conn, id, 30, NOW).unwrap(), None);
    }

    #[test]
    fn uptime_24h_is_a_rolling_window_across_midnight() {
        let conn = test_conn();
        let id = monitor(&conn);
        beat(&conn, id, "down", None, "2026-03-09 11:00:00"); // outside 24h
        beat(&conn, id, "down", None, "2026-03-09 13:00:00"); // inside, yesterday
        beat(&conn, id, "up", None, "2026-03-10 10:00:00");
        rollup(&conn, NOW).unwrap(); // watermark = 03-09
        assert_eq!(uptime_pct_at(&conn, id, 1, NOW).unwrap(), Some(50.0));
    }

    #[test]
    fn uptime_long_range_uses_rollups_after_retention() {
        let conn = test_conn();
        let id = monitor(&conn);
        for day in 1..=9 {
            beat(&conn, id, "up", None, &format!("2026-02-0{day} 12:00:00"));
        }
        beat(&conn, id, "down", None, "2026-02-10 12:00:00");
        rollup(&conn, NOW).unwrap();
        while delete_heartbeats_before(&conn, "2026-03-01", 100).unwrap() > 0 {}
        assert_eq!(uptime_pct_at(&conn, id, 90, NOW).unwrap(), Some(90.0));
    }

    #[test]
    fn uptime_does_not_double_count_rolled_and_raw_days() {
        let conn = test_conn();
        let id = monitor(&conn);
        beat(&conn, id, "up", None, "2026-03-05 12:00:00");
        beat(&conn, id, "down", None, "2026-03-10 11:00:00");
        rollup(&conn, NOW).unwrap(); // 03-05 now in both tables
        assert_eq!(uptime_pct_at(&conn, id, 30, NOW).unwrap(), Some(50.0));
    }
}
