//! Prometheus metrics and the Integrations settings page. Owned by the
//! integrations agent.
//!
//! - `GET /metrics` (public route, API-token auth via `Authorization: Bearer`
//!   or as the basic-auth password; 404 unless the `metrics_enabled` setting
//!   is on). Text exposition format 0.0.4.
//! - `GET/POST /settings/integrations`, `POST /settings/integrations/sync` (admin).
//!
//! Besides the `vigilant_*` families this exports Uptime Kuma's metric names
//! (`monitor_status`, `monitor_response_time`, `monitor_cert_days_remaining`,
//! `monitor_cert_is_valid`) with Kuma's labels, so existing Grafana
//! dashboards built for Kuma work unchanged.

use crate::i18n::Tr as _;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use askama::Template;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::SignedCookieJar;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use super::agents::public_base_url;
use super::{ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::agent::protocol::parse_db;
use crate::auth::{CurrentUser, Role};
use crate::db::agents as db_agents;
use crate::db::models::{HeartbeatStatus, MonitorType};
use crate::db::monitors as db_monitors;
use crate::db::{rollups, settings as db_settings};
use crate::discovery;
use crate::monitor::{self, EngineMetrics};

/// Prometheus text exposition content type.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Protected routes (behind `require_auth`; `CurrentUser` is available).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/integrations", get(integrations_page).post(save_integrations))
        .route("/settings/integrations/sync", post(sync_now))
}

/// Unauthenticated routes.
pub fn public_router() -> Router<AppState> {
    Router::new().route("/metrics", get(metrics))
}

// ---------------------------------------------------------------------------
// Exposition format
// ---------------------------------------------------------------------------

/// Minimal Prometheus text-format writer. Each family's HELP/TYPE is written
/// once, before its first sample; samples of one family must be contiguous.
#[derive(Default)]
pub struct Exposition {
    out: String,
    seen: HashSet<String>,
}

impl Exposition {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a metric family (`kind`: counter | gauge | histogram).
    pub fn family(&mut self, name: &str, kind: &str, help: &str) -> &mut Self {
        if self.seen.insert(name.to_string()) {
            let _ = writeln!(self.out, "# HELP {name} {}", escape_help(help));
            let _ = writeln!(self.out, "# TYPE {name} {kind}");
        }
        self
    }

    /// One sample line.
    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) -> &mut Self {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (k, v)) in labels.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                let _ = write!(self.out, "{k}=\"{}\"", escape_label(v));
            }
            self.out.push('}');
        }
        self.out.push(' ');
        self.out.push_str(&format_value(value));
        self.out.push('\n');
        self
    }

    pub fn finish(self) -> String {
        self.out
    }
}

/// Label values escape `\`, `"` and newlines.
pub fn escape_label(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            c => s.push(c),
        }
    }
    s
}

/// HELP text escapes `\` and newlines.
pub fn escape_help(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

pub fn format_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf".into() } else { "-Inf".into() }
    } else if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

// ---------------------------------------------------------------------------
// Collected data → text
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MonitorSample {
    pub id: i64,
    pub name: String,
    pub monitor_type: MonitorType,
    pub target: String,
    pub active: bool,
    pub status: Option<HeartbeatStatus>,
    pub response_time_ms: Option<i64>,
    /// Percent (0–100).
    pub uptime_24h: Option<f64>,
    pub uptime_30d: Option<f64>,
    pub cert_days: Option<f64>,
    pub domain_days: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentSample {
    pub id: i64,
    pub name: String,
    pub location: String,
    pub active: bool,
    pub online: bool,
    pub last_seen_unix: Option<i64>,
    /// `(monitor_id, status, response_time_ms)` — latest within the last hour.
    pub results: Vec<(i64, HeartbeatStatus, Option<i64>)>,
}

#[derive(Debug, Clone, Default)]
pub struct MetricsInput {
    pub version: String,
    pub monitors: Vec<MonitorSample>,
    pub agents: Vec<AgentSample>,
    pub engine: EngineMetrics,
    pub discovery: discovery::DiscoveryStatus,
}

/// Uptime Kuma's numeric status: 0 down, 1 up, 2 pending, 3 maintenance.
pub fn kuma_status(s: HeartbeatStatus) -> f64 {
    match s {
        HeartbeatStatus::Down => 0.0,
        HeartbeatStatus::Up => 1.0,
        HeartbeatStatus::Pending => 2.0,
        HeartbeatStatus::Maintenance => 3.0,
    }
}

/// Drop `user:pass@` from URL-ish targets so credentials never reach labels.
pub fn redact_target(target: &str) -> String {
    if let Ok(mut url) = url::Url::parse(target)
        && (!url.username().is_empty() || url.password().is_some())
    {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        return url.to_string();
    }
    target.to_string()
}

/// Kuma's `monitor_url` / `monitor_hostname` / `monitor_port` labels.
fn kuma_target_labels(t: MonitorType, target: &str) -> (String, String, String) {
    match t {
        MonitorType::Http | MonitorType::Keyword | MonitorType::JsonQuery | MonitorType::Websocket => {
            (redact_target(target), String::new(), String::new())
        }
        MonitorType::Tcp | MonitorType::Steam | MonitorType::Ntp => match target.trim().rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (
                String::new(),
                h.trim_start_matches('[').trim_end_matches(']').to_string(),
                p.to_string(),
            ),
            _ => (String::new(), target.trim().to_string(), String::new()),
        },
        MonitorType::Ping | MonitorType::Dns | MonitorType::DomainExpiry => {
            (String::new(), target.trim().to_string(), String::new())
        }
        MonitorType::Docker | MonitorType::Push | MonitorType::Group => (String::new(), String::new(), String::new()),
    }
}

