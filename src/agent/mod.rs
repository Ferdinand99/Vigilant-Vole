//! Remote probe agent mode (`vigilant-vole agent --server <url> --token <tok>`).
//! Owned by the integrations agent.
//!
//! The agent polls `GET /api/agent/v1/monitors` every 30 seconds (with
//! `If-None-Match`), runs each `run_on_agents` monitor on its own interval
//! with the same [`checkers::run_check`] the server uses (bounded
//! concurrency, jittered start), buffers RAW results and posts them to
//! `POST /api/agent/v1/results` every 5 seconds. When the server is
//! unreachable it retries with exponential backoff and keeps up to
//! [`MAX_BUFFER`] results (oldest dropped first). Ctrl-C / SIGTERM stops the
//! checks and flushes what's buffered.
//!
//! `--once` fetches the monitors, runs every check one time, posts the
//! results, prints a summary and exits (non-zero if the server rejected any).

pub mod protocol;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rand::RngExt;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, ETAG, HeaderMap, HeaderValue, IF_NONE_MATCH};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

use self::protocol::{
    MAX_BATCH, MONITORS_PATH, MonitorsResponse, NAME_HEADER, POLL_SECONDS, RESULTS_PATH, ResultItem,
    ResultsResponse, VERSION_HEADER, now_db,
};
use crate::db::models::{HeartbeatStatus, Monitor};
use crate::monitor::checkers;

/// Agent version reported to the server.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Most results kept while the server is unreachable.
pub const MAX_BUFFER: usize = 5_000;
/// How often buffered results are posted.
const FLUSH_EVERY: Duration = Duration::from_secs(5);
/// Longest wait between retries when the server is unreachable.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const DEFAULT_CONCURRENCY: usize = 16;

/// Command-line arguments for agent mode.
#[derive(Debug, Clone, clap::Args)]
pub struct AgentArgs {
    /// Base URL of the Vigilant Vole server, e.g. https://status.example.com
    #[arg(long, env = "VV_AGENT_SERVER")]
    pub server: String,
    /// Agent token (shown once when the agent is created in Settings → Remote agents)
    #[arg(long, env = "VV_AGENT_TOKEN", hide_env_values = true)]
    pub token: String,
    /// Display name reported to the server (defaults to the hostname)
    #[arg(long, env = "VV_AGENT_NAME")]
    pub name: Option<String>,
    /// Run every monitor once, post the results, print a summary and exit
    #[arg(long)]
    pub once: bool,
    /// Checks allowed to run at the same time
    #[arg(long, env = "VV_AGENT_CONCURRENCY", default_value_t = DEFAULT_CONCURRENCY)]
    pub concurrency: usize,
}

pub type AgentError = Box<dyn std::error::Error + Send + Sync>;

/// Run the agent until it is stopped.
pub async fn run(args: AgentArgs) -> Result<(), AgentError> {
    let client = Client::new(&args)?;
    if args.once {
        let summary = run_once(&client, args.concurrency).await?;
        println!("{summary}");
        // A down target is a valid result; only a refused delivery is an error.
        return if summary.rejected > 0 {
            Err(format!("the server rejected {} results", summary.rejected).into())
        } else {
            Ok(())
        };
    }
    let (stop_tx, stop_rx) = watch::channel(false);
    let agent = tokio::spawn(run_loop(client, args.concurrency.max(1), stop_rx));
    shutdown_signal().await;
    tracing::info!("agent stopping");
    let _ = stop_tx.send(true);
    match agent.await {
        Ok(result) => result,
        Err(err) => Err(format!("agent task failed: {err}").into()),
    }
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

/// Talks to the server's agent API.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
}

/// Outcome of a monitor list fetch.
#[derive(Debug)]
pub enum Fetch {
    Changed(MonitorsResponse, Option<String>),
    NotModified,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the server rejected the agent token (401); check --token")]
    Unauthorized,
    #[error("this agent is disabled on the server (403)")]
    Disabled,
    #[error("server answered {0}: {1}")]
    Status(StatusCode, String),
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
}

impl ClientError {
    /// Errors worth retrying soon (network, 5xx, 429).
    pub fn is_transient(&self) -> bool {
        match self {
            ClientError::Http(_) => true,
            ClientError::Status(code, _) => code.is_server_error() || *code == StatusCode::TOO_MANY_REQUESTS,
            ClientError::Unauthorized | ClientError::Disabled => false,
        }
    }
}

fn default_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "agent".into())
}

