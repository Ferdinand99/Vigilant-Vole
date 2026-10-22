//! Shared domain types. Owned by the foundation agent: every feature module
//! builds on these, so changes here are contract changes.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Monitor types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorType {
    #[default]
    Http,
    Keyword,
    JsonQuery,
    Tcp,
    Ping,
    /// A Tailscale-level ping through the local tailscaled (LocalAPI).
    TailscalePing,
    Dns,
    Push,
    Docker,
    Websocket,
    DomainExpiry,
    Ntp,
    Steam,
    /// Minecraft (Java, Bedrock), Satisfactory, FiveM/RedM or Mumble status query.
    GameServer,
    Tls,
    Smtp,
    Redis,
    Postgres,
    Mysql,
    Mqtt,
    Kafka,
    Rabbitmq,
    Grpc,
    /// Any unary gRPC method, keyword matched in its JSON-rendered answer.
    GrpcKeyword,
    Radius,
    Snmp,
    /// `sqlserver` (the serde name matches [`MonitorType::as_str`]).
    #[serde(rename = "sqlserver")]
    SqlServer,
    Mongodb,
    /// A page load in a real (remote, headless) Chromium over CDP.
    RealBrowser,
    Group,
}

impl MonitorType {
    /// A slice, not an array: adding a type needs no count bump.
    const ALL: &[MonitorType] = &[
        MonitorType::Http,
        MonitorType::Keyword,
        MonitorType::JsonQuery,
        MonitorType::Tcp,
        MonitorType::Ping,
        MonitorType::TailscalePing,
        MonitorType::Dns,
        MonitorType::Push,
        MonitorType::Docker,
        MonitorType::Websocket,
        MonitorType::DomainExpiry,
        MonitorType::Ntp,
        MonitorType::Steam,
        MonitorType::GameServer,
        MonitorType::Tls,
        MonitorType::Smtp,
        MonitorType::Redis,
        MonitorType::Postgres,
        MonitorType::Mysql,
        MonitorType::Mqtt,
        MonitorType::Kafka,
        MonitorType::Rabbitmq,
        MonitorType::Grpc,
        MonitorType::GrpcKeyword,
        MonitorType::Radius,
        MonitorType::Snmp,
        MonitorType::SqlServer,
        MonitorType::Mongodb,
        MonitorType::RealBrowser,
        MonitorType::Group,
    ];

