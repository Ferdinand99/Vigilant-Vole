//! Check scheduler. Owned by the engine agent.
//!
//! Contract:
//! - [`Scheduler::start_all`] at boot,
//! - [`Scheduler::reload`] after ANY monitor create/update/pause/resume/delete,
//! - [`Scheduler::stop`] to stop one monitor's loop,
//! - [`Scheduler::ingest_external`] for results that arrive from outside
//!   (push endpoint, remote agents),
//! - [`Scheduler::check_now`] / [`Scheduler::status_snapshot`] for the UI,
//! - [`Scheduler::shutdown`] on graceful shutdown.
//!
//! Design: every monitor has one runtime entry (its [`state::MonitorState`]
//! behind an async mutex). Local checks, push requests, missed-push timeouts
//! and group recomputes all go through [`Scheduler::process`], which
//! serialises on that mutex and runs the pure state machine in
//! [`alerting::evaluate`] — so every source obeys the same rules.
//!
//! Tasks: active-check monitors get a check loop (jittered start, `interval`
//! while up, `effective_retry_interval()` while failing); push monitors get a
//! watchdog that records `down` when no push arrives within interval + 10%;
//! group monitors get no task — they are recomputed whenever a child records.
//! Probes run under a global semaphore (`VV_MAX_CONCURRENT_CHECKS`, default 64).
//!
//! Remote agents: results with `agent_id = Some` are recorded (upside-down
//! applied) but never drive the monitor's state — the local check is
//! authoritative, agent results only confirm it (`confirm_with_agents`).
//! `ingest_external` expects RAW outcomes; the engine applies upside-down.

pub mod alerting;
pub mod checkers;
pub mod metrics;
pub mod retention;
pub mod state;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use deadpool_sqlite::Pool;
use futures_util::future::join_all;
use rusqlite::{Connection, OptionalExtension, params};
use time::OffsetDateTime;
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, broadcast};
use tokio::task::AbortHandle;
use tracing::Instrument;

use self::alerting::{Action, AlertConfig, Notice, Observation};
use self::state::{MonitorState, fmt_db_time, parse_db_time};
use crate::db::events::{self, MonitorEventKind};
use crate::db::models::{
    CheckOutcome, EventKind, ExpiryInfo, ExpiryKind, HeartbeatStatus, Monitor, MonitorType, NotificationEvent,
};
use crate::db::{
    self, heartbeats as db_heartbeats, monitors as db_monitors, notifications as db_notifications, settings,
};
use crate::{maintenance, notification};

pub use self::metrics::{DurationHistogram, EngineMetrics, MonitorMetrics, NotificationMetrics};
pub use self::state::StatusSnapshot;

/// Default bound on concurrently running probes (`VV_MAX_CONCURRENT_CHECKS` overrides).
const DEFAULT_MAX_CONCURRENT_CHECKS: usize = 64;
/// At boot, first checks are spread over min(interval, this).
const MAX_START_SPREAD_SECS: i64 = 30;
/// After a reload the first check runs within this many milliseconds.
const RELOAD_SPREAD_MS: u64 = 1_500;
/// Hard ceiling on a probe beyond the monitor's own timeout (checkers must
/// respect `timeout_seconds`; this only guards against a misbehaving one).
const CHECK_TIMEOUT_GRACE: Duration = Duration::from_secs(5);
/// Push monitors go down after interval × this without a push.
const PUSH_GRACE_FACTOR: f64 = 1.1;
/// SSE "something changed" signals are coalesced to at most one per this gap.
const UPDATE_MIN_GAP: Duration = Duration::from_millis(250);
/// Nested group depth limit (also breaks parent cycles).
const MAX_GROUP_DEPTH: usize = 8;

/// Engine counters for `/metrics`.
pub fn metrics_snapshot() -> EngineMetrics {
    metrics::snapshot()
}

/// Where an observation came from (for logs and source-specific bookkeeping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Local,
    Push,
    PushMissed,
    Group,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Local => "local",
            Source::Push => "push",
            Source::PushMissed => "push_missed",
            Source::Group => "group",
        }
    }
}

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Pool,
    /// Shared client for notifications (not for checks).
    http: reqwest::Client,
    updates: Arc<UpdateCoalescer>,
    tasks: StdMutex<HashMap<i64, TaskEntry>>,
    runtimes: StdMutex<HashMap<i64, Arc<AsyncMutex<Runtime>>>>,
    /// Serialises reload/start/shutdown so concurrent reloads of one monitor
    /// can't race each other into a double spawn or a stale config.
    reload_lock: AsyncMutex<()>,
    permits: Arc<Semaphore>,
    shut_down: AtomicBool,
}

