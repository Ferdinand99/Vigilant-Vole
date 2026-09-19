pub mod checkers;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use deadpool_sqlite::Pool;
use tokio::sync::{Mutex, broadcast};
use tokio::task::AbortHandle;

use crate::db::models::{HeartbeatStatus, Monitor, MonitorType};
use crate::db::monitors as db_monitors;
use crate::db::notifications as db_notifications;
use crate::notification;

#[derive(Clone)]
pub struct Scheduler {
    pool: Pool,
    http_client: reqwest::Client,
    tasks: Arc<Mutex<HashMap<i64, AbortHandle>>>,
    update_tx: broadcast::Sender<()>,
}

impl Scheduler {
    pub fn new(pool: Pool, update_tx: broadcast::Sender<()>) -> Self {
        Self {
            pool,
            http_client: reqwest::Client::new(),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            update_tx,
        }
    }

    /// Load every active monitor from the database and start a check loop for each.
    pub async fn start_all(&self) {
        let conn = self.pool.get().await.expect("failed to get db connection");
        let monitors = conn
            .interact(|conn| db_monitors::list_active_monitors(conn))
            .await
            .expect("db task panicked")
            .expect("failed to list active monitors");
        for monitor in monitors {
            self.spawn(monitor).await;
        }
    }

    pub async fn spawn(&self, monitor: Monitor) {
        let id = monitor.id;
        let pool = self.pool.clone();
        let http_client = self.http_client.clone();
        let update_tx = self.update_tx.clone();
        let handle = tokio::spawn(run_check_loop(pool, http_client, monitor, update_tx));
        self.tasks.lock().await.insert(id, handle.abort_handle());
    }

    pub async fn respawn(&self, monitor: Monitor) {
        self.stop(monitor.id).await;
        self.spawn(monitor).await;
    }

    pub async fn stop(&self, id: i64) {
        if let Some(handle) = self.tasks.lock().await.remove(&id) {
            handle.abort();
        }
    }
}

async fn run_check_loop(pool: Pool, http_client: reqwest::Client, monitor: Monitor, update_tx: broadcast::Sender<()>) {
    let mut consecutive_failures: i64 = 0;
    let mut last_notified: Option<HeartbeatStatus> = None;
    let mut interval = tokio::time::interval(Duration::from_secs(monitor.interval_seconds.max(1) as u64));

    loop {
        interval.tick().await;
        let timeout = Duration::from_secs(monitor.timeout_seconds.max(1) as u64);

        let outcome = match monitor.monitor_type {
            MonitorType::Http => checkers::check_http(&http_client, &monitor.target, timeout).await,
            MonitorType::Tcp => checkers::check_tcp(&monitor.target, timeout).await,
            MonitorType::Ping => checkers::check_ping(&monitor.target, timeout).await,
        };

        let status = if outcome.status == HeartbeatStatus::Up {
            consecutive_failures = 0;
            HeartbeatStatus::Up
        } else {
            consecutive_failures += 1;
            if consecutive_failures <= monitor.retries {
                HeartbeatStatus::Pending
            } else {
                HeartbeatStatus::Down
            }
        };

        let Ok(conn) = pool.get().await else {
            tracing::warn!(monitor_id = monitor.id, "skipped heartbeat: db pool unavailable");
            continue;
        };

        let monitor_id = monitor.id;
        let response_time_ms = outcome.response_time_ms;
        let message = outcome.message.clone();
        if let Err(err) = conn
            .interact(move |conn| {
                db_monitors::insert_heartbeat(conn, monitor_id, status, response_time_ms, message.as_deref())
            })
            .await
        {
            tracing::warn!(monitor_id, %err, "failed to record heartbeat");
        }

        // Wake up any connected dashboards so they refresh without a manual reload.
        let _ = update_tx.send(());

        // Only alert on settled up/down transitions - a "pending" (within the retry
        // grace period) state never triggers a notification.
        if status != HeartbeatStatus::Pending && Some(status) != last_notified {
            last_notified = Some(status);
            let channels = conn
                .interact(|conn| db_notifications::list_active_channels(conn))
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            if !channels.is_empty() {
                notification::notify_all(&http_client, &channels, &monitor.name, status, outcome.message.as_deref())
                    .await;
            }
        }
    }
}