    /// Every monitor type, in the order they should appear in the type picker.
    pub fn all() -> &'static [MonitorType] {
        Self::ALL
    }

    /// The value stored in `monitors.monitor_type`.
    pub fn as_str(self) -> &'static str {
        match self {
            MonitorType::Http => "http",
            MonitorType::Keyword => "keyword",
            MonitorType::JsonQuery => "json_query",
            MonitorType::Tcp => "tcp",
            MonitorType::Ping => "ping",
            MonitorType::TailscalePing => "tailscale_ping",
            MonitorType::Dns => "dns",
            MonitorType::Push => "push",
            MonitorType::Docker => "docker",
            MonitorType::Websocket => "websocket",
            MonitorType::DomainExpiry => "domain_expiry",
            MonitorType::Ntp => "ntp",
            MonitorType::Steam => "steam",
            MonitorType::GameServer => "game_server",
            MonitorType::Tls => "tls",
            MonitorType::Smtp => "smtp",
            MonitorType::Redis => "redis",
            MonitorType::Postgres => "postgres",
            MonitorType::Mysql => "mysql",
            MonitorType::Mqtt => "mqtt",
            MonitorType::Kafka => "kafka",
            MonitorType::Rabbitmq => "rabbitmq",
            MonitorType::Grpc => "grpc",
            MonitorType::GrpcKeyword => "grpc_keyword",
            MonitorType::Radius => "radius",
            MonitorType::Snmp => "snmp",
            MonitorType::SqlServer => "sqlserver",
            MonitorType::Mongodb => "mongodb",
            MonitorType::RealBrowser => "real_browser",
            MonitorType::Group => "group",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == s)
    }

    /// Human-readable name for forms and lists (in the current language).
    pub fn label(self) -> &'static str {
        match self {
            MonitorType::Http => crate::i18n::t("monitor_type.http"),
            MonitorType::Keyword => crate::i18n::t("monitor_type.keyword"),
            MonitorType::JsonQuery => crate::i18n::t("monitor_type.json_query"),
            MonitorType::Tcp => crate::i18n::t("monitor_type.tcp"),
            MonitorType::Ping => crate::i18n::t("monitor_type.ping"),
            MonitorType::TailscalePing => crate::i18n::t("monitor_type.tailscale_ping"),
            MonitorType::Dns => crate::i18n::t("monitor_type.dns"),
            MonitorType::Push => crate::i18n::t("monitor_type.push"),
            MonitorType::Docker => crate::i18n::t("monitor_type.docker"),
            MonitorType::Websocket => crate::i18n::t("monitor_type.websocket"),
            MonitorType::DomainExpiry => crate::i18n::t("monitor_type.domain_expiry"),
            MonitorType::Ntp => crate::i18n::t("monitor_type.ntp"),
            MonitorType::Steam => crate::i18n::t("monitor_type.steam"),
            MonitorType::GameServer => crate::i18n::t("monitor_type.game_server"),
            MonitorType::Tls => crate::i18n::t("monitor_type.tls"),
            MonitorType::Smtp => crate::i18n::t("monitor_type.smtp"),
            MonitorType::Redis => crate::i18n::t("monitor_type.redis"),
            MonitorType::Postgres => crate::i18n::t("monitor_type.postgres"),
            MonitorType::Mysql => crate::i18n::t("monitor_type.mysql"),
            MonitorType::Mqtt => crate::i18n::t("monitor_type.mqtt"),
            MonitorType::Kafka => crate::i18n::t("monitor_type.kafka"),
            MonitorType::Rabbitmq => crate::i18n::t("monitor_type.rabbitmq"),
            MonitorType::Grpc => crate::i18n::t("monitor_type.grpc"),
            MonitorType::GrpcKeyword => crate::i18n::t("monitor_type.grpc_keyword"),
            MonitorType::Radius => crate::i18n::t("monitor_type.radius"),
            MonitorType::Snmp => crate::i18n::t("monitor_type.snmp"),
            MonitorType::SqlServer => crate::i18n::t("monitor_type.sqlserver"),
            MonitorType::Mongodb => crate::i18n::t("monitor_type.mongodb"),
            MonitorType::RealBrowser => crate::i18n::t("monitor_type.real_browser"),
            MonitorType::Group => crate::i18n::t("monitor_type.group"),
        }
    }

    /// True for the HTTP family (http, keyword, json_query), which share the
    /// request options (method, headers, body, auth, TLS, redirects, proxy).
    pub fn is_http_family(self) -> bool {
        matches!(
            self,
            MonitorType::Http | MonitorType::Keyword | MonitorType::JsonQuery
        )
    }

    /// Types the scheduler actively probes. Push monitors are fed externally
    /// and group monitors derive their status from children.
    pub fn is_active_check(self) -> bool {
        !matches!(self, MonitorType::Push | MonitorType::Group)
    }

    /// Types remote probe agents can run. Real-browser checks drive the
    /// server's configured remote browsers, which agents don't have.
    pub fn runs_on_agents(self) -> bool {
        self.is_active_check() && self != MonitorType::RealBrowser
    }
}

impl fmt::Display for MonitorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Heartbeats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatStatus {
    Up,
    Down,
    Pending,
    Maintenance,
}

