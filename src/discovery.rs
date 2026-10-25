//! Docker label discovery. Owned by the integrations agent.
//!
//! When the `discovery_enabled` setting is on, containers labelled
//! `vigilant.enable=true` on the Docker host (`discovery_docker_host`, empty =
//! local socket) become monitors with `source = 'docker'` and
//! `external_id = <container name>`. A sync runs every 60 seconds, right away
//! when container events arrive, and on demand from Settings → Integrations.
//!
//! - New labelled container → monitor created (plus its group, tags and
//!   notification links).
//! - Labels changed → monitor updated (fields managed by labels only).
//! - Container removed → monitor paused (remembered in `discovery_paused`);
//!   container back → resumed. Monitors a user paused by hand stay paused.
//! - Monitors with `source = 'manual'` (or `'config'`) are never touched.
//!
//! The decision logic is the pure [`plan`] function; [`sync_once`] gathers
//! inputs, applies the plan in one transaction and reloads the scheduler.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use bollard::query_parameters::{EventsOptions, ListContainersOptions};
use bollard::{API_DEFAULT_VERSION, Docker};
use futures_util::StreamExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use tokio::sync::Notify;

use crate::agent::protocol::{now_db, parse_db};
use crate::api::AppState;
use crate::db::events as db_events;
use crate::db::models::{Monitor, MonitorType, NewMonitor};
use crate::db::monitors::{self as db_monitors, MONITOR_COLUMNS, row_to_monitor};

/// Label namespace.
pub const LABEL_PREFIX: &str = "vigilant.";
/// `source` value of discovered monitors.
pub const SOURCE: &str = "docker";
/// `external_id` prefix of group monitors created from `vigilant.group`.
pub const GROUP_PREFIX: &str = "group:";
/// Seconds between periodic syncs.
const SYNC_EVERY: Duration = Duration::from_secs(60);
/// Bound on one Docker API call.
const DOCKER_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_INTERVAL: i64 = 20;
const MAX_INTERVAL: i64 = 86_400;
const MAX_RETRIES: i64 = 10;
const MAX_TIMEOUT: i64 = 300;
/// Tag colours handed out to auto-created tags (design-system palette names).
const TAG_COLORS: [&str; 10] = [
    "blue", "green", "orange", "purple", "teal", "pink", "indigo", "mint", "cyan", "yellow",
];

/// `(label, example, meaning)` for the settings page cheat-sheet.
pub const LABEL_REFERENCE: [(&str, &str, &str); 12] = [
    (
        "vigilant.enable",
        "true",
        "Required. Monitor this container.",
    ),
    (
        "vigilant.name",
        "Payments API",
        "Display name. Defaults to the container name.",
    ),
    (
        "vigilant.type",
        "http",
        "Monitor type: docker (default), http, keyword, json_query, tcp, ping, dns, websocket, ntp, steam, domain_expiry.",
    ),
    (
        "vigilant.target",
        "http://payments:8080/health",
        "URL or host:port to check. Required for every type except docker.",
    ),
    (
        "vigilant.interval",
        "60",
        "Seconds between checks (20 to 86400).",
    ),
    (
        "vigilant.retries",
        "2",
        "Failed checks before the monitor goes down (0 to 10).",
    ),
    (
        "vigilant.timeout",
        "10",
        "Seconds before a check gives up (1 to 300).",
    ),
    (
        "vigilant.keyword",
        "OK",
        "Text the response must contain (keyword type).",
    ),
    (
        "vigilant.group",
        "Production",
        "Put the monitor in this group. The group is created if needed.",
    ),
    (
        "vigilant.tags",
        "prod,team:payments",
        "Comma-separated tags; name:value sets a tag value.",
    ),
    (
        "vigilant.notify",
        "Ops Discord,Pager",
        "Comma-separated notification channel names to link.",
    ),
    (
        "vigilant.description",
        "Customer checkout",
        "Description shown on the monitor page.",
    ),
];

/// Example for the settings page.
pub const COMPOSE_EXAMPLE: &str = "services:
  payments:
    image: ghcr.io/acme/payments:latest
    labels:
      vigilant.enable: \"true\"
      vigilant.name: \"Payments API\"
      vigilant.type: \"http\"
      vigilant.target: \"http://payments:8080/health\"
      vigilant.interval: \"30\"
      vigilant.group: \"Production\"
      vigilant.tags: \"prod,team:payments\"
      vigilant.notify: \"Ops Discord\"
  redis:
    image: redis:7
    labels:
      vigilant.enable: \"true\"   # type defaults to docker: running + healthy
";

// ---------------------------------------------------------------------------
// Status (for the settings page and /metrics)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DiscoveryStatus {
    pub enabled: bool,
    /// UTC `YYYY-MM-DD HH:MM:SS` of the last sync attempt.
    pub last_sync_at: Option<String>,
    pub last_sync_unix: Option<i64>,
    pub last_error: Option<String>,
    /// "5 containers · 1 created · 0 updated · 0 paused · 0 resumed".
    pub last_summary: Option<String>,
    /// Per-container problems from the last sync (`container: message`).
    pub warnings: Vec<String>,
    /// Monitors managed by discovery (excluding groups).
    pub managed: usize,
    /// Whether the Docker events stream is connected.
    pub events_connected: bool,
}