impl Client {
    pub fn new(args: &AgentArgs) -> Result<Self, AgentError> {
        let base = args.server.trim().trim_end_matches('/').to_string();
        let parsed = url::Url::parse(&base).map_err(|e| format!("--server must be a URL like https://status.example.com: {e}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("--server must start with http:// or https://".into());
        }
        let token = args.token.trim();
        if !token.starts_with(crate::db::agents::TOKEN_PREFIX) {
            return Err("--token must be an agent token (it starts with vva_)".into());
        }
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| "invalid token")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(VERSION_HEADER, HeaderValue::from_static(VERSION));
        let name = args.name.clone().unwrap_or_else(default_name);
        if let Ok(v) = HeaderValue::from_str(name.trim()) {
            headers.insert(NAME_HEADER, v);
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("VigilantVoleAgent/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, base })
    }

    async fn error_for(resp: reqwest::Response) -> ClientError {
        match resp.status() {
            StatusCode::UNAUTHORIZED => ClientError::Unauthorized,
            StatusCode::FORBIDDEN => ClientError::Disabled,
            status => {
                let body = resp.text().await.unwrap_or_default();
                ClientError::Status(status, body.chars().take(200).collect())
            }
        }
    }

    /// Fetch the monitor list (`etag` = the last one seen).
    pub async fn fetch_monitors(&self, etag: Option<&str>) -> Result<Fetch, ClientError> {
        let mut req = self.http.get(format!("{}{MONITORS_PATH}", self.base));
        if let Some(tag) = etag {
            req = req.header(IF_NONE_MATCH, tag);
        }
        let resp = req.send().await?;
        if resp.status() == StatusCode::NOT_MODIFIED {
            return Ok(Fetch::NotModified);
        }
        if !resp.status().is_success() {
            return Err(Self::error_for(resp).await);
        }
        let tag = resp.headers().get(ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
        Ok(Fetch::Changed(resp.json().await?, tag))
    }

    /// Post one batch (≤ [`MAX_BATCH`]).
    pub async fn post_results(&self, batch: &[ResultItem]) -> Result<ResultsResponse, ClientError> {
        let resp = self
            .http
            .post(format!("{}{RESULTS_PATH}", self.base))
            .json(batch)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Self::error_for(resp).await);
        }
        Ok(resp.json().await?)
    }
}

// ---------------------------------------------------------------------------
// Result buffer
// ---------------------------------------------------------------------------

/// Bounded FIFO of results waiting to be posted.
#[derive(Clone, Default)]
pub struct Buffer {
    inner: Arc<Mutex<VecDeque<ResultItem>>>,
    cap: usize,
}

impl Buffer {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::default(),
            cap: cap.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<ResultItem>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add a result; returns how many old results were dropped to make room.
    pub fn push(&self, item: ResultItem) -> usize {
        let mut q = self.lock();
        let mut dropped = 0;
        while q.len() >= self.cap {
            q.pop_front();
            dropped += 1;
        }
        q.push_back(item);
        dropped
    }

    /// Take up to `n` of the oldest results.
    pub fn take(&self, n: usize) -> Vec<ResultItem> {
        let mut q = self.lock();
        let n = n.min(q.len());
        q.drain(..n).collect()
    }

    /// Put a batch back at the front after a failed post (respecting the cap:
    /// the oldest are dropped first).
    pub fn requeue(&self, batch: Vec<ResultItem>) -> usize {
        let mut q = self.lock();
        for item in batch.into_iter().rev() {
            q.push_front(item);
        }
        let mut dropped = 0;
        while q.len() > self.cap {
            q.pop_front();
            dropped += 1;
        }
        dropped
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Post everything buffered. Stops at the first failure (results are put back).
async fn flush(client: &Client, buffer: &Buffer) -> Result<usize, ClientError> {
    let mut sent = 0;
    loop {
        let batch = buffer.take(MAX_BATCH);
        if batch.is_empty() {
            return Ok(sent);
        }
        match client.post_results(&batch).await {
            Ok(resp) => {
                sent += resp.accepted;
                for r in &resp.rejected {
                    tracing::debug!(monitor_id = r.monitor_id, reason = %r.reason, "server rejected result");
                }
            }
            Err(err) => {
                if err.is_transient() {
                    let dropped = buffer.requeue(batch);
                    if dropped > 0 {
                        tracing::warn!(dropped, "result buffer full; dropped oldest results");
                    }
                }
                return Err(err);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

async fn check_once(monitor: &Monitor, permits: &Semaphore) -> Option<ResultItem> {
    let _permit = permits.acquire().await.ok()?;
    let checked_at = now_db();
    let outcome = checkers::run_check(monitor).await;
    Some(ResultItem {
        monitor_id: monitor.id,
        status: outcome.status,
        response_time_ms: outcome.response_time_ms,
        message: outcome.message,
        checked_at: Some(checked_at),
    })
}

/// Seconds to wait before the next check: the interval while up, the retry
/// interval otherwise, ±5% jitter.
fn next_delay(monitor: &Monitor, last: Option<HeartbeatStatus>) -> Duration {
    let base = match last {
        Some(HeartbeatStatus::Up) | None => monitor.interval_seconds,
        Some(_) => monitor.effective_retry_interval(),
    }
    .max(1) as f64;
    let jitter = rand::rng().random_range(-0.05..=0.05);
    Duration::from_secs_f64((base * (1.0 + jitter)).max(1.0))
}

fn spawn_monitor(monitor: Monitor, permits: Arc<Semaphore>, buffer: Buffer) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Spread first checks so a fresh agent doesn't fire everything at once.
        let spread = monitor.interval_seconds.clamp(1, 30) as f64;
        let first = rand::rng().random_range(0.0..spread);
        tokio::time::sleep(Duration::from_secs_f64(first)).await;
        loop {
            let Some(item) = check_once(&monitor, &permits).await else { return };
            let status = item.status;
            tracing::debug!(monitor_id = monitor.id, status = %status, ms = ?item.response_time_ms, "check done");
            let dropped = buffer.push(item);
            if dropped > 0 {
                tracing::warn!(dropped, "result buffer full; dropped oldest results");
            }
            tokio::time::sleep(next_delay(&monitor, Some(status))).await;
        }
    })
}

/// Summary printed by `--once`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OnceSummary {
    pub agent: String,
    pub checked: usize,
    pub up: usize,
    pub failed: usize,
    pub accepted: usize,
    pub rejected: usize,
}

impl std::fmt::Display for OnceSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "agent “{}”: {} checks, {} up, {} not up; server accepted {}, rejected {}",
            self.agent, self.checked, self.up, self.failed, self.accepted, self.rejected
        )
    }
}