impl HeartbeatStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HeartbeatStatus::Up => "up",
            HeartbeatStatus::Down => "down",
            HeartbeatStatus::Pending => "pending",
            HeartbeatStatus::Maintenance => "maintenance",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "up" => Some(HeartbeatStatus::Up),
            "down" => Some(HeartbeatStatus::Down),
            "pending" => Some(HeartbeatStatus::Pending),
            "maintenance" => Some(HeartbeatStatus::Maintenance),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HeartbeatStatus::Up => crate::i18n::t("status.up"),
            HeartbeatStatus::Down => crate::i18n::t("status.down"),
            HeartbeatStatus::Pending => crate::i18n::t("status.pending"),
            HeartbeatStatus::Maintenance => crate::i18n::t("status.maintenance"),
        }
    }
}

impl fmt::Display for HeartbeatStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A row of `heartbeats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub id: i64,
    pub monitor_id: i64,
    /// `None` = checked locally by this server; `Some` = reported by a remote agent.
    pub agent_id: Option<i64>,
    pub status: HeartbeatStatus,
    pub response_time_ms: Option<i64>,
    pub message: Option<String>,
    /// UTC, `YYYY-MM-DD HH:MM:SS`.
    pub checked_at: String,
}

// ---------------------------------------------------------------------------
// Monitors
// ---------------------------------------------------------------------------

/// A row of `monitors` — every column. Optional text columns are `None` when
/// NULL. Enum-like text columns that are only interpreted by one feature
/// (`http_method`, `http_body_encoding`, `json_operator`, `dns_record_type`,
/// `source`) are kept as plain strings; the owning module validates them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub monitor_type: MonitorType,
    /// URL, `host:port`, hostname, container name, … depending on type.
    /// Empty for `push` and `group`.
    pub target: String,
    /// Must reference a monitor of type `group` (validated in the app layer).
    pub parent_id: Option<i64>,
    pub interval_seconds: i64,
    /// Interval while the monitor is failing/pending. `None` = same as `interval_seconds`.
    pub retry_interval_seconds: Option<i64>,
    pub timeout_seconds: i64,
    pub retries: i64,
    pub active: bool,
    pub upside_down: bool,
    // -- HTTP family --
    pub http_method: String,
    /// JSON object text, e.g. `{"Authorization": "Bearer x"}`.
    pub http_headers: Option<String>,
    pub http_body: Option<String>,
    /// `json` | `form` | `raw`.
    pub http_body_encoding: String,
    /// Comma-separated codes/ranges, e.g. `200-299,301`.
    pub accepted_status_codes: String,
    pub max_redirects: i64,
    pub ignore_tls: bool,
    pub basic_auth_user: Option<String>,
    pub basic_auth_pass: Option<String>,
    /// RADIUS shared secret / SNMP community (masked like `basic_auth_pass`).
    pub secret: Option<String>,
    pub proxy_url: Option<String>,
    // -- keyword --
    pub keyword: Option<String>,
    pub keyword_invert: bool,
    // -- json_query --
    pub json_path: Option<String>,
    /// `==` | `!=` | `contains` | `<` | `<=` | `>` | `>=`.
    pub json_operator: String,
    pub json_expected: Option<String>,
    // -- dns --
    /// A | AAAA | CNAME | MX | NS | TXT | SOA | SRV | CAA.
    pub dns_record_type: String,
    pub dns_resolver: Option<String>,
    pub dns_expected: Option<String>,
    // -- docker --
    pub docker_host: Option<String>,
    // -- push --
    pub push_token: Option<String>,
    // -- expiry (TLS cert / domain) --
    pub expiry_notify: bool,
    pub expiry_days: i64,
    // -- alerting --
    /// Re-notify every N down checks; 0 = off.
    pub resend_interval: i64,
    /// Notify escalation channels after N minutes continuously down; 0 = off.
    pub escalation_minutes: i64,
    pub flap_detection: bool,
    // -- remote agents --
    pub run_on_agents: bool,
    pub confirm_with_agents: bool,
    // -- provenance --
    /// `manual` | `docker` | `config`.
    pub source: String,
    pub external_id: Option<String>,
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
    // -- HTTP authentication (see `checkers::http_auth`) --
    /// `none` | `basic` | `oauth2` | `mtls`.
    #[serde(default = "default_auth_method")]
    pub auth_method: String,
    #[serde(default)]
    pub oauth_token_url: Option<String>,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    #[serde(default)]
    pub oauth_client_secret: Option<String>,
    /// Space-separated scopes.
    #[serde(default)]
    pub oauth_scope: Option<String>,
    #[serde(default)]
    pub oauth_audience: Option<String>,
    /// `client_secret_basic` | `client_secret_post`.
    #[serde(default = "default_oauth_client_auth")]
    pub oauth_client_auth: String,
    /// PEM certificate (chain) sent to servers that ask for a client certificate.
    #[serde(default)]
    pub tls_client_cert: Option<String>,
    #[serde(default)]
    pub tls_client_key: Option<String>,
    /// Extra PEM CA certificate(s) to trust for the monitored server.
    #[serde(default)]
    pub tls_ca_cert: Option<String>,
    // -- real browser (see `checkers::real_browser`) --
    /// `remote_browsers.id`; `None` = the first configured remote browser.
    #[serde(default)]
    pub remote_browser_id: Option<i64>,
    /// CSS selector that must match (checked instead of `keyword`).
    #[serde(default)]
    pub browser_selector: Option<String>,
    /// `load` | `network_idle`.
    #[serde(default = "default_browser_wait")]
    pub browser_wait: String,
    /// `desktop` | `mobile`.
    #[serde(default = "default_browser_viewport")]
    pub browser_viewport: String,
    /// An uncaught JavaScript exception fails the check.
    #[serde(default)]
    pub browser_js_errors: bool,
    // -- gRPC keyword (see `checkers::grpc_keyword`) --
    /// `.proto` source for the called method; `None` = ask the server
    /// (server reflection).
    #[serde(default)]
    pub grpc_proto: Option<String>,
}

fn default_auth_method() -> String {
    "none".into()
}

fn default_browser_wait() -> String {
    "load".into()
}

fn default_browser_viewport() -> String {
    "desktop".into()
}

fn default_oauth_client_auth() -> String {
    "client_secret_basic".into()
}

impl Monitor {
    /// The writable part of this monitor, e.g. to clone it or to re-save it
    /// after changing a few fields.
    pub fn to_new(&self) -> NewMonitor {
        NewMonitor {
            name: self.name.clone(),
            description: self.description.clone(),
            monitor_type: self.monitor_type,
            target: self.target.clone(),
            parent_id: self.parent_id,
            interval_seconds: self.interval_seconds,
            retry_interval_seconds: self.retry_interval_seconds,
            timeout_seconds: self.timeout_seconds,
            retries: self.retries,
            active: self.active,
            upside_down: self.upside_down,
            http_method: self.http_method.clone(),
            http_headers: self.http_headers.clone(),
            http_body: self.http_body.clone(),
            http_body_encoding: self.http_body_encoding.clone(),
            accepted_status_codes: self.accepted_status_codes.clone(),
            max_redirects: self.max_redirects,
            ignore_tls: self.ignore_tls,
            basic_auth_user: self.basic_auth_user.clone(),
            basic_auth_pass: self.basic_auth_pass.clone(),
            secret: self.secret.clone(),
            proxy_url: self.proxy_url.clone(),
            keyword: self.keyword.clone(),
            keyword_invert: self.keyword_invert,
            json_path: self.json_path.clone(),
            json_operator: self.json_operator.clone(),
            json_expected: self.json_expected.clone(),
            dns_record_type: self.dns_record_type.clone(),
            dns_resolver: self.dns_resolver.clone(),
            dns_expected: self.dns_expected.clone(),
            docker_host: self.docker_host.clone(),
            push_token: self.push_token.clone(),
            expiry_notify: self.expiry_notify,
            expiry_days: self.expiry_days,
            resend_interval: self.resend_interval,
            escalation_minutes: self.escalation_minutes,
            flap_detection: self.flap_detection,
            run_on_agents: self.run_on_agents,
            confirm_with_agents: self.confirm_with_agents,
            source: self.source.clone(),
            external_id: self.external_id.clone(),
            sort_order: self.sort_order,
            auth_method: self.auth_method.clone(),
            oauth_token_url: self.oauth_token_url.clone(),
            oauth_client_id: self.oauth_client_id.clone(),
            oauth_client_secret: self.oauth_client_secret.clone(),
            oauth_scope: self.oauth_scope.clone(),
            oauth_audience: self.oauth_audience.clone(),
            oauth_client_auth: self.oauth_client_auth.clone(),
            tls_client_cert: self.tls_client_cert.clone(),
            tls_client_key: self.tls_client_key.clone(),
            tls_ca_cert: self.tls_ca_cert.clone(),
            remote_browser_id: self.remote_browser_id,
            browser_selector: self.browser_selector.clone(),
            browser_wait: self.browser_wait.clone(),
            browser_viewport: self.browser_viewport.clone(),
            browser_js_errors: self.browser_js_errors,
            grpc_proto: self.grpc_proto.clone(),
        }
    }

    /// Interval to wait before the next check while failing.
    pub fn effective_retry_interval(&self) -> i64 {
        self.retry_interval_seconds.unwrap_or(self.interval_seconds)
    }
}

/// Every writable column of `monitors` (no id / timestamps). `Default` gives
/// the schema defaults, so callers can write
/// `NewMonitor { name, monitor_type, target, ..NewMonitor::default() }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewMonitor {
    pub name: String,
    pub description: String,
    pub monitor_type: MonitorType,
    pub target: String,
    pub parent_id: Option<i64>,
    pub interval_seconds: i64,
    pub retry_interval_seconds: Option<i64>,
    pub timeout_seconds: i64,
    pub retries: i64,
    pub active: bool,
    pub upside_down: bool,
    pub http_method: String,
    pub http_headers: Option<String>,
    pub http_body: Option<String>,
    pub http_body_encoding: String,
    pub accepted_status_codes: String,
    pub max_redirects: i64,
    pub ignore_tls: bool,
    pub basic_auth_user: Option<String>,
    pub basic_auth_pass: Option<String>,
    pub secret: Option<String>,
    pub proxy_url: Option<String>,
    pub keyword: Option<String>,
    pub keyword_invert: bool,
    pub json_path: Option<String>,
    pub json_operator: String,
    pub json_expected: Option<String>,
    pub dns_record_type: String,
    pub dns_resolver: Option<String>,
    pub dns_expected: Option<String>,
    pub docker_host: Option<String>,
    pub push_token: Option<String>,
    pub expiry_notify: bool,
    pub expiry_days: i64,
    pub resend_interval: i64,
    pub escalation_minutes: i64,
    pub flap_detection: bool,
    pub run_on_agents: bool,
    pub confirm_with_agents: bool,
    pub source: String,
    pub external_id: Option<String>,
    pub sort_order: i64,
    pub auth_method: String,
    pub oauth_token_url: Option<String>,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub oauth_scope: Option<String>,
    pub oauth_audience: Option<String>,
    pub oauth_client_auth: String,
    pub tls_client_cert: Option<String>,
    pub tls_client_key: Option<String>,
    pub tls_ca_cert: Option<String>,
    pub remote_browser_id: Option<i64>,
    pub browser_selector: Option<String>,
    pub browser_wait: String,
    pub browser_viewport: String,
    pub browser_js_errors: bool,
    pub grpc_proto: Option<String>,
}