static STATUS: LazyLock<Mutex<DiscoveryStatus>> =
    LazyLock::new(|| Mutex::new(DiscoveryStatus::default()));
static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);
static RECONFIGURE: LazyLock<Notify> = LazyLock::new(Notify::new);
/// One sync at a time (periodic, event-driven and "Sync now").
static SYNC_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

fn update_status(f: impl FnOnce(&mut DiscoveryStatus)) {
    let mut guard = STATUS.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut guard);
}

/// Snapshot of the discovery job's state.
pub fn status() -> DiscoveryStatus {
    STATUS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Ask the background job to re-read settings and sync soon.
pub fn trigger() {
    WAKE.notify_one();
    RECONFIGURE.notify_one();
}

// ---------------------------------------------------------------------------
// Label parsing and planning (pure)
// ---------------------------------------------------------------------------

/// A container as discovery sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// Without the leading `/`.
    pub name: String,
    pub labels: HashMap<String, String>,
}

/// The monitor a container's labels ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// Container name.
    pub external_id: String,
    pub name: String,
    pub description: String,
    pub monitor_type: MonitorType,
    pub target: String,
    pub interval_seconds: i64,
    pub retries: i64,
    pub timeout_seconds: Option<i64>,
    pub keyword: Option<String>,
    pub docker_host: Option<String>,
    pub group: Option<String>,
    /// `(name, value)`.
    pub tags: Vec<(String, String)>,
    /// Notification channel names.
    pub notify: Vec<String>,
}

impl Spec {
    /// Whether `m`'s label-managed fields differ from this spec.
    pub fn differs_from(&self, m: &Monitor) -> bool {
        m.name != self.name
            || m.description != self.description
            || m.monitor_type != self.monitor_type
            || m.target != self.target
            || m.interval_seconds != self.interval_seconds
            || m.retries != self.retries
            || self.timeout_seconds.is_some_and(|t| t != m.timeout_seconds)
            || (self.keyword.is_some() && m.keyword != self.keyword)
            || m.docker_host != self.docker_host
    }

    /// Copy the label-managed fields onto `m`.
    pub fn apply_to(&self, m: &mut NewMonitor) {
        m.name = self.name.clone();
        m.description = self.description.clone();
        m.monitor_type = self.monitor_type;
        m.target = self.target.clone();
        m.interval_seconds = self.interval_seconds;
        m.retries = self.retries;
        if let Some(t) = self.timeout_seconds {
            m.timeout_seconds = t;
        }
        if self.keyword.is_some() {
            m.keyword = self.keyword.clone();
        }
        m.docker_host = self.docker_host.clone();
        m.source = SOURCE.to_string();
        m.external_id = Some(self.external_id.clone());
    }
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

fn label<'a>(c: &'a Container, key: &str) -> Option<&'a str> {
    c.labels
        .get(&format!("{LABEL_PREFIX}{key}"))
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

fn int_label(c: &Container, key: &str, min: i64, max: i64) -> Result<Option<i64>, String> {
    let Some(raw) = label(c, key) else {
        return Ok(None);
    };
    match raw.parse::<i64>() {
        Ok(n) if (min..=max).contains(&n) => Ok(Some(n)),
        _ => Err(format!(
            "{LABEL_PREFIX}{key} must be a whole number from {min} to {max}, got “{raw}”"
        )),
    }
}

fn split_list(v: Option<&str>) -> Vec<String> {
    let mut seen = HashSet::new();
    v.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.to_string()))
        .map(str::to_string)
        .collect()
}

/// Parse a container's labels. `Ok(None)` when it isn't enabled.
pub fn parse_labels(c: &Container, docker_host: Option<&str>) -> Result<Option<Spec>, String> {
    if !label(c, "enable").is_some_and(truthy) {
        return Ok(None);
    }
    let monitor_type = match label(c, "type") {
        None => MonitorType::Docker,
        Some(raw) => {
            let norm = raw.to_ascii_lowercase().replace('-', "_");
            let norm = match norm.as_str() {
                "https" => "http".to_string(),
                "port" => "tcp".to_string(),
                "container" => "docker".to_string(),
                _ => norm,
            };
            match MonitorType::parse(&norm) {
                Some(t) if t.is_active_check() => t,
                Some(t) => {
                    return Err(format!(
                        "{LABEL_PREFIX}type “{}” can’t be discovered",
                        t.as_str()
                    ));
                }
                None => return Err(format!("unknown {LABEL_PREFIX}type “{raw}”")),
            }
        }
    };
    let target = match (monitor_type, label(c, "target")) {
        (_, Some(t)) => t.to_string(),
        (MonitorType::Docker, None) => c.name.clone(),
        (t, None) => {
            return Err(format!(
                "{LABEL_PREFIX}target is required for type {}",
                t.as_str()
            ));
        }
    };
    let defaults = NewMonitor::default();
    let interval_seconds =
        int_label(c, "interval", MIN_INTERVAL, MAX_INTERVAL)?.unwrap_or(defaults.interval_seconds);
    let retries = int_label(c, "retries", 0, MAX_RETRIES)?.unwrap_or(defaults.retries);
    let timeout_seconds = int_label(c, "timeout", 1, MAX_TIMEOUT)?;
    let tags = split_list(label(c, "tags"))
        .into_iter()
        .map(|t| match t.split_once(':') {
            Some((n, v)) => (n.trim().to_string(), v.trim().to_string()),
            None => (t, String::new()),
        })
        .filter(|(n, _)| !n.is_empty())
        .collect();
    let name = label(c, "name")
        .unwrap_or(&c.name)
        .chars()
        .take(120)
        .collect();
    let description = label(c, "description")
        .map(str::to_string)
        .unwrap_or_else(|| format!("Discovered from Docker container “{}”.", c.name));
    Ok(Some(Spec {
        external_id: c.name.clone(),
        name,
        description,
        monitor_type,
        target,
        interval_seconds,
        retries,
        timeout_seconds,
        keyword: label(c, "keyword").map(str::to_string),
        docker_host: if monitor_type == MonitorType::Docker {
            docker_host
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string)
        } else {
            None
        },
        group: label(c, "group").map(|g| g.chars().take(120).collect()),
        tags,
        notify: split_list(label(c, "notify")),
    }))
}