/// Fetch, check every monitor once, post, return a summary.
pub async fn run_once(client: &Client, concurrency: usize) -> Result<OnceSummary, AgentError> {
    let Fetch::Changed(list, _) = client.fetch_monitors(None).await? else {
        return Err("server answered 304 without an ETag".into());
    };
    let permits = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    for m in list.monitors {
        let permits = permits.clone();
        tasks.spawn(async move {
            let item = check_once(&m, &permits).await;
            if let Some(item) = &item {
                println!(
                    "{:>5}  {:<11} {:>7}  {}  {}",
                    m.id,
                    item.status.as_str(),
                    item.response_time_ms.map(|ms| format!("{ms} ms")).unwrap_or_default(),
                    m.name,
                    item.message.as_deref().unwrap_or("")
                );
            }
            item
        });
    }
    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        if let Ok(Some(item)) = joined {
            results.push(item);
        }
    }
    let mut summary = OnceSummary {
        agent: list.agent.name,
        checked: results.len(),
        up: results.iter().filter(|r| r.status == HeartbeatStatus::Up).count(),
        ..OnceSummary::default()
    };
    summary.failed = summary.checked - summary.up;
    for chunk in results.chunks(MAX_BATCH) {
        let resp = client.post_results(chunk).await?;
        summary.accepted += resp.accepted;
        summary.rejected += resp.rejected.len();
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

struct Running {
    config: Monitor,
    handle: JoinHandle<()>,
}

/// Make the running tasks match `monitors`. Returns (started, stopped).
fn reconcile(running: &mut HashMap<i64, Running>, monitors: Vec<Monitor>, permits: &Arc<Semaphore>, buffer: &Buffer) -> (usize, usize) {
    let mut started = 0;
    let mut stopped = 0;
    let wanted: HashMap<i64, Monitor> = monitors.into_iter().map(|m| (m.id, m)).collect();
    running.retain(|id, r| {
        let keep = wanted.get(id).is_some_and(|m| *m == r.config);
        if !keep {
            r.handle.abort();
            stopped += 1;
        }
        keep
    });
    for (id, m) in wanted {
        if running.contains_key(&id) {
            continue;
        }
        let handle = spawn_monitor(m.clone(), permits.clone(), buffer.clone());
        running.insert(id, Running { config: m, handle });
        started += 1;
    }
    (started, stopped)
}

fn backoff_after(failures: u32) -> Duration {
    let secs = 2u64.saturating_pow(failures.min(6)).min(MAX_BACKOFF.as_secs());
    Duration::from_secs(secs.max(1))
}

/// The long-running agent: poll, check, flush, until `stop` turns true.
pub async fn run_loop(client: Client, concurrency: usize, mut stop: watch::Receiver<bool>) -> Result<(), AgentError> {
    let permits = Arc::new(Semaphore::new(concurrency.max(1)));
    let buffer = Buffer::new(MAX_BUFFER);
    let mut running: HashMap<i64, Running> = HashMap::new();
    let mut etag: Option<String> = None;
    let mut poll_every = Duration::from_secs(POLL_SECONDS);
    let mut connected_once = false;
    let mut poll_failures = 0u32;
    let mut flush_failures = 0u32;
    let mut next_poll = tokio::time::Instant::now();
    let mut next_flush = tokio::time::Instant::now() + FLUSH_EVERY;

    tracing::info!(server = %client.base, version = VERSION, "agent starting");
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            () = tokio::time::sleep_until(next_poll) => {
                match client.fetch_monitors(etag.as_deref()).await {
                    Ok(Fetch::Changed(list, tag)) => {
                        if !connected_once {
                            tracing::info!(agent = %list.agent.name, location = %list.agent.location, monitors = list.monitors.len(), "connected to server");
                            connected_once = true;
                        }
                        poll_every = Duration::from_secs(list.poll_seconds.clamp(5, 600));
                        let count = list.monitors.len();
                        let (started, stopped) = reconcile(&mut running, list.monitors, &permits, &buffer);
                        if started > 0 || stopped > 0 {
                            tracing::info!(monitors = count, started, stopped, "monitor list updated");
                        }
                        etag = tag;
                        poll_failures = 0;
                        next_poll = tokio::time::Instant::now() + poll_every;
                    }
                    Ok(Fetch::NotModified) => {
                        poll_failures = 0;
                        next_poll = tokio::time::Instant::now() + poll_every;
                    }
                    Err(err @ (ClientError::Unauthorized | ClientError::Disabled)) => {
                        if !connected_once {
                            stop_all(&mut running);
                            return Err(err.into());
                        }
                        tracing::error!(%err, "server refused the agent; pausing checks");
                        stop_all(&mut running);
                        etag = None;
                        next_poll = tokio::time::Instant::now() + poll_every;
                    }
                    Err(err) => {
                        poll_failures += 1;
                        let wait = backoff_after(poll_failures).min(poll_every);
                        tracing::warn!(%err, retry_in = ?wait, "fetching monitors failed");
                        next_poll = tokio::time::Instant::now() + wait;
                    }
                }
            }
            () = tokio::time::sleep_until(next_flush) => {
                match flush(&client, &buffer).await {
                    Ok(sent) => {
                        if flush_failures > 0 {
                            tracing::info!(sent, "server reachable again; results delivered");
                        }
                        flush_failures = 0;
                        next_flush = tokio::time::Instant::now() + FLUSH_EVERY;
                    }
                    Err(err) => {
                        flush_failures += 1;
                        let wait = backoff_after(flush_failures).max(FLUSH_EVERY);
                        tracing::warn!(%err, buffered = buffer.len(), retry_in = ?wait, "posting results failed");
                        next_flush = tokio::time::Instant::now() + wait;
                    }
                }
            }
        }
    }

    stop_all(&mut running);
    if !buffer.is_empty() {
        match tokio::time::timeout(Duration::from_secs(5), flush(&client, &buffer)).await {
            Ok(Ok(sent)) => tracing::info!(sent, "flushed results before exit"),
            Ok(Err(err)) => tracing::warn!(%err, lost = buffer.len(), "could not flush results before exit"),
            Err(_) => tracing::warn!(lost = buffer.len(), "timed out flushing results before exit"),
        }
    }
    tracing::info!("agent stopped");
    Ok(())
}