impl Default for NewMonitor {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            monitor_type: MonitorType::Http,
            target: String::new(),
            parent_id: None,
            interval_seconds: 60,
            retry_interval_seconds: None,
            timeout_seconds: 10,
            retries: 0,
            active: true,
            upside_down: false,
            http_method: "GET".into(),
            http_headers: None,
            http_body: None,
            http_body_encoding: "json".into(),
            accepted_status_codes: "200-399".into(),
            max_redirects: 10,
            ignore_tls: false,
            basic_auth_user: None,
            basic_auth_pass: None,
            secret: None,
            proxy_url: None,
            keyword: None,
            keyword_invert: false,
            json_path: None,
            json_operator: "==".into(),
            json_expected: None,
            dns_record_type: "A".into(),
            dns_resolver: None,
            dns_expected: None,
            docker_host: None,
            push_token: None,
            expiry_notify: true,
            expiry_days: 14,
            resend_interval: 0,
            escalation_minutes: 0,
            flap_detection: true,
            run_on_agents: false,
            confirm_with_agents: false,
            source: "manual".into(),
            external_id: None,
            sort_order: 0,
            auth_method: default_auth_method(),
            oauth_token_url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_scope: None,
            oauth_audience: None,
            oauth_client_auth: default_oauth_client_auth(),
            tls_client_cert: None,
            tls_client_key: None,
            tls_ca_cert: None,
            remote_browser_id: None,
            browser_selector: None,
            browser_wait: default_browser_wait(),
            browser_viewport: default_browser_viewport(),
            browser_js_errors: false,
            grpc_proto: None,
        }
    }
}