/// A discovered monitor already in the database.
#[derive(Debug, Clone)]
pub struct Existing {
    pub monitor: Monitor,
    /// Paused by discovery because its container disappeared.
    pub paused_by_discovery: bool,
}

/// What a sync should do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub create: Vec<Spec>,
    /// Label-managed fields changed.
    pub update: Vec<(i64, Spec)>,
    /// Nothing to change (tags/notify/group links are still ensured).
    pub keep: Vec<(i64, Spec)>,
    /// Container gone: pause.
    pub pause: Vec<i64>,
    /// Container back after discovery paused it.
    pub resume: Vec<(i64, Spec)>,
    /// `(container, problem)`.
    pub errors: Vec<(String, String)>,
}

impl Plan {
    pub fn summary(&self, containers: usize) -> String {
        format!(
            "{containers} labelled {} · {} created · {} updated · {} paused · {} resumed",
            if containers == 1 {
                "container"
            } else {
                "containers"
            },
            self.create.len(),
            self.update.len(),
            self.pause.len(),
            self.resume.len()
        )
    }
}

/// Decide creates/updates/pauses/resumes. `existing` must only contain
/// `source = 'docker'` monitors; group monitors created by discovery are
/// ignored here.
pub fn plan(existing: &[Existing], containers: &[Container], docker_host: Option<&str>) -> Plan {
    let mut out = Plan::default();
    let by_external: HashMap<&str, &Existing> = existing
        .iter()
        .filter(|e| e.monitor.source == SOURCE && e.monitor.monitor_type != MonitorType::Group)
        .filter_map(|e| e.monitor.external_id.as_deref().map(|x| (x, e)))
        .filter(|(x, _)| !x.starts_with(GROUP_PREFIX))
        .collect();
    let mut present: HashSet<&str> = HashSet::new();
    for c in containers {
        let spec = match parse_labels(c, docker_host) {
            Ok(Some(spec)) => spec,
            Ok(None) => continue,
            Err(e) => {
                out.errors.push((c.name.clone(), e));
                // Keep a broken-label container's monitor as is; don't pause it.
                present.insert(c.name.as_str());
                continue;
            }
        };
        present.insert(c.name.as_str());
        match by_external.get(c.name.as_str()) {
            None => out.create.push(spec),
            Some(e) if e.paused_by_discovery && !e.monitor.active => {
                out.resume.push((e.monitor.id, spec))
            }
            Some(e) if spec.differs_from(&e.monitor) => out.update.push((e.monitor.id, spec)),
            Some(e) => out.keep.push((e.monitor.id, spec)),
        }
    }
    for (external, e) in &by_external {
        if !present.contains(external) && e.monitor.active {
            out.pause.push(e.monitor.id);
        }
    }
    out.pause.sort_unstable();
    out
}

// ---------------------------------------------------------------------------
// Applying a plan (database)
// ---------------------------------------------------------------------------

/// Monitor ids whose scheduler state must be reloaded.
#[derive(Debug, Default)]
struct Applied {
    reload: Vec<i64>,
    warnings: Vec<String>,
}

fn load_existing(conn: &Connection) -> rusqlite::Result<Vec<Existing>> {
    let paused: HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT monitor_id FROM discovery_paused")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {MONITOR_COLUMNS} FROM monitors WHERE source = ?1 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![SOURCE], row_to_monitor)?;
    rows.map(|m| {
        m.map(|monitor| Existing {
            paused_by_discovery: paused.contains(&monitor.id),
            monitor,
        })
    })
    .collect()
}

fn ensure_group(conn: &Connection, name: &str, created: &mut Vec<i64>) -> rusqlite::Result<i64> {
    let external = format!("{GROUP_PREFIX}{name}");
    let found: Option<i64> = conn
        .query_row(
            "SELECT id FROM monitors WHERE source = ?1 AND external_id = ?2",
            params![SOURCE, external],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = found {
        return Ok(id);
    }
    // Reuse a manually created group with the same name rather than duplicating it.
    let manual: Option<i64> = conn
        .query_row(
            "SELECT id FROM monitors WHERE monitor_type = 'group' AND name = ?1 ORDER BY id LIMIT 1",
            params![name],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = manual {
        return Ok(id);
    }
    let id = db_monitors::insert_monitor(
        conn,
        &NewMonitor {
            name: name.to_string(),
            description: "Created by Docker discovery.".into(),
            monitor_type: MonitorType::Group,
            source: SOURCE.into(),
            external_id: Some(external),
            ..NewMonitor::default()
        },
    )?;
    created.push(id);
    Ok(id)
}

fn tag_color(name: &str) -> &'static str {
    let sum = name.bytes().fold(0usize, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(usize::from(b))
    });
    TAG_COLORS[sum % TAG_COLORS.len()]
}