fn stop_all(running: &mut HashMap<i64, Running>) {
    for (_, r) in running.drain() {
        r.handle.abort();
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64) -> ResultItem {
        ResultItem {
            monitor_id: id,
            status: HeartbeatStatus::Up,
            response_time_ms: Some(1),
            message: None,
            checked_at: None,
        }
    }

    #[test]
    fn buffer_is_bounded_and_fifo() {
        let b = Buffer::new(3);
        for i in 0..5 {
            b.push(item(i));
        }
        assert_eq!(b.len(), 3);
        let ids: Vec<i64> = b.take(10).iter().map(|r| r.monitor_id).collect();
        assert_eq!(ids, vec![2, 3, 4]);
    }

    #[test]
    fn requeue_puts_batch_back_in_front() {
        let b = Buffer::new(4);
        b.push(item(1));
        b.push(item(2));
        let batch = b.take(2);
        b.push(item(3));
        assert_eq!(b.requeue(batch), 0);
        let ids: Vec<i64> = b.take(10).iter().map(|r| r.monitor_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_after(1), Duration::from_secs(2));
        assert_eq!(backoff_after(3), Duration::from_secs(8));
        assert_eq!(backoff_after(30), MAX_BACKOFF);
    }

    #[test]
    fn client_rejects_bad_arguments() {
        let args = |server: &str, token: &str| AgentArgs {
            server: server.into(),
            token: token.into(),
            name: None,
            once: false,
            concurrency: 1,
        };
        assert!(Client::new(&args("ftp://x", "vva_x")).is_err());
        assert!(Client::new(&args("http://x", "vv_api")).is_err());
        assert!(Client::new(&args("http://x/", "vva_x")).is_ok());
    }
}