/// Dashboard row: a monitor plus its latest heartbeat summary.
#[derive(Debug, Clone)]
pub struct MonitorWithStatus {
    pub monitor: Monitor,
    pub status: Option<HeartbeatStatus>,
    pub response_time_ms: Option<i64>,
    /// Fixed-length, oldest-first heartbeat history for the sparkline; `None`
    /// entries are left-padding for monitors that don't have this much history yet.
    pub recent: Vec<Option<HeartbeatStatus>>,
    pub uptime_24h: Option<f64>,
}

// ---------------------------------------------------------------------------
// Check results (checkers -> engine)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryKind {
    Cert,
    Domain,
}

impl ExpiryKind {
    /// The value stored in `monitor_expiry.kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            ExpiryKind::Cert => "cert",
            ExpiryKind::Domain => "domain",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cert" => Some(ExpiryKind::Cert),
            "domain" => Some(ExpiryKind::Domain),
            _ => None,
        }
    }
}

impl fmt::Display for ExpiryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// TLS certificate or domain registration expiry discovered during a check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiryInfo {
    pub kind: ExpiryKind,
    pub subject: Option<String>,
    pub issuer: Option<String>,
    /// UTC, `YYYY-MM-DD HH:MM:SS`.
    pub expires_at: String,
}

/// The raw result of one probe, before the engine applies retries,
/// upside-down mode, maintenance, etc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub status: HeartbeatStatus,
    pub response_time_ms: Option<i64>,
    pub message: Option<String>,
    pub expiry: Option<ExpiryInfo>,
}