/// Ensure tags and notification links (additive: never removes links).
fn ensure_links(
    conn: &Connection,
    monitor_id: i64,
    spec: &Spec,
    warnings: &mut Vec<String>,
) -> rusqlite::Result<()> {
    for (name, value) in &spec.tags {
        conn.execute(
            "INSERT INTO tags (name, color) VALUES (?1, ?2) ON CONFLICT(name) DO NOTHING",
            params![name, tag_color(name)],
        )?;
        conn.execute(
            "INSERT INTO monitor_tags (monitor_id, tag_id, value)
             SELECT ?1, id, ?3 FROM tags WHERE name = ?2
             ON CONFLICT(monitor_id, tag_id) DO UPDATE SET value = excluded.value",
            params![monitor_id, name, value],
        )?;
    }
    for channel in &spec.notify {
        let exists: Option<i64> = conn
            .query_row(
                "SELECT id FROM notification_channels WHERE name = ?1 LIMIT 1",
                params![channel],
                |r| r.get(0),
            )
            .optional()?;
        match exists {
            Some(channel_id) => {
                conn.execute(
                    "INSERT OR IGNORE INTO monitor_notifications (monitor_id, channel_id, escalation)
                     VALUES (?1, ?2, 0)",
                    params![monitor_id, channel_id],
                )?;
            }
            None => warnings.push(format!(
                "{}: no notification channel named “{channel}”",
                spec.external_id
            )),
        }
    }
    Ok(())
}

fn desired_parent(
    conn: &Connection,
    spec: &Spec,
    created: &mut Vec<i64>,
) -> rusqlite::Result<Option<i64>> {
    spec.group
        .as_deref()
        .map(|g| ensure_group(conn, g, created))
        .transpose()
}

fn apply_plan(conn: &mut Connection, plan: &Plan) -> rusqlite::Result<Applied> {
    let tx = conn.transaction()?;
    let mut applied = Applied::default();
    let mut groups_created = Vec::new();

    for spec in &plan.create {
        let mut m = NewMonitor::default();
        spec.apply_to(&mut m);
        m.parent_id = desired_parent(&tx, spec, &mut groups_created)?;
        let id = db_monitors::insert_monitor(&tx, &m)?;
        ensure_links(&tx, id, spec, &mut applied.warnings)?;
        applied.reload.push(id);
    }

    let resumes = plan.resume.iter().map(|(id, s)| (id, s, true));
    let updates = plan.update.iter().map(|(id, s)| (id, s, false));
    for (&id, spec, resume) in resumes.chain(updates) {
        let Some(existing) = db_monitors::get_monitor(&tx, id)? else {
            continue;
        };
        let mut m = existing.to_new();
        spec.apply_to(&mut m);
        if spec.group.is_some() {
            m.parent_id = desired_parent(&tx, spec, &mut groups_created)?;
        }
        if resume {
            m.active = true;
            tx.execute(
                "DELETE FROM discovery_paused WHERE monitor_id = ?1",
                params![id],
            )?;
            db_events::log_resumed(&tx, id)?;
        }
        db_monitors::update_monitor(&tx, id, &m)?;
        ensure_links(&tx, id, spec, &mut applied.warnings)?;
        applied.reload.push(id);
    }

    for (id, spec) in &plan.keep {
        let parent = desired_parent(&tx, spec, &mut groups_created)?;
        if spec.group.is_some() {
            let current: Option<i64> = tx.query_row(
                "SELECT parent_id FROM monitors WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            if current != parent {
                tx.execute(
                    "UPDATE monitors SET parent_id = ?2, updated_at = datetime('now') WHERE id = ?1",
                    params![id, parent],
                )?;
                applied.reload.push(*id);
            }
        }
        ensure_links(&tx, *id, spec, &mut applied.warnings)?;
    }

    for &id in &plan.pause {
        let Some(existing) = db_monitors::get_monitor(&tx, id)? else {
            continue;
        };
        let mut m = existing.to_new();
        m.active = false;
        let container = existing.external_id.clone().unwrap_or_default();
        m.description =
            format!("Paused by Docker discovery: container “{container}” no longer exists.");
        db_events::log_paused(&tx, id)?;
        db_monitors::update_monitor(&tx, id, &m)?;
        tx.execute(
            "INSERT INTO discovery_paused (monitor_id) VALUES (?1) ON CONFLICT(monitor_id) DO NOTHING",
            params![id],
        )?;
        applied.reload.push(id);
    }

    tx.commit()?;
    applied.reload.extend(groups_created);
    applied.reload.sort_unstable();
    applied.reload.dedup();
    Ok(applied)
}

fn count_managed(conn: &Connection) -> rusqlite::Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM monitors WHERE source = ?1 AND monitor_type != 'group'",
        params![SOURCE],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(n).unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Docker