/// Render everything in Prometheus text format.
pub fn render(input: &MetricsInput) -> String {
    let mut e = Exposition::new();

    e.family("vigilant_build_info", "gauge", "Vigilant Vole build information.")
        .sample("vigilant_build_info", &[("version", &input.version)], 1.0);

    // -- per monitor, Vigilant names -------------------------------------
    let monitors: Vec<_> = input.monitors.iter().collect();
    let ids: Vec<String> = monitors.iter().map(|m| m.id.to_string()).collect();
    let targets: Vec<String> = monitors.iter().map(|m| redact_target(&m.target)).collect();
    let base = |i: usize| -> [(&str, &str); 4] {
        let m = monitors[i];
        [
            ("monitor_id", ids[i].as_str()),
            ("monitor_name", m.name.as_str()),
            ("monitor_type", m.monitor_type.as_str()),
            ("monitor_target", targets[i].as_str()),
        ]
    };

    e.family(
        "vigilant_monitor_active",
        "gauge",
        "1 if the monitor is active, 0 if paused.",
    );
    for (i, m) in monitors.iter().enumerate() {
        e.sample("vigilant_monitor_active", &base(i), f64::from(u8::from(m.active)));
    }

    e.family(
        "vigilant_monitor_status",
        "gauge",
        "Monitor status: 0 down, 1 up, 2 pending, 3 maintenance (active monitors with data).",
    );
    for (i, m) in monitors.iter().enumerate() {
        if let (true, Some(s)) = (m.active, m.status) {
            e.sample("vigilant_monitor_status", &base(i), kuma_status(s));
        }
    }

    e.family(
        "vigilant_monitor_response_time_seconds",
        "gauge",
        "Response time of the latest local check, in seconds.",
    );
    for (i, m) in monitors.iter().enumerate() {
        if let (true, Some(ms)) = (m.active, m.response_time_ms) {
            e.sample("vigilant_monitor_response_time_seconds", &base(i), ms as f64 / 1000.0);
        }
    }

    e.family(
        "vigilant_monitor_uptime_ratio",
        "gauge",
        "Share of up checks (0-1) over a trailing window; pending and maintenance excluded.",
    );
    for (i, m) in monitors.iter().enumerate() {
        for (window, value) in [("24h", m.uptime_24h), ("30d", m.uptime_30d)] {
            if let Some(pct) = value {
                let b = base(i);
                let labels = [b[0], b[1], b[2], b[3], ("window", window)];
                e.sample("vigilant_monitor_uptime_ratio", &labels, pct / 100.0);
            }
        }
    }

    e.family(
        "vigilant_monitor_cert_days_remaining",
        "gauge",
        "Days until the monitor's TLS certificate expires.",
    );
    for (i, m) in monitors.iter().enumerate() {
        if let Some(d) = m.cert_days {
            e.sample("vigilant_monitor_cert_days_remaining", &base(i), d.floor());
        }
    }
    e.family(
        "vigilant_monitor_domain_days_remaining",
        "gauge",
        "Days until the monitor's domain registration expires.",
    );
    for (i, m) in monitors.iter().enumerate() {
        if let Some(d) = m.domain_days {
            e.sample("vigilant_monitor_domain_days_remaining", &base(i), d.floor());
        }
    }

    // -- per monitor, Uptime Kuma compatible ------------------------------
    let kuma: Vec<(String, String, String)> = monitors
        .iter()
        .map(|m| kuma_target_labels(m.monitor_type, &m.target))
        .collect();
    let kuma_labels = |i: usize| -> [(&str, &str); 6] {
        let m = monitors[i];
        [
            ("monitor_id", ids[i].as_str()),
            ("monitor_name", m.name.as_str()),
            ("monitor_type", m.monitor_type.as_str()),
            ("monitor_url", kuma[i].0.as_str()),
            ("monitor_hostname", kuma[i].1.as_str()),
            ("monitor_port", kuma[i].2.as_str()),
        ]
    };
    e.family(
        "monitor_status",
        "gauge",
        "Monitor Status (1 = UP, 0= DOWN, 2= PENDING, 3= MAINTENANCE)",
    );
    for (i, m) in monitors.iter().enumerate() {
        if let (true, Some(s)) = (m.active, m.status) {
            e.sample("monitor_status", &kuma_labels(i), kuma_status(s));
        }
    }
    e.family("monitor_response_time", "gauge", "Monitor Response Time (ms)");
    for (i, m) in monitors.iter().enumerate() {
        if let (true, Some(ms)) = (m.active, m.response_time_ms) {
            e.sample("monitor_response_time", &kuma_labels(i), ms as f64);
        }
    }
    e.family(
        "monitor_cert_days_remaining",
        "gauge",
        "The number of days remaining until the certificate expires",
    );
    for (i, m) in monitors.iter().enumerate() {
        if let (true, Some(d)) = (m.active, m.cert_days) {
            e.sample("monitor_cert_days_remaining", &kuma_labels(i), d.floor());
        }
    }
    e.family(
        "monitor_cert_is_valid",
        "gauge",
        "Is the certificate still valid? (1 = Yes, 0= No)",
    );
    for (i, m) in monitors.iter().enumerate() {
        if let (true, Some(d)) = (m.active, m.cert_days) {
            e.sample("monitor_cert_is_valid", &kuma_labels(i), if d > 0.0 { 1.0 } else { 0.0 });
        }
    }

    // -- engine ---------------------------------------------------------------
    let eng = &input.engine;
    e.family(
        "vigilant_checks_total",
        "counter",
        "Heartbeats recorded by the engine since start.",
    )
    .sample("vigilant_checks_total", &[], eng.checks_total as f64);
    e.family(
        "vigilant_checks_failed_total",
        "counter",
        "Recorded heartbeats that were down or pending since start.",
    )
    .sample("vigilant_checks_failed_total", &[], eng.checks_failed as f64);
    e.family(
        "vigilant_running_tasks",
        "gauge",
        "Monitors with a running check loop or push watchdog.",
    )
    .sample("vigilant_running_tasks", &[], eng.running_tasks as f64);

    let monitor_ids: Vec<String> = eng.monitors.iter().map(|m| m.monitor_id.to_string()).collect();
    e.family(
        "vigilant_monitor_checks_total",
        "counter",
        "Heartbeats recorded per monitor since start.",
    );
    for (m, id) in eng.monitors.iter().zip(&monitor_ids) {
        e.sample("vigilant_monitor_checks_total", &[("monitor_id", id)], m.checks_total as f64);
    }
    e.family(
        "vigilant_monitor_checks_failed_total",
        "counter",
        "Down or pending heartbeats per monitor since start.",
    );
    for (m, id) in eng.monitors.iter().zip(&monitor_ids) {
        e.sample("vigilant_monitor_checks_failed_total", &[("monitor_id", id)], m.checks_failed as f64);
    }

    e.family(
        "vigilant_check_duration_seconds",
        "histogram",
        "Wall-clock duration of local probes by monitor type.",
    );
    for h in &eng.check_duration {
        for (bound, count) in &h.buckets {
            let le = format_value(*bound);
            e.sample(
                "vigilant_check_duration_seconds_bucket",
                &[("monitor_type", &h.monitor_type), ("le", &le)],
                *count as f64,
            );
        }
        e.sample(
            "vigilant_check_duration_seconds_bucket",
            &[("monitor_type", &h.monitor_type), ("le", "+Inf")],
            h.count as f64,
        );
        e.sample(
            "vigilant_check_duration_seconds_sum",
            &[("monitor_type", &h.monitor_type)],
            h.sum_seconds,
        );
        e.sample(
            "vigilant_check_duration_seconds_count",
            &[("monitor_type", &h.monitor_type)],
            h.count as f64,
        );
    }

    e.family(
        "vigilant_notifications_sent_total",
        "counter",
        "Notifications delivered by channel kind since start.",
    );
    for n in &eng.notifications {
        e.sample("vigilant_notifications_sent_total", &[("kind", &n.kind)], n.sent as f64);
    }
    e.family(
        "vigilant_notifications_failed_total",
        "counter",
        "Notification deliveries that failed by channel kind since start.",
    );
    for n in &eng.notifications {
        e.sample("vigilant_notifications_failed_total", &[("kind", &n.kind)], n.failed as f64);
    }

    // -- agents -----------------------------------------------------------------
    let agent_ids: Vec<String> = input.agents.iter().map(|a| a.id.to_string()).collect();
    let agent_labels = |i: usize| -> [(&str, &str); 3] {
        let a = &input.agents[i];
        [
            ("agent_id", agent_ids[i].as_str()),
            ("agent_name", a.name.as_str()),
            ("agent_location", a.location.as_str()),
        ]
    };
    e.family(
        "vigilant_agent_up",
        "gauge",
        "1 if the remote agent is enabled and checked in within 90 seconds.",
    );
    for (i, a) in input.agents.iter().enumerate() {
        e.sample("vigilant_agent_up", &agent_labels(i), f64::from(u8::from(a.online)));
    }
    e.family(
        "vigilant_agent_last_seen_timestamp_seconds",
        "gauge",
        "Unix time the remote agent last checked in.",
    );
    for (i, a) in input.agents.iter().enumerate() {
        if let Some(ts) = a.last_seen_unix {
            e.sample("vigilant_agent_last_seen_timestamp_seconds", &agent_labels(i), ts as f64);
        }
    }
    e.family(
        "vigilant_agent_monitor_status",
        "gauge",
        "Latest status a remote agent reported for a monitor (0 down, 1 up, 2 pending), last hour.",
    );
    for (i, a) in input.agents.iter().enumerate() {
        for (monitor_id, status, _) in &a.results {
            let mid = monitor_id.to_string();
            let l = agent_labels(i);
            e.sample(
                "vigilant_agent_monitor_status",
                &[l[0], l[1], l[2], ("monitor_id", &mid)],
                kuma_status(*status),
            );
        }
    }
    e.family(
        "vigilant_agent_monitor_response_time_seconds",
        "gauge",
        "Latest response time a remote agent reported for a monitor, last hour.",
    );
    for (i, a) in input.agents.iter().enumerate() {
        for (monitor_id, _, ms) in &a.results {
            if let Some(ms) = ms {
                let mid = monitor_id.to_string();
                let l = agent_labels(i);
                e.sample(
                    "vigilant_agent_monitor_response_time_seconds",
                    &[l[0], l[1], l[2], ("monitor_id", &mid)],
                    *ms as f64 / 1000.0,
                );
            }
        }
    }

    // -- discovery --------------------------------------------------------------
    let d = &input.discovery;
    e.family(
        "vigilant_discovery_monitors",
        "gauge",
        "Monitors managed by Docker label discovery.",
    )
    .sample("vigilant_discovery_monitors", &[], d.managed as f64);
    if let Some(ts) = d.last_sync_unix {
        e.family(
            "vigilant_discovery_last_sync_timestamp_seconds",
            "gauge",
            "Unix time of the last Docker discovery sync attempt.",
        )
        .sample("vigilant_discovery_last_sync_timestamp_seconds", &[], ts as f64);
        e.family(
            "vigilant_discovery_last_sync_success",
            "gauge",
            "1 if the last Docker discovery sync succeeded.",
        )
        .sample(
            "vigilant_discovery_last_sync_success",
            &[],
            if d.last_error.is_none() { 1.0 } else { 0.0 },
        );
    }

    e.finish()
}