struct TaskEntry {
    /// `None` for group monitors (no task; tracked for their parent link).
    abort: Option<AbortHandle>,
    /// Wakes the loop early: "check now" for check loops, "push arrived" for watchdogs.
    wake: Arc<Notify>,
    parent_id: Option<i64>,
}

#[derive(Default)]
struct Runtime {
    /// `None` until seeded from the database.
    state: Option<MonitorState>,
}

fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Scheduler {
    pub fn new(pool: Pool, update_tx: broadcast::Sender<()>, http: reqwest::Client) -> Self {
        let max_checks = std::env::var("VV_MAX_CONCURRENT_CHECKS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_CONCURRENT_CHECKS);
        Self {
            inner: Arc::new(Inner {
                pool,
                http,
                updates: Arc::new(UpdateCoalescer::new(update_tx)),
                tasks: StdMutex::new(HashMap::new()),
                runtimes: StdMutex::new(HashMap::new()),
                reload_lock: AsyncMutex::new(()),
                permits: Arc::new(Semaphore::new(max_checks)),
                shut_down: AtomicBool::new(false),
            }),
        }
    }

    /// Load every active monitor and start its loop/watchdog. First checks are
    /// spread over min(interval, 30s) so a large fleet doesn't fire at once.
    pub async fn start_all(&self) {
        let _guard = self.inner.reload_lock.lock().await;
        let monitors = match db::interact(&self.inner.pool, |c| db_monitors::list_active_monitors(c)).await {
            Ok(monitors) => monitors,
            Err(err) => {
                tracing::error!(%err, "failed to load monitors; scheduler not started");
                return;
            }
        };
        let count = monitors.len();
        for monitor in monitors {
            let delay = start_jitter(monitor.id, monitor.interval_seconds);
            self.install(monitor, delay);
        }
        tracing::info!(monitors = count, "scheduler started");
    }

    /// Re-read a monitor from the database and make its task match: deleted or
    /// paused → stopped (and its in-memory state dropped); otherwise
    /// (re)started with the new config, keeping its alerting state. Parent
    /// groups (old and new) are recomputed. Never leaves two tasks running.
    pub async fn reload(&self, monitor_id: i64) {
        let guard = self.inner.reload_lock.lock().await;
        if self.inner.shut_down.load(Ordering::Acquire) {
            return;
        }
        let monitor = match db::interact(&self.inner.pool, move |c| db_monitors::get_monitor(c, monitor_id)).await {
            Ok(m) => m,
            Err(err) => {
                tracing::error!(monitor_id, %err, "failed to reload monitor");
                return;
            }
        };
        let old_parent = self.remove_task(monitor_id).and_then(|t| t.parent_id);
        let mut groups = Vec::new();
        match monitor {
            Some(m) if m.active => {
                if m.monitor_type == MonitorType::Group {
                    groups.push(m.id);
                }
                groups.extend(m.parent_id);
                let delay = Duration::from_millis(spread(m.id) % RELOAD_SPREAD_MS);
                self.install(m, delay);
            }
            other => {
                lock(&self.inner.runtimes).remove(&monitor_id);
                metrics::forget_monitor(monitor_id);
                groups.extend(other.and_then(|m| m.parent_id));
            }
        }
        groups.extend(old_parent);
        groups.sort_unstable();
        groups.dedup();
        drop(guard);
        for group_id in groups {
            // A reloaded group recomputes itself: it is not its own child.
            let from_child = (group_id != monitor_id).then_some(monitor_id);
            self.recompute_group_chain(group_id, from_child).await;
        }
        self.inner.updates.notify();
    }

    /// Stop a monitor's loop/watchdog (no-op if it isn't running). Its
    /// in-memory alerting state is kept; use [`Scheduler::reload`] for
    /// pause/delete semantics.
    pub async fn stop(&self, monitor_id: i64) {
        self.remove_task(monitor_id);
    }

    /// Stop everything (graceful shutdown). Running probes are abandoned;
    /// heartbeat writes are atomic, so nothing is half-recorded. Later
    /// `reload`/`start_all` calls are no-ops.
    pub async fn shutdown(&self) {
        self.inner.shut_down.store(true, Ordering::Release);
        let _guard = self.inner.reload_lock.lock().await;
        let tasks: Vec<TaskEntry> = lock(&self.inner.tasks).drain().map(|(_, t)| t).collect();
        for task in &tasks {
            if let Some(abort) = &task.abort {
                abort.abort();
            }
        }
        self.inner.permits.close();
        metrics::set_running_tasks(0);
        tracing::info!(tasks = tasks.len(), "scheduler stopped");
    }

    /// Record a result produced outside the scheduler. Outcomes must be RAW
    /// (before upside-down).
    /// - `agent_id = None` (push endpoint): goes through the state machine like
    ///   a local check and resets the push watchdog.
    /// - `agent_id = Some` (remote agent): recorded with its agent id; used to
    ///   confirm local failures, never drives the monitor's status.
    ///
    /// Results for unknown, paused or group monitors are ignored.
    pub async fn ingest_external(&self, monitor_id: i64, outcome: CheckOutcome, agent_id: Option<i64>) {
        let monitor = match db::interact(&self.inner.pool, move |c| db_monitors::get_monitor(c, monitor_id)).await {
            Ok(Some(m)) if m.active && m.monitor_type != MonitorType::Group => m,
            Ok(_) => {
                tracing::debug!(
                    monitor_id,
                    ?agent_id,
                    "ignoring external result for missing/paused/group monitor"
                );
                return;
            }
            Err(err) => {
                tracing::warn!(monitor_id, ?agent_id, %err, "failed to load monitor for external result");
                return;
            }
        };
        match agent_id {
            Some(agent_id) => self.record_agent_result(&monitor, agent_id, outcome).await,
            None => {
                let source = if monitor.monitor_type == MonitorType::Push {
                    Source::Push
                } else {
                    Source::Local
                };
                self.record(&monitor, outcome, source).await;
                if source == Source::Push {
                    self.wake(monitor_id);
                }
            }
        }
    }

    /// Run one check right now (the "Check now" button) and return the status
    /// it recorded. `None` for unknown, paused, push and group monitors, or if
    /// the check couldn't be recorded. The regular schedule is unaffected.
    pub async fn check_now(&self, monitor_id: i64) -> Option<HeartbeatStatus> {
        let monitor = match db::interact(&self.inner.pool, move |c| db_monitors::get_monitor(c, monitor_id)).await {
            Ok(Some(m)) if m.active && m.monitor_type.is_active_check() => m,
            Ok(_) => return None,
            Err(err) => {
                tracing::warn!(monitor_id, %err, "check now: failed to load monitor");
                return None;
            }
        };
        self.run_local_check(&Arc::new(monitor)).await
    }

    /// The monitor's current in-memory state, or `None` if it hasn't been
    /// processed since startup/resume (fall back to the latest heartbeat).
    pub async fn status_snapshot(&self, monitor_id: i64) -> Option<StatusSnapshot> {
        let runtime = lock(&self.inner.runtimes).get(&monitor_id).cloned()?;
        let guard = runtime.lock().await;
        guard.state.as_ref().map(MonitorState::snapshot)
    }

    /// Whether a check loop or push watchdog is running for the monitor.
    pub fn is_running(&self, monitor_id: i64) -> bool {
        lock(&self.inner.tasks)
            .get(&monitor_id)
            .is_some_and(|t| t.abort.is_some())
    }

    // -- task management ------------------------------------------------------

    fn install(&self, monitor: Monitor, delay: Duration) {
        let mut tasks = lock(&self.inner.tasks);
        if let Some(old) = tasks.remove(&monitor.id)
            && let Some(abort) = old.abort
        {
            abort.abort();
        }
        if self.inner.shut_down.load(Ordering::Acquire) {
            return;
        }
        let id = monitor.id;
        let parent_id = monitor.parent_id;
        let wake = Arc::new(Notify::new());
        let span = tracing::info_span!("monitor", monitor_id = id, monitor_type = monitor.monitor_type.as_str());
        let abort = match monitor.monitor_type {
            MonitorType::Group => None,
            MonitorType::Push => {
                let fut = self.clone().push_loop(Arc::new(monitor), wake.clone());
                Some(tokio::spawn(fut.instrument(span)).abort_handle())
            }
            _ => {
                let fut = self.clone().check_loop(Arc::new(monitor), wake.clone(), delay);
                Some(tokio::spawn(fut.instrument(span)).abort_handle())
            }
        };
        tasks.insert(id, TaskEntry { abort, wake, parent_id });
        metrics::set_running_tasks(tasks.values().filter(|t| t.abort.is_some()).count());
    }

    fn remove_task(&self, monitor_id: i64) -> Option<TaskEntry> {
        let mut tasks = lock(&self.inner.tasks);
        let removed = tasks.remove(&monitor_id);
        if let Some(abort) = removed.as_ref().and_then(|t| t.abort.as_ref()) {
            abort.abort();
        }
        metrics::set_running_tasks(tasks.values().filter(|t| t.abort.is_some()).count());
        removed
    }

    fn wake(&self, monitor_id: i64) {
        if let Some(task) = lock(&self.inner.tasks).get(&monitor_id) {
            task.wake.notify_one();
        }
    }

    fn runtime(&self, monitor_id: i64) -> Arc<AsyncMutex<Runtime>> {
        lock(&self.inner.runtimes).entry(monitor_id).or_default().clone()
    }

    // -- loops ------------------------------------------------------------------

    async fn check_loop(self, monitor: Arc<Monitor>, wake: Arc<Notify>, initial_delay: Duration) {
        if !initial_delay.is_zero() {
            tokio::select! {
                () = tokio::time::sleep(initial_delay) => {}
                () = wake.notified() => {}
            }
        }
        loop {
            let started = tokio::time::Instant::now();
            let status = self.run_local_check(&monitor).await;
            if self.inner.shut_down.load(Ordering::Acquire) {
                return;
            }
            let secs = match status {
                Some(HeartbeatStatus::Down | HeartbeatStatus::Pending) => monitor.effective_retry_interval(),
                _ => monitor.interval_seconds,
            };
            let next = started + Duration::from_secs(secs.max(1).unsigned_abs());
            tokio::select! {
                () = tokio::time::sleep_until(next) => {}
                () = wake.notified() => {}
            }
        }
    }

    async fn push_loop(self, monitor: Arc<Monitor>, wake: Arc<Notify>) {
        let grace = Duration::from_secs_f64(monitor.interval_seconds.max(1) as f64 * PUSH_GRACE_FACTOR);
        let retry = Duration::from_secs(monitor.effective_retry_interval().max(1).unsigned_abs());
        // Every (re)start gets a full grace window: pushes that were refused
        // while the server was down aren't the client's fault.
        while !self.mark_push_window_start(&monitor).await {
            tokio::time::sleep(retry).await;
        }
        loop {
            let deadline = self.push_deadline_for(monitor.id, grace, retry).await;
            let wait = deadline.map_or(grace, |d| alerting_elapsed(OffsetDateTime::now_utc(), d));
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = wake.notified() => continue,
            }
            // A push may have landed right at the deadline.
            if let Some(d) = self.push_deadline_for(monitor.id, grace, retry).await
                && d > OffsetDateTime::now_utc()
            {
                continue;
            }
            let outcome = CheckOutcome::down(format!("No push received in the last {}s", grace.as_secs()));
            if self.record(&monitor, outcome, Source::PushMissed).await.is_none() {
                // Couldn't record (DB trouble): back off instead of spinning.
                tokio::time::sleep(retry).await;
            }
        }
    }

    /// Seeds the monitor's state if needed and sets `last_push_at = now`.
    async fn mark_push_window_start(&self, monitor: &Monitor) -> bool {
        let runtime = self.runtime(monitor.id);
        let mut guard = runtime.lock().await;
        let now = OffsetDateTime::now_utc();
        if guard.state.is_none() {
            let m = monitor.clone();
            match db::interact(&self.inner.pool, move |c| state::seed(c, &m, now)).await {
                Ok(s) => guard.state = Some(s),
                Err(err) => {
                    tracing::warn!(monitor_id = monitor.id, %err, "failed to seed push monitor state");
                    return false;
                }
            }
        }
        if let Some(s) = guard.state.as_mut() {
            s.last_push_at = Some(now);
        }
        true
    }

    async fn push_deadline_for(&self, monitor_id: i64, grace: Duration, retry: Duration) -> Option<OffsetDateTime> {
        let runtime = self.runtime(monitor_id);
        let guard = runtime.lock().await;
        let s = guard.state.as_ref()?;
        Some(push_deadline(s.last_push_at?, s.last_miss_at, grace, retry))
    }

    // -- recording ----------------------------------------------------------------

    /// One local probe under the concurrency limit, then the state machine.
    async fn run_local_check(&self, monitor: &Arc<Monitor>) -> Option<HeartbeatStatus> {
        let permit = self.inner.permits.clone().acquire_owned().await.ok()?;
        let span = tracing::info_span!(
            "check",
            monitor_id = monitor.id,
            monitor_type = monitor.monitor_type.as_str(),
            duration_ms = tracing::field::Empty,
            response_ms = tracing::field::Empty,
            status = tracing::field::Empty,
        );
        let started = std::time::Instant::now();
        let outcome = probe(monitor.clone()).instrument(span.clone()).await;
        drop(permit);
        let duration = started.elapsed();
        metrics::record_duration(monitor.monitor_type.as_str(), duration);
        span.record("duration_ms", u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
        if let Some(ms) = outcome.response_time_ms {
            span.record("response_ms", ms);
        }
        let status = self
            .record(monitor, outcome, Source::Local)
            .instrument(span.clone())
            .await;
        if let Some(status) = status {
            span.record("status", status.as_str());
        }
        tracing::debug!(parent: &span, "check finished");
        status
    }

    /// State machine + persistence + group propagation + SSE signal.
    async fn record(&self, monitor: &Monitor, outcome: CheckOutcome, source: Source) -> Option<HeartbeatStatus> {
        let status = self.process(monitor, outcome, source).await;
        if status.is_some()
            && let Some(parent) = monitor.parent_id
        {
            self.recompute_group_chain(parent, Some(monitor.id)).await;
        }
        self.inner.updates.notify();
        status
    }

    /// Runs one observation through the state machine, persists the heartbeat
    /// and events atomically, and dispatches notifications. Returns the
    /// recorded status, or `None` if the database couldn't be read/written.
    async fn process(&self, monitor: &Monitor, outcome: CheckOutcome, source: Source) -> Option<HeartbeatStatus> {
        let monitor_id = monitor.id;
        let runtime = self.runtime(monitor_id);
        let mut guard = runtime.lock().await;
        let now = OffsetDateTime::now_utc();
        let cfg = AlertConfig::for_monitor(monitor);
        let need_seed = guard.state.is_none();
        let want_votes = cfg.confirm_with_agents && source == Source::Local;
        let interval = monitor.interval_seconds;

        let m = monitor.clone();
        let pre = db::interact(&self.inner.pool, move |c| {
            let seeded = if need_seed {
                Some(state::seed(c, &m, now)?)
            } else {
                None
            };
            let in_maintenance = maintenance::is_under_maintenance(c, m.id).unwrap_or_else(|err| {
                tracing::warn!(monitor_id = m.id, %err, "maintenance lookup failed; assuming none");
                false
            });
            let votes = if want_votes {
                Some(state::agent_votes(c, m.id, interval.max(1) * 2, now)?)
            } else {
                None
            };
            Ok((seeded, in_maintenance, votes))
        })
        .await;
        let (seeded, in_maintenance, votes) = match pre {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(monitor_id, %err, "failed to load monitor state; result dropped");
                return None;
            }
        };

        let prev = guard.state.get_or_insert_with(|| seeded.unwrap_or_default());
        match source {
            Source::Push => prev.last_push_at = Some(now),
            Source::PushMissed => prev.last_miss_at = Some(now),
            Source::Local | Source::Group => {}
        }
        let previous_status = prev.last_status;
        let obs = Observation {
            status: Some(outcome.status),
            message: outcome.message.clone(),
            maintenance: in_maintenance,
            agents: votes,
        };
        let decision = alerting::evaluate(prev, &obs, &cfg, now);

        let write = Persist {
            monitor_id,
            status: decision.status,
            message: decision.message.clone(),
            response_time_ms: outcome.response_time_ms,
            actions: decision
                .actions
                .iter()
                .filter(|a| !matches!(a, Action::Notify(_)))
                .cloned()
                .collect(),
            expiry: outcome.expiry.clone(),
            expiry_notify: monitor.expiry_notify && !in_maintenance,
            expiry_days: monitor.expiry_days,
            now,
        };
        let persisted = db::interact(&self.inner.pool, move |c| persist(c, &write)).await;
        let expiry_notices = match persisted {
            Ok(n) => n,
            Err(err) => {
                // Keep going: alerting state and notifications still matter
                // even if this heartbeat couldn't be stored.
                tracing::warn!(monitor_id, %err, "failed to record heartbeat");
                Vec::new()
            }
        };

        if previous_status != Some(decision.status) {
            tracing::info!(
                monitor_id,
                monitor = %monitor.name,
                from = previous_status.map_or("none", HeartbeatStatus::as_str),
                to = decision.status.as_str(),
                source = source.as_str(),
                message = decision.message.as_deref().unwrap_or(""),
                "monitor status changed"
            );
        }
        let status = decision.status;
        let mut notices: Vec<Notice> = decision.notices().cloned().collect();
        guard.state = Some(decision.state);
        drop(guard);

        metrics::record_result(monitor_id, status, outcome.response_time_ms);
        notices.extend(expiry_notices);
        if !notices.is_empty() {
            self.dispatch(monitor, notices, now);
        }
        Some(status)
    }

    /// Remote-agent result: stored with its agent id, no state change.
    async fn record_agent_result(&self, monitor: &Monitor, agent_id: i64, outcome: CheckOutcome) {
        let status = match (outcome.status, monitor.upside_down) {
            (HeartbeatStatus::Up, true) => HeartbeatStatus::Down,
            (HeartbeatStatus::Down, true) => HeartbeatStatus::Up,
            (s, _) => s,
        };
        let monitor_id = monitor.id;
        let result = db::interact(&self.inner.pool, move |c| {
            db_heartbeats::insert(
                c,
                monitor_id,
                Some(agent_id),
                status,
                outcome.response_time_ms,
                outcome.message.as_deref(),
            )
        })
        .await;
        match result {
            Ok(_) => self.inner.updates.notify(),
            Err(err) => tracing::warn!(monitor_id, agent_id, %err, "failed to record agent heartbeat"),
        }
    }

    /// Recompute `group_id` and, while it records a new status, its ancestors.
    /// `from_child` is the monitor whose change triggered this (`None` when
    /// the group itself was reloaded).
    async fn recompute_group_chain(&self, group_id: i64, from_child: Option<i64>) {
        let mut visited: Vec<i64> = from_child.into_iter().collect();
        let mut next = Some(group_id);
        for _ in 0..MAX_GROUP_DEPTH {
            let Some(id) = next else { return };
            if visited.contains(&id) {
                tracing::warn!(group_id = id, "monitor group cycle detected; stopping propagation");
                return;
            }
            visited.push(id);
            next = self.recompute_group(id).await;
        }
    }

    /// Aggregates a group's children and records the result when it changed
    /// (or at most once per the group's interval otherwise). Returns the
    /// group's parent when something was recorded.
    async fn recompute_group(&self, group_id: i64) -> Option<i64> {
        let loaded = db::interact(&self.inner.pool, move |c| {
            match db_monitors::get_monitor(c, group_id)? {
                Some(g) if g.active && g.monitor_type == MonitorType::Group => {
                    let children = state::group_children(c, group_id)?;
                    Ok(Some((g, children)))
                }
                _ => Ok(None),
            }
        })
        .await;
        let (group, children) = match loaded {
            Ok(v) => v?,
            Err(err) => {
                tracing::warn!(group_id, %err, "failed to load group children");
                return None;
            }
        };
        let (status, message) = alerting::aggregate_group(&children)?;

        let existing = lock(&self.inner.runtimes).get(&group_id).cloned();
        if let Some(runtime) = existing {
            let guard = runtime.lock().await;
            if let Some(s) = &guard.state {
                let recent = s.last_recorded_at.is_some_and(|at| {
                    alerting_elapsed(at, OffsetDateTime::now_utc())
                        < Duration::from_secs(group.interval_seconds.max(1).unsigned_abs())
                });
                let same =
                    matches!(s.last_status, Some(last) if last == status || last == HeartbeatStatus::Maintenance);
                if recent && same {
                    return None;
                }
            }
        }
        let outcome = CheckOutcome {
            status,
            response_time_ms: None,
            message: Some(message),
            expiry: None,
        };
        self.process(&group, outcome, Source::Group).await.and(group.parent_id)
    }

    // -- notifications ------------------------------------------------------------

    fn dispatch(&self, monitor: &Monitor, notices: Vec<Notice>, occurred_at: OffsetDateTime) {
        let pool = self.inner.pool.clone();
        let http = self.inner.http.clone();
        let monitor_id = monitor.id;
        let monitor_name = monitor.name.clone();
        let monitor_type = monitor.monitor_type.as_str().to_string();
        let target = monitor.target.clone();
        let task = async move {
            let loaded = db::interact(&pool, move |c| {
                Ok((
                    db_notifications::list_channels_for_monitor(c, monitor_id, false)?,
                    db_notifications::list_channels_for_monitor(c, monitor_id, true)?,
                    settings::load(c)?,
                ))
            })
            .await;
            let (regular, escalation, settings) = match loaded {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(monitor_id, %err, "failed to load notification channels");
                    return;
                }
            };
            // Sequential per notice so channels see Down before Escalation, etc.
            for notice in notices {
                let channels = if notice.escalation { &escalation } else { &regular };
                if channels.is_empty() {
                    tracing::debug!(
                        monitor_id,
                        event = notice.kind.as_str(),
                        "no channels linked; notification skipped"
                    );
                    continue;
                }
                let event = NotificationEvent {
                    kind: notice.kind,
                    monitor_id: Some(monitor_id),
                    monitor_name: monitor_name.clone(),
                    monitor_type: monitor_type.clone(),
                    target: target.clone(),
                    message: notice.message,
                    downtime: notice.downtime,
                    expires_in_days: notice.expires_in_days,
                    url: settings.monitor_url(monitor_id),
                    occurred_at,
                };
                let results = join_all(channels.iter().map(|c| notification::send(&http, c, &event))).await;
                for (channel, result) in channels.iter().zip(results) {
                    metrics::record_notification(&channel.kind, result.is_ok());
                    match result {
                        Ok(()) => tracing::info!(
                            monitor_id,
                            channel_id = channel.id,
                            channel_kind = %channel.kind,
                            event = notice.kind.as_str(),
                            escalation = notice.escalation,
                            "notification sent"
                        ),
                        Err(err) => tracing::warn!(
                            monitor_id,
                            channel_id = channel.id,
                            channel_kind = %channel.kind,
                            event = notice.kind.as_str(),
                            %err,
                            "notification failed"
                        ),
                    }
                }
            }
        };
        tokio::spawn(task.instrument(tracing::info_span!("notify", monitor_id)));
    }
}