// ---------------------------------------------------------------------------

/// Check a Docker host setting before saving it.
pub fn validate_docker_host(host: &str) -> Result<(), String> {
    let h = host.trim();
    if h.is_empty()
        || h.starts_with("unix://")
        || h.starts_with('/')
        || h.starts_with("npipe://")
        || h.starts_with("tcp://")
        || h.starts_with("http://")
    {
        return Ok(());
    }
    if h.starts_with("https://") {
        return Err("TLS Docker hosts (https://) aren’t supported. Use a socket proxy over tcp:// or the local socket.".into());
    }
    Err(format!(
        "Enter a Docker host like unix:///var/run/docker.sock or tcp://docker-proxy:2375, not “{h}”"
    ))
}

fn connect(host: &str) -> Result<Docker, String> {
    let fail = |e: bollard::errors::Error| format!("Can’t connect to Docker: {e}");
    let timeout = DOCKER_TIMEOUT.as_secs();
    let host = host.trim();
    if host.is_empty() {
        return Docker::connect_with_local_defaults().map_err(fail);
    }
    validate_docker_host(host)?;
    if host.starts_with("tcp://") || host.starts_with("http://") {
        return Docker::connect_with_http(host, timeout, API_DEFAULT_VERSION).map_err(fail);
    }
    if host.starts_with("npipe://") {
        #[cfg(windows)]
        {
            return Docker::connect_with_named_pipe(host, timeout, API_DEFAULT_VERSION)
                .map_err(fail);
        }
        #[cfg(not(windows))]
        {
            return Err("npipe:// Docker hosts only work on Windows".into());
        }
    }
    #[cfg(unix)]
    {
        Docker::connect_with_unix(host, timeout, API_DEFAULT_VERSION).map_err(fail)
    }
    #[cfg(not(unix))]
    {
        Err("unix:// Docker sockets aren’t available on this platform".into())
    }
}

fn label_filter() -> HashMap<String, Vec<String>> {
    HashMap::from([("label".to_string(), vec![format!("{LABEL_PREFIX}enable")])])
}

async fn list_containers(docker: &Docker) -> Result<Vec<Container>, String> {
    let options = ListContainersOptions {
        all: true,
        filters: Some(label_filter()),
        ..ListContainersOptions::default()
    };
    let list = tokio::time::timeout(DOCKER_TIMEOUT, docker.list_containers(Some(options)))
        .await
        .map_err(|_| "Docker didn’t answer within 30 seconds".to_string())?
        .map_err(|e| format!("Listing containers failed: {e}"))?;
    Ok(list
        .into_iter()
        .filter_map(|c| {
            let name = c
                .names?
                .into_iter()
                .next()?
                .trim_start_matches('/')
                .to_string();
            Some(Container {
                name,
                labels: c.labels.unwrap_or_default(),
            })
        })
        .collect())
}

/// Run one sync now. Returns a one-line summary.
pub async fn sync_once(state: &AppState) -> Result<String, String> {
    let _guard = SYNC_LOCK.lock().await;
    let settings = state.settings().await.map_err(|e| e.to_string())?;
    let host = settings.discovery_docker_host.clone().unwrap_or_default();
    let result = sync_inner(state, &host).await;
    let now = now_db();
    let unix = parse_db(&now).map(|t| t.unix_timestamp());
    let managed = state.with_conn(|c| count_managed(c)).await.unwrap_or(0);
    update_status(|s| {
        s.enabled = settings.discovery_enabled;
        s.last_sync_at = Some(now);
        s.last_sync_unix = unix;
        s.managed = managed;
        match &result {
            Ok((summary, warnings)) => {
                s.last_error = None;
                s.last_summary = Some(summary.clone());
                s.warnings = warnings.clone();
            }
            Err(err) => s.last_error = Some(err.clone()),
        }
    });
    result.map(|(summary, _)| summary)
}

async fn sync_inner(state: &AppState, host: &str) -> Result<(String, Vec<String>), String> {
    let docker = connect(host)?;
    let containers = list_containers(&docker).await?;
    let host_opt = Some(host)
        .filter(|h| !h.trim().is_empty())
        .map(str::to_string);
    let containers_for_plan = containers.clone();
    let (plan, applied) = state
        .with_conn(move |c| {
            let existing = load_existing(c)?;
            let plan = plan(&existing, &containers_for_plan, host_opt.as_deref());
            let applied = apply_plan(c, &plan)?;
            Ok((plan, applied))
        })
        .await
        .map_err(|e| format!("Saving discovered monitors failed: {e}"))?;

    for id in &applied.reload {
        state.scheduler.reload(*id).await;
    }
    if !applied.reload.is_empty() {
        state.notify_update();
    }
    let labelled = containers
        .iter()
        .filter(|c| {
            c.labels
                .get(&format!("{LABEL_PREFIX}enable"))
                .is_some_and(|v| truthy(v))
        })
        .count();
    let summary = plan.summary(labelled);
    let mut warnings: Vec<String> = plan
        .errors
        .iter()
        .map(|(c, e)| format!("{c}: {e}"))
        .collect();
    warnings.extend(applied.warnings);
    if !plan.create.is_empty()
        || !plan.update.is_empty()
        || !plan.pause.is_empty()
        || !plan.resume.is_empty()
    {
        tracing::info!(
            created = plan.create.len(),
            updated = plan.update.len(),
            paused = plan.pause.len(),
            resumed = plan.resume.len(),
            "docker discovery applied changes"
        );
    }
    for w in &warnings {
        tracing::warn!(warning = %w, "docker discovery");
    }
    Ok((summary, warnings))
}