// ---------------------------------------------------------------------------
// /metrics handler
// ---------------------------------------------------------------------------

fn unauthorized() -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, "Unauthorized: send an API token as a Bearer token or basic-auth password\n")
        .into_response();
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Vigilant Vole metrics\""),
    );
    r
}

async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let settings = state.settings().await?;
    if !settings.metrics_enabled {
        return Ok((StatusCode::NOT_FOUND, "Prometheus metrics are turned off (Settings → Integrations)\n").into_response());
    }
    // Any valid API token (read scope is enough), as Bearer or basic-auth password.
    if super::auth::authenticate_headers(&state, &headers).await?.is_none() {
        return Ok(unauthorized());
    }
    let input = collect(&state).await?;
    let body = render(&input);
    Ok(([(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response())
}

struct DbSnapshot {
    monitors: Vec<MonitorSample>,
    agents: Vec<AgentSample>,
}

fn load_snapshot(conn: &mut Connection) -> rusqlite::Result<DbSnapshot> {
    let all = db_monitors::list_monitors(conn)?;
    let mut expiry: HashMap<(i64, String), f64> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT monitor_id, kind, julianday(expires_at) - julianday('now') FROM monitor_expiry",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<f64>>(2)?))
        })?;
        for row in rows {
            let (id, kind, days) = row?;
            if let Some(days) = days {
                expiry.insert((id, kind), days);
            }
        }
    }
    let mut latest = conn.prepare(
        "SELECT status, response_time_ms FROM heartbeats
         WHERE monitor_id = ?1 AND agent_id IS NULL ORDER BY checked_at DESC, id DESC LIMIT 1",
    )?;
    let mut monitors = Vec::with_capacity(all.len());
    for m in all {
        let last: Option<(String, Option<i64>)> = latest
            .query_row(params![m.id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let (uptime_24h, uptime_30d) = if m.active {
            (rollups::uptime_pct(conn, m.id, 1)?, rollups::uptime_pct(conn, m.id, 30)?)
        } else {
            (None, None)
        };
        monitors.push(MonitorSample {
            id: m.id,
            status: last.as_ref().and_then(|(s, _)| HeartbeatStatus::parse(s)),
            response_time_ms: last.and_then(|(_, ms)| ms),
            uptime_24h,
            uptime_30d,
            cert_days: expiry.get(&(m.id, "cert".to_string())).copied(),
            domain_days: expiry.get(&(m.id, "domain".to_string())).copied(),
            name: m.name,
            monitor_type: m.monitor_type,
            target: m.target,
            active: m.active,
        });
    }
    drop(latest);

    let results = db_agents::latest_agent_results(conn)?;
    let agents = db_agents::list_agents(conn)?
        .into_iter()
        .map(|s| AgentSample {
            id: s.agent.id,
            results: results
                .iter()
                .filter(|r| r.agent_id == s.agent.id)
                .filter_map(|r| HeartbeatStatus::parse(&r.status).map(|st| (r.monitor_id, st, r.response_time_ms)))
                .collect(),
            last_seen_unix: s
                .agent
                .last_seen_at
                .as_deref()
                .and_then(parse_db)
                .map(|t| t.unix_timestamp()),
            name: s.agent.name,
            location: s.agent.location,
            active: s.agent.active,
            online: s.online,
        })
        .collect();
    Ok(DbSnapshot { monitors, agents })
}

async fn collect(state: &AppState) -> ApiResult<MetricsInput> {
    let DbSnapshot { mut monitors, agents } = state.with_conn(load_snapshot).await?;
    // Prefer the engine's live status (it knows retries/maintenance) over the
    // latest heartbeat row.
    for m in monitors.iter_mut().filter(|m| m.active) {
        if let Some(snap) = state.scheduler.status_snapshot(m.id).await
            && let Some(status) = snap.status
        {
            m.status = Some(status);
        }
    }
    Ok(MetricsInput {
        version: env!("CARGO_PKG_VERSION").to_string(),
        monitors,
        agents,
        engine: monitor::metrics_snapshot(),
        discovery: discovery::status(),
    })
}

// ---------------------------------------------------------------------------
// Integrations settings page
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings_integrations.html")]
struct IntegrationsTemplate {
    ctx: PageCtx,
    discovery_enabled: bool,
    docker_host: String,
    metrics_enabled: bool,
    discovery: discovery::DiscoveryStatus,
    last_sync_at: String,
    metrics_url: String,
    scrape_config: String,
    compose_example: String,
    labels: Vec<(&'static str, &'static str, &'static str)>,
}

#[derive(Debug, Default, Deserialize)]
struct IntegrationsForm {
    #[serde(default)]
    discovery_enabled: bool,
    #[serde(default)]
    discovery_docker_host: String,
    #[serde(default)]
    metrics_enabled: bool,
}

/// Prometheus scrape job for this server.
pub fn scrape_config(base_url: &str) -> String {
    let (scheme, host) = match base_url.split_once("://") {
        Some((s, h)) => (s, h),
        None => ("http", base_url),
    };
    let (host, path_prefix) = match host.split_once('/') {
        Some((h, p)) => (h, format!("/{}", p.trim_end_matches('/'))),
        None => (host, String::new()),
    };
    format!(
        "scrape_configs:\n  - job_name: vigilant-vole\n    scheme: {scheme}\n    metrics_path: {path_prefix}/metrics\n    authorization:\n      type: Bearer\n      credentials: vv_YOUR_API_TOKEN\n    static_configs:\n      - targets: [\"{host}\"]\n"
    )
}

async fn integrations_page(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
) -> ApiResult<(SignedCookieJar, Response)> {
    user.require(Role::Admin)?;
    let settings = state.settings().await?;
    let base = public_base_url(&settings, &headers);
    let status = discovery::status();
    let (ctx, jar) = PageCtx::build(&state, Some(&user), &jar, crate::i18n::t("nav.integrations"), "settings", "integrations").await;
    let page = IntegrationsTemplate {
        ctx,
        discovery_enabled: settings.discovery_enabled,
        docker_host: settings.discovery_docker_host.clone().unwrap_or_default(),
        metrics_enabled: settings.metrics_enabled,
        last_sync_at: status.last_sync_at.clone().unwrap_or_default(),
        discovery: status,
        metrics_url: format!("{base}/metrics"),
        scrape_config: scrape_config(&base),
        compose_example: discovery::COMPOSE_EXAMPLE.to_string(),
        labels: discovery::LABEL_REFERENCE
            .iter()
            .map(|(name, example, help)| {
                let key = format!("integ.label_{}", name.trim_start_matches("vigilant."));
                (*name, *example, crate::i18n::t_or(&key, help))
            })
            .collect(),
    };
    Ok((jar, HtmlTemplate(page).into_response()))
}

async fn save_integrations(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<IntegrationsForm>,
) -> ApiResult<(SignedCookieJar, Redirect)> {
    user.require(Role::Admin)?;
    let host = form.discovery_docker_host.trim().to_string();
    if let Err(msg) = discovery::validate_docker_host(&host) {
        return Ok((flash(jar, "error", msg), Redirect::to("/settings/integrations")));
    }
    let (discovery_enabled, metrics_enabled) = (form.discovery_enabled, form.metrics_enabled);
    state
        .with_conn(move |c| {
            let tx = c.transaction()?;
            db_settings::set_bool(&tx, db_settings::DISCOVERY_ENABLED, discovery_enabled)?;
            db_settings::set_opt(
                &tx,
                db_settings::DISCOVERY_DOCKER_HOST,
                Some(host.as_str()).filter(|h| !h.is_empty()),
            )?;
            db_settings::set_bool(&tx, db_settings::METRICS_ENABLED, metrics_enabled)?;
            tx.commit()
        })
        .await?;
    tracing::info!(discovery_enabled, metrics_enabled, user = %user.username, "integration settings saved");
    discovery::trigger();
    Ok((flash(jar, "success", crate::i18n::t("integ.saved")), Redirect::to("/settings/integrations")))
}

async fn sync_now(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<(SignedCookieJar, Redirect)> {
    user.require(Role::Admin)?;
    let settings = state.settings().await?;
    if !settings.discovery_enabled {
        return Ok((
            flash(jar, "error", crate::i18n::t("integ.turn_on_first")),
            Redirect::to("/settings/integrations"),
        ));
    }
    let message = match discovery::sync_once(&state).await {
        Ok(summary) => flash(jar, "success", crate::i18n::tf("integ.synced", &[("summary", &summary)])),
        Err(err) => flash(jar, "error", crate::i18n::tf("integ.sync_failed", &[("err", &err.to_string())])),
    };
    Ok((message, Redirect::to("/settings/integrations")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{DurationHistogram, MonitorMetrics, NotificationMetrics};

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(escape_label("line1\nline2"), "line1\\nline2");
        assert_eq!(escape_help("x\\y\nz"), "x\\\\y\\nz");
    }

    #[test]
    fn values_are_formatted_for_prometheus() {
        assert_eq!(format_value(1.0), "1");
        assert_eq!(format_value(0.25), "0.25");
        assert_eq!(format_value(f64::INFINITY), "+Inf");
        assert_eq!(format_value(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_value(f64::NAN), "NaN");
    }

    #[test]
    fn families_get_help_and_type_once() {
        let mut e = Exposition::new();
        e.family("x_total", "counter", "Things.");
        e.sample("x_total", &[("a", "1")], 2.0);
        e.family("x_total", "counter", "Things.");
        e.sample("x_total", &[("a", "2")], 3.0);
        assert_eq!(
            e.finish(),
            "# HELP x_total Things.\n# TYPE x_total counter\nx_total{a=\"1\"} 2\nx_total{a=\"2\"} 3\n"
        );
    }

    #[test]
    fn credentials_are_removed_from_targets() {
        assert_eq!(redact_target("https://bob:secret@example.com/x"), "https://example.com/x");
        assert_eq!(redact_target("example.com:443"), "example.com:443");
    }

    fn sample_input() -> MetricsInput {
        MetricsInput {
            version: "1.2.3".into(),
            monitors: vec![
                MonitorSample {
                    id: 7,
                    name: "Web \"main\"".into(),
                    monitor_type: MonitorType::Http,
                    target: "https://example.com".into(),
                    active: true,
                    status: Some(HeartbeatStatus::Up),
                    response_time_ms: Some(120),
                    uptime_24h: Some(99.5),
                    uptime_30d: None,
                    cert_days: Some(12.7),
                    domain_days: None,
                },
                MonitorSample {
                    id: 8,
                    name: "db".into(),
                    monitor_type: MonitorType::Tcp,
                    target: "db.local:5432".into(),
                    active: true,
                    status: Some(HeartbeatStatus::Maintenance),
                    ..MonitorSample::default()
                },
                MonitorSample {
                    id: 9,
                    name: "paused".into(),
                    monitor_type: MonitorType::Ping,
                    target: "1.1.1.1".into(),
                    active: false,
                    status: Some(HeartbeatStatus::Down),
                    ..MonitorSample::default()
                },
            ],
            agents: vec![AgentSample {
                id: 1,
                name: "fra".into(),
                location: "eu".into(),
                active: true,
                online: true,
                last_seen_unix: Some(1_700_000_000),
                results: vec![(7, HeartbeatStatus::Down, Some(1500))],
            }],
            engine: EngineMetrics {
                checks_total: 10,
                checks_failed: 2,
                running_tasks: 2,
                monitors: vec![MonitorMetrics {
                    monitor_id: 7,
                    checks_total: 10,
                    checks_failed: 2,
                    last_latency_ms: Some(120),
                    last_status: Some(HeartbeatStatus::Up),
                }],
                check_duration: vec![DurationHistogram {
                    monitor_type: "http".into(),
                    buckets: vec![(0.1, 1), (1.0, 3)],
                    sum_seconds: 1.5,
                    count: 4,
                }],
                notifications: vec![NotificationMetrics {
                    kind: "discord".into(),
                    sent: 3,
                    failed: 1,
                }],
            },
            discovery: discovery::DiscoveryStatus::default(),
        }
    }

    #[test]
    fn render_exports_vigilant_and_kuma_names() {
        let text = render(&sample_input());
        let has = |line: &str| text.lines().any(|l| l == line);
        assert!(has("vigilant_build_info{version=\"1.2.3\"} 1"));
        assert!(has(
            "vigilant_monitor_status{monitor_id=\"7\",monitor_name=\"Web \\\"main\\\"\",monitor_type=\"http\",monitor_target=\"https://example.com\"} 1"
        ));
        assert!(has(
            "monitor_status{monitor_id=\"8\",monitor_name=\"db\",monitor_type=\"tcp\",monitor_url=\"\",monitor_hostname=\"db.local\",monitor_port=\"5432\"} 3"
        ));
        assert!(has(
            "monitor_response_time{monitor_id=\"7\",monitor_name=\"Web \\\"main\\\"\",monitor_type=\"http\",monitor_url=\"https://example.com/\",monitor_hostname=\"\",monitor_port=\"\"} 120"
        ) || has(
            "monitor_response_time{monitor_id=\"7\",monitor_name=\"Web \\\"main\\\"\",monitor_type=\"http\",monitor_url=\"https://example.com\",monitor_hostname=\"\",monitor_port=\"\"} 120"
        ));
        assert!(text.contains("vigilant_monitor_response_time_seconds{monitor_id=\"7\""));
        assert!(text.contains("monitor_cert_days_remaining{monitor_id=\"7\""));
        assert!(text.contains("} 12\n"));
        assert!(text.contains("window=\"24h\"} 0.995"));
        assert!(has("vigilant_check_duration_seconds_bucket{monitor_type=\"http\",le=\"+Inf\"} 4"));
        assert!(has("vigilant_check_duration_seconds_bucket{monitor_type=\"http\",le=\"0.1\"} 1"));
        assert!(has("vigilant_check_duration_seconds_sum{monitor_type=\"http\"} 1.5"));
        assert!(has("vigilant_notifications_failed_total{kind=\"discord\"} 1"));
        assert!(has("vigilant_agent_up{agent_id=\"1\",agent_name=\"fra\",agent_location=\"eu\"} 1"));
        assert!(has(
            "vigilant_agent_monitor_status{agent_id=\"1\",agent_name=\"fra\",agent_location=\"eu\",monitor_id=\"7\"} 0"
        ));
        // Paused monitors: no status, but an active=0 series.
        assert!(!text.contains("monitor_status{monitor_id=\"9\""));
        assert!(text.contains("vigilant_monitor_active{monitor_id=\"9\",monitor_name=\"paused\",monitor_type=\"ping\",monitor_target=\"1.1.1.1\"} 0"));
    }

    #[test]
    fn every_family_has_help_and_type_before_samples() {
        let text = render(&sample_input());
        let mut declared = HashSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let name = rest.split(' ').next().unwrap();
                assert!(declared.insert(name.to_string()), "duplicate TYPE for {name}");
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            let name = line.split(['{', ' ']).next().unwrap();
            let family = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|s| name.strip_suffix(s).filter(|f| declared.contains(*f)))
                .unwrap_or(name);
            assert!(declared.contains(family), "sample before TYPE: {line}");
        }
    }

    #[tokio::test]
    async fn metrics_endpoint_is_gated_and_token_protected() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt;

        use crate::auth::tokens;
        use crate::db::api_tokens::{self as db_tokens, TokenScope};
        use crate::db::models::{NewMonitor, Role};

        let state = super::super::agents::test_support::test_state().await;
        let token = tokens::generate();
        let hash = tokens::hash(&token);
        state
            .with_conn(move |c| {
                let uid = crate::db::users::insert_user(c, "prom", "x", Role::Viewer)?;
                db_tokens::insert(c, uid, "prom", "vv_", &hash, TokenScope::Read, None)?;
                db_monitors::insert_monitor(
                    c,
                    &NewMonitor {
                        name: "Site".into(),
                        target: "https://example.com".into(),
                        ..NewMonitor::default()
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let call = |auth: Option<String>| {
            let mut req = Request::builder().uri("/metrics");
            if let Some(a) = auth {
                req = req.header("authorization", a);
            }
            public_router()
                .with_state(state.clone())
                .oneshot(req.body(Body::empty()).unwrap())
        };

        // Off by default.
        let resp = call(Some(format!("Bearer {token}"))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        state
            .with_conn(|c| db_settings::set_bool(c, db_settings::METRICS_ENABLED, true))
            .await
            .unwrap();
        assert_eq!(call(None).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let bad = format!("Bearer {}", tokens::generate());
        assert_eq!(call(Some(bad)).await.unwrap().status(), StatusCode::UNAUTHORIZED);

        let resp = call(Some(format!("Bearer {token}"))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], CONTENT_TYPE);
        let body = String::from_utf8(to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("vigilant_build_info{version="));
        assert!(body.contains("vigilant_monitor_active{monitor_id=\"1\",monitor_name=\"Site\""));

        // Basic auth with the token as password (Prometheus basic_auth).
        use base64::Engine as _;
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("prometheus:{token}"));
        let resp = call(Some(format!("Basic {basic}"))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn scrape_config_splits_url() {
        let cfg = scrape_config("https://status.example.com/vv");
        assert!(cfg.contains("scheme: https"));
        assert!(cfg.contains("metrics_path: /vv/metrics"));
        assert!(cfg.contains("targets: [\"status.example.com\"]"));
    }
}