impl CheckOutcome {
    pub fn up(response_time_ms: i64) -> Self {
        Self {
            status: HeartbeatStatus::Up,
            response_time_ms: Some(response_time_ms),
            message: None,
            expiry: None,
        }
    }

    pub fn down(message: impl Into<String>) -> Self {
        Self {
            status: HeartbeatStatus::Down,
            response_time_ms: None,
            message: Some(message.into()),
            expiry: None,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_response_time(mut self, response_time_ms: i64) -> Self {
        self.response_time_ms = Some(response_time_ms);
        self
    }

    pub fn with_expiry(mut self, expiry: ExpiryInfo) -> Self {
        self.expiry = Some(expiry);
        self
    }
}

// Users & roles live in the kit.
pub use crate::kit::db::role::Role;

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

// Channels are the kit's; events are Vigilant Vole's (the kit sends `Alert`s).
pub use crate::kit::notify::alert::NotificationChannel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Down,
    Up,
    Reminder,
    Escalation,
    CertExpiring,
    DomainExpiring,
    MaintenanceStart,
    MaintenanceEnd,
    Flapping,
    Test,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Down => "down",
            EventKind::Up => "up",
            EventKind::Reminder => "reminder",
            EventKind::Escalation => "escalation",
            EventKind::CertExpiring => "cert_expiring",
            EventKind::DomainExpiring => "domain_expiring",
            EventKind::MaintenanceStart => "maintenance_start",
            EventKind::MaintenanceEnd => "maintenance_end",
            EventKind::Flapping => "flapping",
            EventKind::Test => "test",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EventKind::Down => crate::i18n::t("event_kind.down"),
            EventKind::Up => crate::i18n::t("event_kind.up"),
            EventKind::Reminder => crate::i18n::t("event_kind.reminder"),
            EventKind::Escalation => crate::i18n::t("event_kind.escalation"),
            EventKind::CertExpiring => crate::i18n::t("event_kind.cert_expiring"),
            EventKind::DomainExpiring => crate::i18n::t("event_kind.domain_expiring"),
            EventKind::MaintenanceStart => crate::i18n::t("event_kind.maintenance_start"),
            EventKind::MaintenanceEnd => crate::i18n::t("event_kind.maintenance_end"),
            EventKind::Flapping => crate::i18n::t("event_kind.flapping"),
            EventKind::Test => crate::i18n::t("event_kind.test"),
        }
    }

    /// Whether this event signals a problem (used for colour/severity).
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            EventKind::Down
                | EventKind::Reminder
                | EventKind::Escalation
                | EventKind::CertExpiring
                | EventKind::DomainExpiring
                | EventKind::Flapping
        )
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything a notification provider needs to render one alert.
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationEvent {
    pub kind: EventKind,
    pub monitor_id: Option<i64>,
    pub monitor_name: String,
    /// `MonitorType::as_str()` of the monitor (empty for non-monitor events).
    pub monitor_type: String,
    pub target: String,
    pub message: Option<String>,
    /// On `Up`: how long the monitor was down.
    pub downtime: Option<Duration>,
    /// On `CertExpiring` / `DomainExpiring`.
    pub expires_in_days: Option<i64>,
    /// Deep link (`base_url + /monitors/{id}`), when a base URL is configured.
    pub url: Option<String>,
    pub occurred_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_type_round_trips_through_its_string_form() {
        for t in MonitorType::all() {
            assert_eq!(MonitorType::parse(t.as_str()), Some(*t));
        }
    }

    #[test]
    fn monitor_type_serde_matches_db_strings() {
        for t in MonitorType::all() {
            let json = serde_json::to_string(t).unwrap();
            assert_eq!(json, format!("\"{}\"", t.as_str()));
        }
    }

    #[test]
    fn heartbeat_status_round_trips() {
        for s in [
            HeartbeatStatus::Up,
            HeartbeatStatus::Down,
            HeartbeatStatus::Pending,
            HeartbeatStatus::Maintenance,
        ] {
            assert_eq!(HeartbeatStatus::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn roles_are_ordered_by_privilege() {
        assert!(Role::Viewer < Role::Editor && Role::Editor < Role::Admin);
        assert!(!Role::Viewer.can_edit());
        assert!(Role::Editor.can_edit() && !Role::Editor.is_admin());
        assert!(Role::Admin.can_edit() && Role::Admin.is_admin());
    }

    #[test]
    fn role_round_trips() {
        for r in Role::all() {
            assert_eq!(Role::parse(r.as_str()), Some(*r));
        }
    }

    #[test]
    fn new_monitor_defaults_match_schema_defaults() {
        let m = NewMonitor::default();
        assert_eq!(m.interval_seconds, 60);
        assert_eq!(m.accepted_status_codes, "200-399");
        assert_eq!(m.source, "manual");
        assert!(m.active && m.expiry_notify && m.flap_detection);
    }
}