// ---------------------------------------------------------------------------
// Background job
// ---------------------------------------------------------------------------

/// Called once at server start (see `server::run`).
pub fn spawn(state: AppState) {
    let sync_state = state.clone();
    tokio::spawn(async move {
        // Let the server finish starting before talking to Docker.
        tokio::time::sleep(Duration::from_secs(3)).await;
        loop {
            match sync_state.settings().await {
                Ok(s) if s.discovery_enabled => {
                    if let Err(err) = sync_once(&sync_state).await {
                        tracing::warn!(%err, "docker discovery sync failed");
                    }
                }
                Ok(_) => update_status(|s| {
                    s.enabled = false;
                    s.events_connected = false;
                }),
                Err(err) => tracing::warn!(%err, "docker discovery: failed to read settings"),
            }
            tokio::select! {
                () = tokio::time::sleep(SYNC_EVERY) => {}
                () = WAKE.notified() => {
                    // Coalesce bursts of container events (compose up).
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });
    tokio::spawn(watch_events(state));
}

/// Follow the Docker events stream and wake the sync loop on container
/// changes. Reconnects with backoff; re-reads settings when they change.
async fn watch_events(state: AppState) {
    let mut backoff = Duration::from_secs(5);
    loop {
        let settings = match state.settings().await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        if !settings.discovery_enabled {
            update_status(|s| s.events_connected = false);
            RECONFIGURE.notified().await;
            continue;
        }
        let host = settings.discovery_docker_host.unwrap_or_default();
        let docker = match connect(&host) {
            Ok(d) => d,
            Err(err) => {
                tracing::debug!(%err, "docker discovery: events unavailable");
                wait_or_reconfigure(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(300));
                continue;
            }
        };
        let mut filters = label_filter();
        filters.insert("type".into(), vec!["container".into()]);
        filters.insert(
            "event".into(),
            ["create", "start", "die", "destroy", "rename", "update"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        );
        let mut stream = docker.events(Some(EventsOptions {
            filters: Some(filters),
            ..EventsOptions::default()
        }));
        // Optimistic: the stream is idle until something happens; an error flips it back.
        update_status(|s| s.events_connected = true);
        loop {
            tokio::select! {
                item = stream.next() => match item {
                    Some(Ok(_event)) => {
                        backoff = Duration::from_secs(5);
                        WAKE.notify_one();
                    }
                    Some(Err(err)) => {
                        tracing::debug!(%err, "docker discovery: events stream error");
                        break;
                    }
                    None => break,
                },
                () = RECONFIGURE.notified() => break,
                // Re-check settings now and then even without events.
                () = tokio::time::sleep(Duration::from_secs(600)) => break,
            }
        }
        update_status(|s| s.events_connected = false);
        wait_or_reconfigure(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}

async fn wait_or_reconfigure(d: Duration) {
    tokio::select! {
        () = tokio::time::sleep(d) => {}
        () = RECONFIGURE.notified() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(name: &str, labels: &[(&str, &str)]) -> Container {
        Container {
            name: name.into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    fn existing_from(spec: &Spec, id: i64, active: bool, paused_by_discovery: bool) -> Existing {
        let mut m = NewMonitor::default();
        spec.apply_to(&mut m);
        Existing {
            monitor: Monitor {
                id,
                name: m.name,
                description: m.description,
                monitor_type: m.monitor_type,
                target: m.target,
                parent_id: None,
                interval_seconds: m.interval_seconds,
                retry_interval_seconds: None,
                timeout_seconds: m.timeout_seconds,
                retries: m.retries,
                active,
                upside_down: false,
                http_method: m.http_method,
                http_headers: None,
                http_body: None,
                http_body_encoding: m.http_body_encoding,
                accepted_status_codes: m.accepted_status_codes,
                max_redirects: m.max_redirects,
                ignore_tls: false,
                basic_auth_user: None,
                basic_auth_pass: None,
                secret: None,
                proxy_url: None,
                keyword: m.keyword,
                keyword_invert: false,
                json_path: None,
                json_operator: m.json_operator,
                json_expected: None,
                dns_record_type: m.dns_record_type,
                dns_resolver: None,
                dns_expected: None,
                docker_host: m.docker_host,
                push_token: None,
                expiry_notify: true,
                expiry_days: 14,
                resend_interval: 0,
                escalation_minutes: 0,
                flap_detection: true,
                run_on_agents: false,
                confirm_with_agents: false,
                source: m.source,
                external_id: m.external_id,
                sort_order: 0,
                created_at: String::new(),
                updated_at: String::new(),
                auth_method: "none".into(),
                oauth_token_url: None,
                oauth_client_id: None,
                oauth_client_secret: None,
                oauth_scope: None,
                oauth_audience: None,
                oauth_client_auth: "client_secret_basic".into(),
                tls_client_cert: None,
                tls_client_key: None,
                tls_ca_cert: None,
                remote_browser_id: None,
                browser_selector: None,
                browser_wait: "load".into(),
                browser_viewport: "desktop".into(),
                browser_js_errors: false,
                grpc_proto: None,
                check_from_agent_id: None,
            },
            paused_by_discovery,
        }
    }

    #[test]
    fn disabled_or_unlabelled_containers_are_ignored() {
        assert_eq!(parse_labels(&container("a", &[]), None), Ok(None));
        assert_eq!(
            parse_labels(&container("a", &[("vigilant.enable", "false")]), None),
            Ok(None)
        );
    }

    #[test]
    fn defaults_to_docker_type_on_container_name() {
        let spec = parse_labels(
            &container("redis", &[("vigilant.enable", "TRUE")]),
            Some("tcp://proxy:2375"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(spec.monitor_type, MonitorType::Docker);
        assert_eq!(spec.target, "redis");
        assert_eq!(spec.name, "redis");
        assert_eq!(spec.interval_seconds, 60);
        assert_eq!(spec.docker_host.as_deref(), Some("tcp://proxy:2375"));
    }

    #[test]
    fn parses_every_label() {
        let c = container(
            "web-1",
            &[
                ("vigilant.enable", "true"),
                ("vigilant.name", "Web"),
                ("vigilant.type", "keyword"),
                ("vigilant.target", "http://web:80/"),
                ("vigilant.interval", "30"),
                ("vigilant.retries", "2"),
                ("vigilant.timeout", "5"),
                ("vigilant.keyword", "OK"),
                ("vigilant.group", "Production"),
                ("vigilant.tags", "prod, team:payments ,prod"),
                ("vigilant.notify", "Ops Discord,Pager"),
            ],
        );
        let spec = parse_labels(&c, Some("tcp://x:2375")).unwrap().unwrap();
        assert_eq!(spec.name, "Web");
        assert_eq!(spec.monitor_type, MonitorType::Keyword);
        assert_eq!(spec.target, "http://web:80/");
        assert_eq!(
            (spec.interval_seconds, spec.retries, spec.timeout_seconds),
            (30, 2, Some(5))
        );
        assert_eq!(spec.keyword.as_deref(), Some("OK"));
        assert_eq!(spec.group.as_deref(), Some("Production"));
        assert_eq!(
            spec.tags,
            vec![
                ("prod".into(), String::new()),
                ("team".into(), "payments".into())
            ]
        );
        assert_eq!(
            spec.notify,
            vec!["Ops Discord".to_string(), "Pager".to_string()]
        );
        assert_eq!(
            spec.docker_host, None,
            "docker host only applies to docker monitors"
        );
    }

    #[test]
    fn rejects_bad_labels() {
        let bad = |labels: &[(&str, &str)]| {
            let mut all = vec![("vigilant.enable", "true")];
            all.extend_from_slice(labels);
            parse_labels(&container("c", &all), None).unwrap_err()
        };
        assert!(bad(&[("vigilant.type", "http")]).contains("target is required"));
        assert!(bad(&[("vigilant.type", "gopher")]).contains("unknown"));
        assert!(bad(&[("vigilant.type", "push")]).contains("can’t be discovered"));
        assert!(bad(&[("vigilant.interval", "5")]).contains("20 to 86400"));
        assert!(bad(&[("vigilant.retries", "lots")]).contains("whole number"));
    }

    #[test]
    fn type_aliases_are_accepted() {
        let spec = |t: &str| {
            parse_labels(
                &container(
                    "c",
                    &[
                        ("vigilant.enable", "1"),
                        ("vigilant.type", t),
                        ("vigilant.target", "x:1"),
                    ],
                ),
                None,
            )
            .unwrap()
            .unwrap()
            .monitor_type
        };
        assert_eq!(spec("HTTPS"), MonitorType::Http);
        assert_eq!(spec("json-query"), MonitorType::JsonQuery);
        assert_eq!(spec("port"), MonitorType::Tcp);
    }

    #[test]
    fn plan_creates_new_containers() {
        let p = plan(
            &[],
            &[
                container("a", &[("vigilant.enable", "true")]),
                container("b", &[]),
            ],
            None,
        );
        assert_eq!(p.create.len(), 1);
        assert_eq!(p.create[0].external_id, "a");
        assert!(p.update.is_empty() && p.pause.is_empty() && p.resume.is_empty());
    }

    #[test]
    fn plan_keeps_unchanged_and_updates_changed() {
        let c = container("a", &[("vigilant.enable", "true")]);
        let spec = parse_labels(&c, None).unwrap().unwrap();
        let existing = vec![existing_from(&spec, 5, true, false)];
        let p = plan(&existing, &[c], None);
        assert_eq!(p.keep.len(), 1);
        assert!(p.update.is_empty());

        let changed = container(
            "a",
            &[("vigilant.enable", "true"), ("vigilant.interval", "120")],
        );
        let p = plan(&existing, &[changed], None);
        assert_eq!(p.update.len(), 1);
        assert_eq!(p.update[0].0, 5);
        assert_eq!(p.update[0].1.interval_seconds, 120);
    }

    #[test]
    fn plan_pauses_missing_and_resumes_returning_containers() {
        let c = container("a", &[("vigilant.enable", "true")]);
        let spec = parse_labels(&c, None).unwrap().unwrap();

        let p = plan(&[existing_from(&spec, 5, true, false)], &[], None);
        assert_eq!(p.pause, vec![5]);

        // Already paused by discovery and still missing: nothing to do.
        let p = plan(&[existing_from(&spec, 5, false, true)], &[], None);
        assert!(p.pause.is_empty());

        // Back again: resume.
        let p = plan(
            &[existing_from(&spec, 5, false, true)],
            std::slice::from_ref(&c),
            None,
        );
        assert_eq!(p.resume.len(), 1);

        // Paused by a user (not by discovery): leave it paused.
        let p = plan(&[existing_from(&spec, 5, false, false)], &[c], None);
        assert!(p.resume.is_empty());
        assert_eq!(p.keep.len(), 1);
    }

    #[test]
    fn plan_ignores_manual_monitors_and_groups() {
        let c = container("a", &[("vigilant.enable", "true")]);
        let spec = parse_labels(&c, None).unwrap().unwrap();
        let mut manual = existing_from(&spec, 1, true, false);
        manual.monitor.source = "manual".into();
        let mut group = existing_from(&spec, 2, true, false);
        group.monitor.monitor_type = MonitorType::Group;
        group.monitor.external_id = Some("group:Prod".into());
        let p = plan(&[manual, group], &[], None);
        assert!(p.pause.is_empty());
        // The manual monitor with the same name doesn't count as discovered.
        let p = plan(
            &[existing_from(&spec, 1, true, false)].map(|mut e| {
                e.monitor.source = "manual".into();
                e
            }),
            &[c],
            None,
        );
        assert_eq!(p.create.len(), 1);
    }

    #[test]
    fn broken_labels_report_errors_without_pausing() {
        let good = container("a", &[("vigilant.enable", "true")]);
        let spec = parse_labels(&good, None).unwrap().unwrap();
        let broken = container(
            "a",
            &[("vigilant.enable", "true"), ("vigilant.type", "http")],
        );
        let p = plan(&[existing_from(&spec, 5, true, false)], &[broken], None);
        assert_eq!(p.errors.len(), 1);
        assert!(p.pause.is_empty());
    }

    #[test]
    fn apply_plan_creates_groups_tags_and_pauses() {
        let mut conn = crate::db::test_conn();
        conn.execute(
            "INSERT INTO notification_channels (name, kind, config) VALUES ('Ops', 'webhook', '{}')",
            [],
        )
        .unwrap();
        let c = container(
            "api",
            &[
                ("vigilant.enable", "true"),
                ("vigilant.group", "Prod"),
                ("vigilant.tags", "prod,team:core"),
                ("vigilant.notify", "Ops,Missing"),
            ],
        );
        let p = plan(
            &load_existing(&conn).unwrap(),
            std::slice::from_ref(&c),
            None,
        );
        let applied = apply_plan(&mut conn, &p).unwrap();
        assert_eq!(applied.reload.len(), 2, "monitor + group");
        assert_eq!(applied.warnings.len(), 1);
        let existing = load_existing(&conn).unwrap();
        let api = existing
            .iter()
            .find(|e| e.monitor.external_id.as_deref() == Some("api"))
            .unwrap();
        let group = existing
            .iter()
            .find(|e| e.monitor.monitor_type == MonitorType::Group)
            .unwrap();
        assert_eq!(api.monitor.parent_id, Some(group.monitor.id));
        let tags: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM monitor_tags WHERE monitor_id = ?1",
                params![api.monitor.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tags, 2);
        let links: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM monitor_notifications WHERE monitor_id = ?1",
                params![api.monitor.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(links, 1);

        // Second sync with the same container: nothing to do.
        let p = plan(&existing, std::slice::from_ref(&c), None);
        assert!(p.create.is_empty() && p.update.is_empty());
        assert!(apply_plan(&mut conn, &p).unwrap().reload.is_empty());

        // Container removed: paused and remembered; back: resumed.
        let p = plan(&load_existing(&conn).unwrap(), &[], None);
        assert_eq!(p.pause, vec![api.monitor.id]);
        apply_plan(&mut conn, &p).unwrap();
        let after = load_existing(&conn).unwrap();
        let api2 = after
            .iter()
            .find(|e| e.monitor.id == api.monitor.id)
            .unwrap();
        assert!(!api2.monitor.active && api2.paused_by_discovery);
        assert!(api2.monitor.description.contains("no longer exists"));

        let p = plan(&after, &[c], None);
        assert_eq!(p.resume.len(), 1);
        apply_plan(&mut conn, &p).unwrap();
        let back = load_existing(&conn).unwrap();
        let api3 = back
            .iter()
            .find(|e| e.monitor.id == api.monitor.id)
            .unwrap();
        assert!(api3.monitor.active && !api3.paused_by_discovery);
        assert_eq!(count_managed(&conn).unwrap(), 1);
    }

    #[test]
    fn docker_hosts_are_validated() {
        assert!(validate_docker_host("").is_ok());
        assert!(validate_docker_host("unix:///var/run/docker.sock").is_ok());
        assert!(validate_docker_host("tcp://proxy:2375").is_ok());
        assert!(validate_docker_host("https://x:2376").is_err());
        assert!(validate_docker_host("docker.local").is_err());
    }
}