/// Run the checker on its own task (a panic becomes a `down` result instead of
/// killing the loop) with a hard ceiling above the monitor's timeout.
async fn probe(monitor: Arc<Monitor>) -> CheckOutcome {
    let limit = checkers::timeout_of(&monitor) + CHECK_TIMEOUT_GRACE;
    let m = monitor.clone();
    let mut handle = tokio::spawn(async move { checkers::run_check(&m).await });
    match tokio::time::timeout(limit, &mut handle).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) => {
            tracing::error!(monitor_id = monitor.id, %err, "checker task failed");
            CheckOutcome::down("Check failed unexpectedly")
        }
        Err(_) => {
            handle.abort();
            tracing::warn!(
                monitor_id = monitor.id,
                limit_secs = limit.as_secs(),
                "checker exceeded its timeout"
            );
            CheckOutcome::down(format!("Check timed out after {}s", limit.as_secs()))
        }
    }
}

fn alerting_elapsed(from: OffsetDateTime, to: OffsetDateTime) -> Duration {
    (to - from).max(time::Duration::ZERO).unsigned_abs()
}

/// When the push watchdog should next record a miss: `grace` after the last
/// push, and after a miss, `retry` after that miss.
fn push_deadline(
    last_push: OffsetDateTime,
    last_miss: Option<OffsetDateTime>,
    grace: Duration,
    retry: Duration,
) -> OffsetDateTime {
    let base = last_push + grace;
    match last_miss {
        Some(miss) if miss >= last_push => base.max(miss + retry),
        _ => base,
    }
}

