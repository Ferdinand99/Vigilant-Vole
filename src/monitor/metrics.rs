//! In-process engine counters, exposed via [`super::metrics_snapshot`] for the
//! Prometheus `/metrics` endpoint (integrations agent). Owned by the engine agent.
//!
//! Questions these answer for an operator:
//! - Are checks running, and how many fail? (`checks_total` / `checks_failed`,
//!   per monitor and overall)
//! - Are probes getting slow? (`check_duration` histogram per monitor type,
//!   `last_latency_ms` per monitor)
//! - Are alerts actually delivered? (`notifications` sent/failed per channel kind)
//!
//! Label sets are bounded: monitor ids, 13 monitor types, and channel kinds.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;

use crate::db::models::HeartbeatStatus;

/// Histogram bucket upper bounds for check durations, in seconds.
pub const DURATION_BUCKETS: [f64; 10] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0];

/// Point-in-time copy of the engine's counters.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct EngineMetrics {
    /// Heartbeats recorded through the state machine (local checks, pushes,
    /// missed pushes, group recomputes).
    pub checks_total: u64,
    /// Of those, how many recorded `down` or `pending`.
    pub checks_failed: u64,
    /// Ordered by monitor id.
    pub monitors: Vec<MonitorMetrics>,
    /// Ordered by monitor type.
    pub check_duration: Vec<DurationHistogram>,
    /// Ordered by channel kind.
    pub notifications: Vec<NotificationMetrics>,
    /// Monitors with a running check loop / push watchdog.
    pub running_tasks: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonitorMetrics {
    pub monitor_id: i64,
    pub checks_total: u64,
    pub checks_failed: u64,
    pub last_latency_ms: Option<i64>,
    pub last_status: Option<HeartbeatStatus>,
}

/// Cumulative histogram of local probe durations for one monitor type.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DurationHistogram {
    pub monitor_type: String,
    /// `(upper bound in seconds, cumulative count)`, matching [`DURATION_BUCKETS`].
    pub buckets: Vec<(f64, u64)>,
    pub sum_seconds: f64,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotificationMetrics {
    pub kind: String,
    pub sent: u64,
    pub failed: u64,
}

#[derive(Default)]
struct Registry {
    checks_total: u64,
    checks_failed: u64,
    monitors: BTreeMap<i64, MonitorMetrics>,
    durations: BTreeMap<String, (Vec<u64>, f64, u64)>,
    notifications: BTreeMap<String, (u64, u64)>,
    running_tasks: usize,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

fn with_registry<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut guard)
}

/// One recorded heartbeat for a monitor.
pub(crate) fn record_result(monitor_id: i64, status: HeartbeatStatus, latency_ms: Option<i64>) {
    let failed = matches!(status, HeartbeatStatus::Down | HeartbeatStatus::Pending);
    with_registry(|r| {
        r.checks_total += 1;
        r.checks_failed += u64::from(failed);
        let m = r.monitors.entry(monitor_id).or_insert_with(|| MonitorMetrics {
            monitor_id,
            checks_total: 0,
            checks_failed: 0,
            last_latency_ms: None,
            last_status: None,
        });
        m.checks_total += 1;
        m.checks_failed += u64::from(failed);
        m.last_latency_ms = latency_ms;
        m.last_status = Some(status);
    });
}

/// Wall-clock duration of one local probe.
pub(crate) fn record_duration(monitor_type: &str, duration: Duration) {
    let secs = duration.as_secs_f64();
    with_registry(|r| {
        let (buckets, sum, count) = r
            .durations
            .entry(monitor_type.to_string())
            .or_insert_with(|| (vec![0; DURATION_BUCKETS.len()], 0.0, 0));
        for (slot, bound) in buckets.iter_mut().zip(DURATION_BUCKETS) {
            if secs <= bound {
                *slot += 1;
            }
        }
        *sum += secs;
        *count += 1;
    });
}

/// One delivery attempt to a channel of `kind`.
pub(crate) fn record_notification(kind: &str, ok: bool) {
    with_registry(|r| {
        let entry = r.notifications.entry(kind.to_string()).or_default();
        if ok {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    });
}

/// Drop per-monitor series for a deleted/paused monitor.
pub(crate) fn forget_monitor(monitor_id: i64) {
    with_registry(|r| {
        r.monitors.remove(&monitor_id);
    });
}

/// Number of running check loops / push watchdogs.
pub(crate) fn set_running_tasks(n: usize) {
    with_registry(|r| r.running_tasks = n);
}

pub(crate) fn snapshot() -> EngineMetrics {
    with_registry(|r| EngineMetrics {
        checks_total: r.checks_total,
        checks_failed: r.checks_failed,
        monitors: r.monitors.values().cloned().collect(),
        check_duration: r
            .durations
            .iter()
            .map(|(t, (buckets, sum, count))| DurationHistogram {
                monitor_type: t.clone(),
                buckets: DURATION_BUCKETS.iter().copied().zip(buckets.iter().copied()).collect(),
                sum_seconds: *sum,
                count: *count,
            })
            .collect(),
        notifications: r
            .notifications
            .iter()
            .map(|(kind, (sent, failed))| NotificationMetrics {
                kind: kind.clone(),
                sent: *sent,
                failed: *failed,
            })
            .collect(),
        running_tasks: r.running_tasks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is process-global and tests run in parallel, so each test
    // uses its own monitor ids / type names / kinds and only checks those.

    #[test]
    fn results_are_counted_per_monitor() {
        record_result(9_001, HeartbeatStatus::Up, Some(12));
        record_result(9_001, HeartbeatStatus::Down, None);
        let snap = snapshot();
        let m = snap.monitors.iter().find(|m| m.monitor_id == 9_001).unwrap();
        assert_eq!((m.checks_total, m.checks_failed), (2, 1));
        assert_eq!(m.last_status, Some(HeartbeatStatus::Down));
        assert!(snap.checks_total >= 2);
    }

    #[test]
    fn durations_fill_cumulative_buckets() {
        record_duration("test-type-a", Duration::from_millis(200));
        let snap = snapshot();
        let h = snap
            .check_duration
            .iter()
            .find(|h| h.monitor_type == "test-type-a")
            .unwrap();
        let counts: Vec<u64> = h.buckets.iter().map(|b| b.1).collect();
        assert_eq!(counts, [0, 0, 1, 1, 1, 1, 1, 1, 1, 1]);
        assert_eq!(h.count, 1);
    }

    #[test]
    fn notifications_split_sent_and_failed() {
        record_notification("test-kind", true);
        record_notification("test-kind", false);
        record_notification("test-kind", true);
        let snap = snapshot();
        let n = snap.notifications.iter().find(|n| n.kind == "test-kind").unwrap();
        assert_eq!((n.sent, n.failed), (2, 1));
    }

    #[test]
    fn forgotten_monitors_disappear() {
        record_result(9_002, HeartbeatStatus::Up, None);
        forget_monitor(9_002);
        assert!(!snapshot().monitors.iter().any(|m| m.monitor_id == 9_002));
    }
}