/// Deterministic per-monitor spread value (Fibonacci hashing of the id).
fn spread(id: i64) -> u64 {
    (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 24
}

/// Boot-time delay before a monitor's first check, within min(interval, 30s).
fn start_jitter(id: i64, interval_seconds: i64) -> Duration {
    let window_ms = interval_seconds.clamp(1, MAX_START_SPREAD_SECS).unsigned_abs() * 1000;
    Duration::from_millis(spread(id) % window_ms)
}

// ---------------------------------------------------------------------------
// Persistence (one transaction per observation)
// ---------------------------------------------------------------------------

struct Persist {
    monitor_id: i64,
    status: HeartbeatStatus,
    message: Option<String>,
    response_time_ms: Option<i64>,
    actions: Vec<Action>,
    expiry: Option<ExpiryInfo>,
    expiry_notify: bool,
    expiry_days: i64,
    now: OffsetDateTime,
}

fn persist(conn: &mut Connection, p: &Persist) -> rusqlite::Result<Vec<Notice>> {
    let tx = conn.transaction()?;
    db_heartbeats::insert(
        &tx,
        p.monitor_id,
        None,
        p.status,
        p.response_time_ms,
        p.message.as_deref(),
    )?;
    for action in &p.actions {
        match action {
            Action::OpenEvent {
                kind,
                message,
                started_at,
            } => {
                events::open(
                    &tx,
                    p.monitor_id,
                    *kind,
                    message.as_deref(),
                    Some(&fmt_db_time(*started_at)),
                )?;
            }
            Action::CloseEvent { kind, ended_at } => {
                events::close_open(&tx, p.monitor_id, *kind, Some(&fmt_db_time(*ended_at)))?;
            }
            Action::Notify(_) => {}
        }
    }
    let notices = match &p.expiry {
        Some(expiry) => record_expiry(&tx, p, expiry)?,
        None => Vec::new(),
    };
    tx.commit()?;
    Ok(notices)
}

/// Upserts `monitor_expiry` (resetting `notified_at` when the expiry date
/// changed, i.e. renewed) and returns a warning notice when one is due.
fn record_expiry(conn: &Connection, p: &Persist, expiry: &ExpiryInfo) -> rusqlite::Result<Vec<Notice>> {
    let kind = expiry.kind.as_str();
    let existing: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT expires_at, notified_at FROM monitor_expiry WHERE monitor_id = ?1 AND kind = ?2",
            params![p.monitor_id, kind],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let notified_at = match existing {
        Some((expires_at, notified_at)) if expires_at == expiry.expires_at => notified_at,
        _ => None,
    };
    let now_text = fmt_db_time(p.now);
    conn.execute(
        "INSERT INTO monitor_expiry (monitor_id, kind, subject, issuer, expires_at, checked_at, notified_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (monitor_id, kind) DO UPDATE SET
             subject = excluded.subject, issuer = excluded.issuer, expires_at = excluded.expires_at,
             checked_at = excluded.checked_at, notified_at = excluded.notified_at",
        params![
            p.monitor_id,
            kind,
            expiry.subject,
            expiry.issuer,
            expiry.expires_at,
            now_text,
            notified_at
        ],
    )?;
    if !p.expiry_notify {
        return Ok(Vec::new());
    }
    let Some(expires_at) = parse_db_time(&expiry.expires_at) else {
        tracing::warn!(monitor_id = p.monitor_id, expires_at = %expiry.expires_at, "unparseable expiry date");
        return Ok(Vec::new());
    };
    let previous = notified_at.as_deref().and_then(parse_db_time);
    let Some(left) = alerting::expiry_due(expires_at, previous, p.expiry_days, p.now) else {
        return Ok(Vec::new());
    };
    conn.execute(
        "UPDATE monitor_expiry SET notified_at = ?3 WHERE monitor_id = ?1 AND kind = ?2",
        params![p.monitor_id, kind, now_text],
    )?;
    let (event_kind, notice_kind, what) = match expiry.kind {
        ExpiryKind::Cert => (MonitorEventKind::CertExpiring, EventKind::CertExpiring, "Certificate"),
        ExpiryKind::Domain => (MonitorEventKind::DomainExpiring, EventKind::DomainExpiring, "Domain"),
    };
    let date = expiry.expires_at.get(..10).unwrap_or(&expiry.expires_at);
    let subject = expiry.subject.as_deref().map(|s| format!(" {s}")).unwrap_or_default();
    let event_text = match left {
        l if l < 0 => format!("{what}{subject} expired on {date}"),
        0 => format!("{what}{subject} expires today ({date})"),
        1 => format!("{what}{subject} expires in 1 day ({date})"),
        l => format!("{what}{subject} expires in {l} days ({date})"),
    };
    events::log_point(conn, p.monitor_id, event_kind, Some(&event_text), Some(&now_text))?;
    let mut detail = format!("Expires {date} UTC");
    if let Some(s) = &expiry.subject {
        detail.push_str(&format!(" · {s}"));
    }
    if let Some(i) = &expiry.issuer {
        detail.push_str(&format!(" · issued by {i}"));
    }
    Ok(vec![Notice {
        kind: notice_kind,
        escalation: false,
        message: Some(detail),
        downtime: None,
        expires_in_days: Some(left),
    }])
}

// ---------------------------------------------------------------------------
// SSE coalescing
// ---------------------------------------------------------------------------

/// Leading + trailing edge throttle for the dashboard "something changed"
/// broadcast: the first signal goes out immediately, bursts collapse into one
/// trailing signal, at most one per [`UPDATE_MIN_GAP`].
struct UpdateCoalescer {
    tx: broadcast::Sender<()>,
    state: StdMutex<CoalesceState>,
}

#[derive(Default)]
struct CoalesceState {
    last_sent: Option<tokio::time::Instant>,
    trailing_scheduled: bool,
}

impl UpdateCoalescer {
    fn new(tx: broadcast::Sender<()>) -> Self {
        Self {
            tx,
            state: StdMutex::new(CoalesceState::default()),
        }
    }

    fn notify(self: &Arc<Self>) {
        let now = tokio::time::Instant::now();
        let mut st = lock(&self.state);
        match st.last_sent {
            Some(last) if now.duration_since(last) < UPDATE_MIN_GAP => {
                if st.trailing_scheduled {
                    return;
                }
                let Ok(handle) = tokio::runtime::Handle::try_current() else {
                    // No runtime (shouldn't happen in the server): just send.
                    let _ = self.tx.send(());
                    return;
                };
                st.trailing_scheduled = true;
                let this = Arc::clone(self);
                handle.spawn(async move {
                    tokio::time::sleep_until(last + UPDATE_MIN_GAP).await;
                    let mut st = lock(&this.state);
                    st.trailing_scheduled = false;
                    st.last_sent = Some(tokio::time::Instant::now());
                    drop(st);
                    // No subscribers is fine.
                    let _ = this.tx.send(());
                });
            }
            _ => {
                st.last_sent = Some(now);
                drop(st);
                let _ = self.tx.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests;
