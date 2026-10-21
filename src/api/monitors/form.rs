//! Monitor form: raw values (for re-rendering with the user's input intact),
//! parsing from urlencoded pairs, and validation into a [`NewMonitor`].
//!
//! Validation is pure: everything it needs from the database (allowed parent
//! groups, channel and tag ids) comes in through [`FormContext`], so it is
//! unit-tested without a connection.

use std::collections::BTreeMap;

use crate::db::models::{Monitor, MonitorType, NewMonitor};
use crate::db::tags::{self as db_tags, MonitorTag};
use crate::monitor::checkers::conn::SslMode;
use crate::monitor::checkers::http_auth::{self, AUTH_METHODS, MtlsError, OAUTH_CLIENT_AUTHS};
use crate::monitor::checkers::{
    self, BODY_ENCODINGS, DNS_RECORD_TYPES, HTTP_METHODS, JSON_OPERATORS,
};
use crate::notification::SECRET_MASK;

mod browser;
mod grpc;
mod target_opts;

pub const MAX_NAME_LEN: usize = 150;
pub const MAX_DESCRIPTION_LEN: usize = 2_000;
pub const MAX_TARGET_LEN: usize = 2_048;
pub const MAX_BODY_LEN: usize = 64 * 1024;
pub const MIN_INTERVAL: i64 = 10;
pub const MAX_INTERVAL: i64 = 86_400;
pub const MAX_TIMEOUT: i64 = 300;
pub const MAX_RETRIES: i64 = 20;
/// Longest accepted PEM text (certificate chain, key, CA bundle).
pub const MAX_PEM_LEN: usize = 64 * 1024;
const MAX_AUTH_FIELD_LEN: usize = 2_048;

/// Field name → message. Ordered, so the first error is stable.
pub type Errors = BTreeMap<&'static str, String>;

/// How a notification channel is attached to a monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    Off,
    /// Normal alerts (down/up/reminders).
    Alert,
    /// Only after `escalation_minutes` of continuous downtime.
    Escalation,
}

impl ChannelMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelMode::Off => "off",
            ChannelMode::Alert => "alert",
            ChannelMode::Escalation => "escalation",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "alert" | "true" | "on" => ChannelMode::Alert,
            "escalation" => ChannelMode::Escalation,
            _ => ChannelMode::Off,
        }
    }
}

/// The urlencoded body as ordered pairs (repeated keys allowed).
pub struct Pairs(pub Vec<(String, String)>);

impl Pairs {
    /// First value for `key`, untrimmed.
    pub fn raw(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// First value for `key`, trimmed ("" when missing).
    pub fn text(&self, key: &str) -> String {
        self.raw(key).map(str::trim).unwrap_or_default().to_string()
    }

    pub fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }

    /// Checkbox / switch: present with a truthy value.
    pub fn flag(&self, key: &str) -> bool {
        self.0
            .iter()
            .any(|(k, v)| k == key && matches!(v.as_str(), "true" | "on" | "1" | "yes"))
    }

    /// Every value for `key`.
    pub fn all(&self, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// Each target flavour has its own input (label, placeholder and help
/// differ per type); this is the field name used for `kind`.
pub fn target_field(kind: MonitorType) -> &'static str {
    match kind {
        MonitorType::Http | MonitorType::Keyword | MonitorType::JsonQuery => "target_url",
        MonitorType::Websocket => "target_ws",
        MonitorType::Tcp => "target_hostport",
        MonitorType::Ping => "target_host",
        MonitorType::TailscalePing => "target_tailscale",
        MonitorType::Dns => "target_dns",
        MonitorType::Docker => "target_docker",
        MonitorType::DomainExpiry => "target_domain",
        MonitorType::Ntp => "target_ntp",
        MonitorType::Steam => "target_steam",
        MonitorType::GameServer => "target_game",
        MonitorType::Tls => "target_tls",
        MonitorType::Smtp => "target_smtp",
        MonitorType::Redis => "target_redis",
        MonitorType::Postgres => "target_postgres",
        MonitorType::Mysql => "target_mysql",
        MonitorType::Mqtt => "target_mqtt",
        MonitorType::Kafka => "target_kafka",
        MonitorType::Rabbitmq => "target_rabbitmq",
        MonitorType::Grpc => "target_grpc",
        MonitorType::GrpcKeyword => "target_grpc_keyword",
        MonitorType::Radius => "target_radius",
        MonitorType::Snmp => "target_snmp",
        MonitorType::SqlServer => "target_sqlserver",
        MonitorType::Mongodb => "target_mongodb",
        MonitorType::RealBrowser => "target_browser",
        MonitorType::Push | MonitorType::Group => "target",
    }
}

/// Everything the form shows, as the user typed it.
#[derive(Debug, Clone, PartialEq)]
pub struct FormValues {
    pub name: String,
    pub description: String,
    pub monitor_type: String,
    pub target: String,
    /// `(field, value)` of the target options shown as choices (SNMP
    /// version and v3 protocols, Tailscale ping type): see `target_opts`.
    pub target_opts: Vec<(String, String)>,
    /// "" = no group.
    pub parent_id: String,
    pub interval: String,
    /// "" = same as interval.
    pub retry_interval: String,
    pub timeout: String,
    pub retries: String,
    pub upside_down: bool,
    pub http_method: String,
    pub accepted_status_codes: String,
    pub max_redirects: String,
    pub ignore_tls: bool,
    /// JSON object text.
    pub http_headers: String,
    pub http_body: String,
    pub http_body_encoding: String,
    pub basic_auth_user: String,
    pub basic_auth_pass: String,
    /// RADIUS shared secret / SNMP community.
    pub secret: String,
    pub proxy_url: String,
    pub keyword: String,
    pub keyword_invert: bool,
    pub json_path: String,
    pub json_operator: String,
    pub json_expected: String,
    pub dns_record_type: String,
    pub dns_resolver: String,
    pub dns_expected: String,
    pub docker_host: String,
    pub expiry_notify: bool,
    pub expiry_days: String,
    pub resend_interval: String,
    pub escalation_minutes: String,
    pub flap_detection: bool,
    pub run_on_agents: bool,
    pub confirm_with_agents: bool,
    /// `(channel_id, mode)` for every channel the form knows about.
    pub channels: Vec<(i64, ChannelMode)>,
    /// Selected tags with their values.
    pub tags: Vec<(i64, String)>,
    pub new_tag_name: String,
    pub new_tag_color: String,
    pub new_tag_value: String,
    /// `none` | `basic` | `oauth2` | `mtls`.
    pub auth_method: String,
    pub oauth_token_url: String,
    pub oauth_client_id: String,
    pub oauth_client_secret: String,
    pub oauth_scope: String,
    pub oauth_audience: String,
    pub oauth_client_auth: String,
    pub tls_client_cert: String,
    pub tls_client_key: String,
    pub tls_ca_cert: String,
    /// Real browser: remote browser id ("" = the first one).
    pub remote_browser_id: String,
    /// Real browser: `keyword` | `selector` (what the page is checked for).
    pub browser_check: String,
    pub browser_selector: String,
    pub browser_wait: String,
    pub browser_viewport: String,
    pub browser_js_errors: bool,
    /// gRPC keyword: `.proto` text ("" = server reflection).
    pub grpc_proto: String,
}

fn opt(s: &Option<String>) -> String {
    s.clone().unwrap_or_default()
}

impl FormValues {
    /// Values for a monitor (edit / clone) or for `NewMonitor::default()` (create).
    pub fn from_new(
        m: &NewMonitor,
        channels: Vec<(i64, ChannelMode)>,
        tags: &[MonitorTag],
    ) -> Self {
        let (target, target_opts) = target_opts::split(m.monitor_type, &m.target);
        Self {
            name: m.name.clone(),
            description: m.description.clone(),
            monitor_type: m.monitor_type.as_str().to_string(),
            target,
            target_opts,
            parent_id: m.parent_id.map(|p| p.to_string()).unwrap_or_default(),
            interval: m.interval_seconds.to_string(),
            retry_interval: m
                .retry_interval_seconds
                .map(|r| r.to_string())
                .unwrap_or_default(),
            timeout: m.timeout_seconds.to_string(),
            retries: m.retries.to_string(),
            upside_down: m.upside_down,
            http_method: m.http_method.clone(),
            accepted_status_codes: m.accepted_status_codes.clone(),
            max_redirects: m.max_redirects.to_string(),
            ignore_tls: m.ignore_tls,
            http_headers: opt(&m.http_headers),
            http_body: opt(&m.http_body),
            http_body_encoding: m.http_body_encoding.clone(),
            basic_auth_user: opt(&m.basic_auth_user),
            basic_auth_pass: opt(&m.basic_auth_pass),
            secret: opt(&m.secret),
            proxy_url: opt(&m.proxy_url),
            keyword: opt(&m.keyword),
            keyword_invert: m.keyword_invert,
            json_path: opt(&m.json_path),
            json_operator: m.json_operator.clone(),
            json_expected: opt(&m.json_expected),
            dns_record_type: m.dns_record_type.clone(),
            dns_resolver: opt(&m.dns_resolver),
            dns_expected: opt(&m.dns_expected),
            docker_host: opt(&m.docker_host),
            expiry_notify: m.expiry_notify,
            expiry_days: m.expiry_days.to_string(),
            resend_interval: m.resend_interval.to_string(),
            escalation_minutes: m.escalation_minutes.to_string(),
            flap_detection: m.flap_detection,
            run_on_agents: m.run_on_agents,
            confirm_with_agents: m.confirm_with_agents,
            channels,
            tags: tags.iter().map(|t| (t.tag_id, t.value.clone())).collect(),
            new_tag_name: String::new(),
            new_tag_color: db_tags::DEFAULT_COLOR.to_string(),
            new_tag_value: String::new(),
            auth_method: m.auth_method.clone(),
            oauth_token_url: opt(&m.oauth_token_url),
            oauth_client_id: opt(&m.oauth_client_id),
            oauth_client_secret: opt(&m.oauth_client_secret),
            oauth_scope: opt(&m.oauth_scope),
            oauth_audience: opt(&m.oauth_audience),
            oauth_client_auth: m.oauth_client_auth.clone(),
            tls_client_cert: opt(&m.tls_client_cert),
            tls_client_key: opt(&m.tls_client_key),
            tls_ca_cert: opt(&m.tls_ca_cert),
            remote_browser_id: m
                .remote_browser_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            browser_check: if m.browser_selector.is_some() {
                "selector"
            } else {
                "keyword"
            }
            .to_string(),
            browser_selector: opt(&m.browser_selector),
            browser_wait: m.browser_wait.clone(),
            browser_viewport: m.browser_viewport.clone(),
            browser_js_errors: m.browser_js_errors,
            grpc_proto: opt(&m.grpc_proto),
        }
    }

    pub fn from_monitor(
        m: &Monitor,
        channels: Vec<(i64, ChannelMode)>,
        tags: &[MonitorTag],
    ) -> Self {
        Self::from_new(&m.to_new(), channels, tags)
    }

    /// For forms sent to the browser: a stored password, RADIUS/SNMP secret,
    /// OAuth client secret or private key becomes [`SECRET_MASK`], which [`validate`] reads as
    /// "keep the stored one".
    pub fn masked(mut self) -> Self {
        for secret in [
            &mut self.basic_auth_pass,
            &mut self.secret,
            &mut self.oauth_client_secret,
            &mut self.tls_client_key,
        ] {
            if !secret.is_empty() {
                *secret = SECRET_MASK.to_string();
            }
        }
        self
    }

    /// For re-rendering a rejected submission: a freshly typed OAuth client
    /// secret or private key is not echoed back into the page (both are
    /// required for their method, so leaving them out can't silently clear
    /// a saved one). The mask stays, it is not a secret.
    pub fn without_typed_secrets(mut self) -> Self {
        for secret in [&mut self.oauth_client_secret, &mut self.tls_client_key] {
            if secret.trim() != SECRET_MASK {
                secret.clear();
            }
        }
        self
    }

    /// Parse a submitted form. Controls hidden by `data-show-when` are
    /// disabled and therefore missing; validation only reads the fields that
    /// apply to the chosen type and keeps the stored value for the rest.
    /// `known_channels` lists the channel ids the form rendered.
    pub fn from_pairs(p: &Pairs, known_channels: &[i64]) -> Self {
        let channels = known_channels
            .iter()
            .map(|id| (*id, ChannelMode::parse(&p.text(&format!("channel_{id}")))))
            .collect();
        // Selected tags arrive as `tag_{id}=true` (or repeated `tag={id}`).
        let mut tags: Vec<(i64, String)> = Vec::new();
        let selected = p.0.iter().filter_map(|(k, v)| {
            if k == "tag" {
                v.trim().parse::<i64>().ok()
            } else {
                let id = k.strip_prefix("tag_")?.parse::<i64>().ok()?;
                matches!(v.as_str(), "true" | "on" | "1").then_some(id)
            }
        });
        for id in selected {
            if !tags.iter().any(|(t, _)| *t == id) {
                tags.push((id, p.text(&format!("tag_value_{id}"))));
            }
        }
        Self {
            name: p.text("name"),
            description: p.raw("description").unwrap_or_default().trim().to_string(),
            monitor_type: p.text("monitor_type"),
            target: {
                let kind = MonitorType::parse(&p.text("monitor_type")).unwrap_or_default();
                p.raw(target_field(kind))
                    .or_else(|| p.raw("target"))
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            },
            target_opts: target_opts::read(
                MonitorType::parse(&p.text("monitor_type")).unwrap_or_default(),
                p,
            ),
            parent_id: p.text("parent_id"),
            interval: p.text("interval_seconds"),
            retry_interval: p.text("retry_interval_seconds"),
            timeout: p.text("timeout_seconds"),
            retries: p.text("retries"),
            upside_down: p.flag("upside_down"),
            http_method: p.text("http_method"),
            accepted_status_codes: p.text("accepted_status_codes"),
            max_redirects: p.text("max_redirects"),
            ignore_tls: p.flag("ignore_tls"),
            http_headers: p.text("http_headers"),
            // Bodies keep their whitespace.
            http_body: p.raw("http_body").unwrap_or_default().to_string(),
            http_body_encoding: p.text("http_body_encoding"),
            basic_auth_user: p.text("basic_auth_user"),
            basic_auth_pass: p.raw("basic_auth_pass").unwrap_or_default().to_string(),
            secret: p.raw("secret").unwrap_or_default().to_string(),
            proxy_url: p.text("proxy_url"),
            keyword: p.raw("keyword").unwrap_or_default().to_string(),
            keyword_invert: p.flag("keyword_invert"),
            json_path: p.text("json_path"),
            json_operator: p.text("json_operator"),
            json_expected: p.text("json_expected"),
            dns_record_type: p.text("dns_record_type"),
            dns_resolver: p.text("dns_resolver"),
            dns_expected: p.text("dns_expected"),
            docker_host: p.text("docker_host"),
            expiry_notify: p.flag("expiry_notify"),
            expiry_days: p.text("expiry_days"),
            resend_interval: p.text("resend_interval"),
            escalation_minutes: p.text("escalation_minutes"),
            flap_detection: p.flag("flap_detection"),
            run_on_agents: p.flag("run_on_agents"),
            confirm_with_agents: p.flag("confirm_with_agents"),
            channels,
            tags,
            new_tag_name: p.text("new_tag_name"),
            new_tag_color: p.text("new_tag_color"),
            new_tag_value: p.text("new_tag_value"),
            // Forms posted without the field (older clients) mean basic auth
            // when a user name is given.
            auth_method: match p.text("auth_method") {
                m if m.is_empty() => {
                    http_auth::implied_method(p.raw("basic_auth_user")).to_string()
                }
                m => m,
            },
            oauth_token_url: p.text("oauth_token_url"),
            oauth_client_id: p.text("oauth_client_id"),
            // Secrets and PEM text keep their whitespace.
            oauth_client_secret: p.raw("oauth_client_secret").unwrap_or_default().to_string(),
            oauth_scope: p.text("oauth_scope"),
            oauth_audience: p.text("oauth_audience"),
            oauth_client_auth: p.text("oauth_client_auth"),
            tls_client_cert: p.raw("tls_client_cert").unwrap_or_default().to_string(),
            tls_client_key: p.raw("tls_client_key").unwrap_or_default().to_string(),
            tls_ca_cert: p.raw("tls_ca_cert").unwrap_or_default().to_string(),
            remote_browser_id: p.text("remote_browser_id"),
            browser_check: p.text("browser_check"),
            browser_selector: p.text("browser_selector"),
            browser_wait: p.text("browser_wait"),
            browser_viewport: p.text("browser_viewport"),
            browser_js_errors: p.flag("browser_js_errors"),
            // Source text keeps its whitespace.
            grpc_proto: p.raw("grpc_proto").unwrap_or_default().to_string(),
        }
    }

    pub fn monitor_type(&self) -> MonitorType {
        MonitorType::parse(&self.monitor_type).unwrap_or_default()
    }

    pub fn channel_mode(&self, id: i64) -> ChannelMode {
        self.channels
            .iter()
            .find(|(c, _)| *c == id)
            .map(|(_, m)| *m)
            .unwrap_or(ChannelMode::Off)
    }

    /// The value for a selected tag, or `None` when not selected.
    pub fn tag_value(&self, id: i64) -> Option<&str> {
        self.tags
            .iter()
            .find(|(t, _)| *t == id)
            .map(|(_, v)| v.as_str())
    }

    /// Header rows for the key–value editor (at least one, possibly empty).
    pub fn header_rows(&self) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&self.http_headers)
                .map(|obj| {
                    obj.into_iter()
                        .map(|(k, v)| {
                            let v = match v {
                                serde_json::Value::String(s) => s,
                                other => other.to_string(),
                            };
                            (k, v)
                        })
                        .collect()
                })
                .unwrap_or_default();
        if rows.is_empty() {
            rows.push((String::new(), String::new()));
        }
        rows
    }
}

/// What validation needs to know from the database.
#[derive(Debug, Clone, Default)]
pub struct FormContext {
    /// The monitor being edited (`None` when creating).
    pub editing_id: Option<i64>,
    /// Group monitors that may be chosen as parent (already excludes the
    /// monitor itself and its descendants).
    pub allowed_parents: Vec<i64>,
    /// Existing notification channel ids.
    pub channel_ids: Vec<i64>,
    /// Existing tag ids.
    pub tag_ids: Vec<i64>,
    /// Number of direct children (blocks turning a group into another type).
    pub child_count: usize,
    /// Remote browser ids (oldest first; the first is the default).
    pub remote_browser_ids: Vec<i64>,
    /// Real-browser monitors may be saved without any remote browser
    /// (config imports, which warn about it instead).
    pub browser_optional: bool,
}

/// A validated submission.
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub monitor: NewMonitor,
    /// `(channel_id, escalation)`.
    pub channels: Vec<(i64, bool)>,
    pub tags: Vec<(i64, String)>,
    /// `(name, palette colour, value)` of a tag to create (or reuse) and attach.
    pub new_tag: Option<(String, String, String)>,
}

fn int_in(
    errors: &mut Errors,
    field: &'static str,
    raw: &str,
    min: i64,
    max: i64,
    what: &str,
) -> Option<i64> {
    match raw.trim().parse::<i64>() {
        Ok(v) if (min..=max).contains(&v) => Some(v),
        Ok(_) => {
            errors.insert(
                field,
                crate::i18n::tf(
                    "validation.int_range",
                    &[
                        ("what", what),
                        ("min", &min.to_string()),
                        ("max", &max.to_string()),
                    ],
                ),
            );
            None
        }
        Err(_) => {
            errors.insert(
                field,
                crate::i18n::tf(
                    "validation.int_whole",
                    &[("what", what), ("min", &min.to_string())],
                ),
            );
            None
        }
    }
}

// -- shared field rules (the monitor form and bulk edit) --------------------

/// `interval_seconds`: the check interval.
pub(crate) fn check_interval(e: &mut Errors, raw: &str) -> Option<i64> {
    int_in(
        e,
        "interval_seconds",
        raw,
        MIN_INTERVAL,
        MAX_INTERVAL,
        crate::i18n::t("validation.what_interval"),
    )
}

/// `retries`: failed checks before a monitor counts as down.
pub(crate) fn check_retries(e: &mut Errors, raw: &str) -> Option<i64> {
    int_in(
        e,
        "retries",
        raw,
        0,
        MAX_RETRIES,
        crate::i18n::t("validation.what_retries"),
    )
}

/// `retry_interval_seconds` (a value; the form treats "" as "same as the interval").
pub(crate) fn check_retry_interval(e: &mut Errors, raw: &str) -> Option<i64> {
    int_in(
        e,
        "retry_interval_seconds",
        raw,
        MIN_INTERVAL,
        MAX_INTERVAL,
        crate::i18n::t("validation.what_retry_interval"),
    )
}

/// `timeout_seconds`: how long one check may take.
pub(crate) fn check_timeout(e: &mut Errors, raw: &str) -> Option<i64> {
    int_in(
        e,
        "timeout_seconds",
        raw,
        1,
        MAX_TIMEOUT,
        crate::i18n::t("validation.what_timeout"),
    )
}

/// Whether a check with this timeout fits in the interval.
pub(crate) fn timeout_fits(timeout: i64, interval: i64) -> bool {
    timeout <= interval
}

/// Flags `timeout_seconds` when the timeout is longer than the interval.
pub(crate) fn check_timeout_fits(e: &mut Errors, timeout: i64, interval: i64) {
    if !timeout_fits(timeout, interval) {
        e.insert(
            "timeout_seconds",
            crate::i18n::t("validation.timeout_gt_interval").into(),
        );
    }
}

/// Why a tag name can't be used (`None` when it can; empty names are the caller's business).
pub(crate) fn tag_name_error(name: &str) -> Option<String> {
    (name.trim().chars().count() > db_tags::MAX_NAME_LEN).then(|| {
        crate::i18n::tf(
            "validation.tag_name_long",
            &[("max", &db_tags::MAX_NAME_LEN.to_string())],
        )
    })
}

/// Why a tag value can't be used (`None` when it can).
pub(crate) fn tag_value_error(value: &str) -> Option<String> {
    (value.chars().count() > db_tags::MAX_VALUE_LEN).then(|| {
        crate::i18n::tf(
            "validation.tag_value_long",
            &[("max", &db_tags::MAX_VALUE_LEN.to_string())],
        )
    })
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Types whose checks can report a certificate or domain expiry, i.e. the ones
/// the form shows the expiry section for (keep in sync with `sec-expiry` in
/// `monitor_form_fields.html`).
pub(crate) fn reports_expiry(kind: MonitorType) -> bool {
    kind.is_http_family()
        || matches!(
            kind,
            MonitorType::DomainExpiry
                | MonitorType::Tls
                | MonitorType::Smtp
                | MonitorType::Redis
                | MonitorType::Postgres
                | MonitorType::Mysql
                | MonitorType::Mqtt
                | MonitorType::Kafka
                | MonitorType::Rabbitmq
                | MonitorType::Grpc
                | MonitorType::GrpcKeyword
                | MonitorType::SqlServer
                | MonitorType::Mongodb
        )
}

fn first_line(msg: &str) -> String {
    let mut s = msg.trim().to_string();
    if let Some(first) = s.chars().next() {
        let upper: String = first.to_uppercase().collect();
        s.replace_range(..first.len_utf8(), &upper);
    }
    if !s.ends_with('.') {
        s.push('.');
    }
    s
}

/// Validates the target for `kind`; returns the normalised target.
pub fn validate_target(kind: MonitorType, target: &str) -> Result<String, String> {
    let t = target.trim();
    if matches!(kind, MonitorType::Push | MonitorType::Group) {
        return Ok(String::new());
    }
    if t.is_empty() {
        return Err(match kind {
            k if k.is_http_family() => crate::i18n::t("validation.target_url").into(),
            MonitorType::Websocket => crate::i18n::t("validation.target_ws").into(),
            MonitorType::RealBrowser => crate::i18n::t("validation.target_url").into(),
            MonitorType::Docker => crate::i18n::t("validation.target_docker").into(),
            MonitorType::DomainExpiry => crate::i18n::t("validation.target_domain").into(),
            _ => crate::i18n::t("validation.target_host").into(),
        });
    }
    if t.chars().count() > MAX_TARGET_LEN {
        return Err(crate::i18n::tf(
            "validation.target_long",
            &[("max", &MAX_TARGET_LEN.to_string())],
        ));
    }
    let host_like = |t: &str| -> Result<(), String> {
        if t.contains("://") || t.contains('/') {
            return Err(crate::i18n::t("validation.host_only").into());
        }
        if t.chars().any(char::is_whitespace) {
            return Err(crate::i18n::t("validation.host_spaces").into());
        }
        Ok(())
    };
    match kind {
        MonitorType::Http
        | MonitorType::Keyword
        | MonitorType::JsonQuery
        | MonitorType::RealBrowser => {
            let url = url::Url::parse(t)
                .map_err(|_| crate::i18n::t("validation.url_full").to_string())?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(crate::i18n::t("validation.url_scheme").into());
            }
            if url.host_str().is_none_or(str::is_empty) {
                return Err(crate::i18n::t("validation.url_host").into());
            }
            Ok(t.to_string())
        }
        MonitorType::Websocket => checkers::websocket::parse_ws_url(t)
            .map(|_| t.to_string())
            .map_err(|e| first_line(&e)),
        MonitorType::Tcp => {
            host_like(t)?;
            checkers::util::split_host_port(t, None)
                .map(|_| t.to_string())
                .map_err(|_| crate::i18n::t("validation.host_port").to_string())
        }
        MonitorType::Ntp => {
            host_like(t)?;
            checkers::util::split_host_port(t, Some(123))
                .map(|_| t.to_string())
                .map_err(|e| first_line(&e))
        }
        MonitorType::Steam => {
            host_like(t)?;
            checkers::util::split_host_port(t, Some(27015))
                .map(|_| t.to_string())
                .map_err(|e| first_line(&e))
        }
        MonitorType::Tls => {
            host_like(t)?;
            checkers::util::split_host_port(t, Some(443))
                .map(|_| t.to_string())
                .map_err(|e| first_line(&e))
        }
        MonitorType::Smtp => {
            host_like(t)?;
            checkers::util::split_host_port(t, Some(25))
                .map(|_| t.to_string())
                .map_err(|e| first_line(&e))
        }
        MonitorType::Redis
        | MonitorType::Postgres
        | MonitorType::Mysql
        | MonitorType::Mqtt
        | MonitorType::Kafka
        | MonitorType::Rabbitmq
        | MonitorType::Grpc
        | MonitorType::Radius
        | MonitorType::SqlServer
        | MonitorType::Mongodb
        | MonitorType::GameServer => checkers::endpoint::parse(kind, t)
            .map(|_| t.to_string())
            .map_err(|e| first_line(&e)),
        MonitorType::GrpcKeyword => checkers::grpc_keyword::parse_target(t)
            .map(|_| t.to_string())
            .map_err(|e| first_line(&e)),
        MonitorType::Ping | MonitorType::Dns => {
            host_like(t)?;
            Ok(t.to_string())
        }
        MonitorType::Snmp => checkers::endpoint::parse(kind, t)
            .and_then(|ep| checkers::snmp::options(&ep))
            .map(|_| t.to_string())
            .map_err(|e| first_line(&e)),
        MonitorType::TailscalePing => checkers::tailscale::Target::parse(t)
            .map(|_| t.to_string())
            .map_err(|e| first_line(&e)),
        MonitorType::Docker => {
            if t.chars().any(char::is_whitespace) {
                return Err(crate::i18n::t("validation.container_spaces").into());
            }
            Ok(t.to_string())
        }
        MonitorType::DomainExpiry => checkers::domain::registrable_domain(t)
            .map(|_| t.to_string())
            .map_err(|e| first_line(&e)),
        MonitorType::Push | MonitorType::Group => Ok(String::new()),
    }
}

/// Accepted Docker host forms (empty = local socket).
pub fn validate_docker_host(host: &str) -> Result<(), String> {
    let h = host.trim();
    if h.is_empty()
        || h.starts_with('/')
        || ["unix://", "npipe://", "tcp://", "http://"]
            .iter()
            .any(|p| h.starts_with(p))
    {
        Ok(())
    } else if h.starts_with("https://") {
        Err(crate::i18n::t("validation.docker_tls").into())
    } else {
        Err(crate::i18n::t("validation.docker_host").into())
    }
}

fn validate_proxy(proxy: &str) -> Result<(), String> {
    let url =
        url::Url::parse(proxy).map_err(|_| crate::i18n::t("validation.proxy_url").to_string())?;
    if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") {
        return Err(crate::i18n::t("validation.proxy_scheme").into());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(crate::i18n::t("validation.proxy_host").into());
    }
    Ok(())
}

/// Scheme, host and port of a target URL (the whole target if it isn't one).
fn destination(target: &str) -> String {
    match url::Url::parse(target.trim()) {
        Ok(u) => format!(
            "{}://{}:{}",
            u.scheme(),
            u.host_str().unwrap_or_default().to_ascii_lowercase(),
            u.port_or_known_default().unwrap_or_default()
        ),
        Err(_) => target.trim().to_ascii_lowercase(),
    }
}

/// Whether credentials stored for `old` would still reach the same server
/// (and through the same proxy) in `new`.
fn same_destination(old: &NewMonitor, new: &NewMonitor) -> bool {
    destination(&old.target) == destination(&new.target) && old.proxy_url == new.proxy_url
}

/// Whether a secret stored for `old` would still reach the same server in
/// `new`, at least as well protected (service monitors compare type, host
/// and port, not the path). Like the scheme in an HTTP [`destination`],
/// dropping TLS (`rediss://` → `redis://`, a lower `sslmode`) puts the
/// secret on the wire in the clear, so it has to be entered again.
fn same_service(old: &NewMonitor, new: &NewMonitor) -> bool {
    let kind = old.monitor_type;
    kind == new.monitor_type
        && match (
            checkers::endpoint::parse(kind, &old.target),
            checkers::endpoint::parse(kind, &new.target),
        ) {
            (Ok(a), Ok(b)) => {
                let (was, now) = (protection(kind, &a), protection(kind, &b));
                let same_host = |(h1, p1): &(String, u16), (h2, p2): &(String, u16)| {
                    h1.eq_ignore_ascii_case(h2) && p1 == p2
                };
                a.host.eq_ignore_ascii_case(&b.host)
                    && a.port == b.port
                    && a.srv == b.srv
                    && a.more_hosts.len() == b.more_hosts.len()
                    && a.more_hosts
                        .iter()
                        .zip(&b.more_hosts)
                        .all(|(x, y)| same_host(x, y))
                    && now.0 >= was.0
                    && now.1 >= was.1
            }
            _ => false,
        }
}

/// How a service target protects what is sent over it: TLS from the first
/// byte (from the scheme) and, for the databases, the `sslmode` (or what
/// SQL Server's `encrypt` and MongoDB's TLS options amount to on that scale).
fn protection(kind: MonitorType, ep: &checkers::endpoint::Endpoint) -> (bool, SslMode) {
    let sslmode = match kind {
        MonitorType::Postgres | MonitorType::Mysql => SslMode::of(ep).unwrap_or(SslMode::Disable),
        MonitorType::SqlServer => {
            checkers::sqlserver::Settings::of(ep).map_or(SslMode::Disable, |s| s.protection())
        }
        MonitorType::Mongodb => checkers::mongodb::protection(ep),
        // SNMPv3 never sends its passwords; v1/v2c sends the secret (the
        // community) in the clear, so going back from v3 asks for it again.
        MonitorType::Snmp
            if checkers::snmp::options(ep)
                .is_ok_and(|o| o.version == checkers::snmp::Version::V3) =>
        {
            SslMode::Require
        }
        _ => SslMode::Disable,
    };
    (ep.tls, sslmode)
}

/// PEM text as stored: line endings normalised (browsers send CRLF), trimmed.
fn pem_text(s: &str) -> Option<String> {
    non_empty(&s.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Settles a submitted secret. [`SECRET_MASK`] means "keep the stored one",
/// which only holds while `unchanged` says it still goes to the same place;
/// otherwise an editor could point the monitor at their own server and
/// receive it. Anything else is a new value, empty clears it.
fn resolve_secret(
    e: &mut Errors,
    field: &'static str,
    submitted: &str,
    stored: Option<&String>,
    unchanged: bool,
    reenter: &'static str,
) -> Option<String> {
    if submitted.trim() != SECRET_MASK {
        return (!submitted.trim().is_empty()).then(|| submitted.to_string());
    }
    match stored {
        Some(secret) if unchanged => Some(secret.clone()),
        _ => {
            e.insert(field, crate::i18n::t(reenter).into());
            None
        }
    }
}

/// The authentication section of an HTTP-family monitor. Only the chosen
/// method's settings are kept; the others are cleared so no stale secret
/// lingers (or travels in exports) after switching methods.
fn validate_auth(v: &FormValues, base: &NewMonitor, m: &mut NewMonitor, e: &mut Errors) {
    let method = match v.auth_method.trim() {
        "" => "none",
        m => m,
    };
    if !AUTH_METHODS.contains(&method) {
        e.insert("auth_method", crate::i18n::t("httpauth.err_method").into());
    }
    m.auth_method = if AUTH_METHODS.contains(&method) {
        method.to_string()
    } else {
        "none".into()
    };
    m.basic_auth_user = None;
    m.basic_auth_pass = None;
    m.oauth_token_url = None;
    m.oauth_client_id = None;
    m.oauth_client_secret = None;
    m.oauth_scope = None;
    m.oauth_audience = None;
    m.oauth_client_auth = "client_secret_basic".into();
    m.tls_client_cert = None;
    m.tls_client_key = None;
    m.tls_ca_cert = None;

    match m.auth_method.as_str() {
        "basic" => {
            m.basic_auth_user = non_empty(&v.basic_auth_user);
            let unchanged = same_destination(base, m);
            m.basic_auth_pass = resolve_secret(
                e,
                "basic_auth_pass",
                &v.basic_auth_pass,
                base.basic_auth_pass.as_ref(),
                unchanged,
                "validation.basic_pass_reenter",
            );
            if m.basic_auth_user.is_none() {
                let key = if m.basic_auth_pass.is_some() {
                    "validation.basic_user"
                } else {
                    "httpauth.err_basic_user"
                };
                e.insert("basic_auth_user", crate::i18n::t(key).into());
            }
        }
        "oauth2" => {
            m.oauth_token_url = non_empty(&v.oauth_token_url);
            match &m.oauth_token_url {
                None => {
                    e.insert(
                        "oauth_token_url",
                        crate::i18n::t("httpauth.err_token_url").into(),
                    );
                }
                Some(u) if u.chars().count() > MAX_TARGET_LEN => {
                    e.insert(
                        "oauth_token_url",
                        crate::i18n::tf(
                            "validation.target_long",
                            &[("max", &MAX_TARGET_LEN.to_string())],
                        ),
                    );
                }
                Some(u) => {
                    if http_auth::parse_token_url(u).is_err() {
                        e.insert(
                            "oauth_token_url",
                            crate::i18n::t("httpauth.err_token_url_invalid").into(),
                        );
                    }
                }
            }
            m.oauth_client_id = non_empty(&v.oauth_client_id);
            if m.oauth_client_id.is_none() {
                e.insert(
                    "oauth_client_id",
                    crate::i18n::t("httpauth.err_client_id").into(),
                );
            }
            // The secret goes to the token endpoint, so that is its destination.
            let unchanged = destination(base.oauth_token_url.as_deref().unwrap_or_default())
                == destination(m.oauth_token_url.as_deref().unwrap_or_default())
                && base.proxy_url == m.proxy_url;
            m.oauth_client_secret = resolve_secret(
                e,
                "oauth_client_secret",
                &v.oauth_client_secret,
                base.oauth_client_secret.as_ref(),
                unchanged,
                "httpauth.err_secret_reenter",
            );
            if m.oauth_client_secret.is_none() && !e.contains_key("oauth_client_secret") {
                e.insert(
                    "oauth_client_secret",
                    crate::i18n::t("httpauth.err_secret").into(),
                );
            }
            let scope = v
                .oauth_scope
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            m.oauth_scope = (!scope.is_empty()).then_some(scope);
            m.oauth_audience = non_empty(&v.oauth_audience);
            for (field, value) in [
                ("oauth_client_id", &m.oauth_client_id),
                ("oauth_scope", &m.oauth_scope),
                ("oauth_audience", &m.oauth_audience),
            ] {
                if value
                    .as_ref()
                    .is_some_and(|t| t.chars().count() > MAX_AUTH_FIELD_LEN)
                {
                    e.insert(
                        field,
                        crate::i18n::tf(
                            "httpauth.err_long",
                            &[("max", &MAX_AUTH_FIELD_LEN.to_string())],
                        ),
                    );
                }
            }
            match v.oauth_client_auth.trim() {
                "" => {}
                other if OAUTH_CLIENT_AUTHS.contains(&other) => {
                    m.oauth_client_auth = other.to_string()
                }
                _ => {
                    e.insert(
                        "oauth_client_auth",
                        crate::i18n::t("httpauth.err_client_auth").into(),
                    );
                }
            }
        }
        "mtls" => {
            m.tls_client_cert = pem_text(&v.tls_client_cert);
            m.tls_ca_cert = pem_text(&v.tls_ca_cert);
            let unchanged = same_destination(base, m);
            m.tls_client_key = resolve_secret(
                e,
                "tls_client_key",
                &v.tls_client_key.replace("\r\n", "\n"),
                base.tls_client_key.as_ref(),
                unchanged,
                "httpauth.err_key_reenter",
            )
            .map(|k| k.trim().to_string());
            if url::Url::parse(m.target.trim()).is_ok_and(|u| u.scheme() == "http") {
                e.insert(
                    "auth_method",
                    crate::i18n::t("httpauth.err_mtls_https").into(),
                );
            }
            for (field, value) in [
                ("tls_client_cert", &m.tls_client_cert),
                ("tls_client_key", &m.tls_client_key),
                ("tls_ca_cert", &m.tls_ca_cert),
            ] {
                if value.as_ref().is_some_and(|t| t.len() > MAX_PEM_LEN) {
                    e.insert(
                        field,
                        crate::i18n::tf(
                            "httpauth.err_pem_long",
                            &[("max", &(MAX_PEM_LEN / 1024).to_string())],
                        ),
                    );
                }
            }
            if !["tls_client_cert", "tls_client_key", "tls_ca_cert"]
                .iter()
                .any(|f| e.contains_key(f))
            {
                let problems = http_auth::mtls_problems(
                    m.tls_client_cert.as_deref().unwrap_or_default(),
                    m.tls_client_key.as_deref().unwrap_or_default(),
                    m.tls_ca_cert.as_deref(),
                );
                for err in problems {
                    let (field, key, detail) = match err {
                        MtlsError::MissingCert => {
                            ("tls_client_cert", "httpauth.err_cert", String::new())
                        }
                        MtlsError::CertNotPem => {
                            ("tls_client_cert", "httpauth.err_cert_pem", String::new())
                        }
                        MtlsError::MissingKey => {
                            ("tls_client_key", "httpauth.err_key", String::new())
                        }
                        MtlsError::KeyNotPem => {
                            ("tls_client_key", "httpauth.err_key_pem", String::new())
                        }
                        MtlsError::KeyEncrypted => (
                            "tls_client_key",
                            "httpauth.err_key_encrypted",
                            String::new(),
                        ),
                        MtlsError::CaNotPem => {
                            ("tls_ca_cert", "httpauth.err_ca_pem", String::new())
                        }
                        MtlsError::Rejected(why) => ("tls_client_key", "httpauth.err_pair", why),
                        MtlsError::CaRejected(why) => {
                            ("tls_ca_cert", "httpauth.err_ca_rejected", why)
                        }
                    };
                    e.insert(field, crate::i18n::tf(key, &[("err", &detail)]));
                }
            }
        }
        _ => {}
    }
}

/// Validate `v` on top of `base` (the stored monitor when editing, defaults
/// when creating). Fields that don't apply to the chosen type keep `base`'s value.
pub fn validate(v: &FormValues, base: &NewMonitor, cx: &FormContext) -> Result<Validated, Errors> {
    let mut e = Errors::new();
    let mut m = base.clone();

    // -- general ---------------------------------------------------------
    let kind = match MonitorType::parse(&v.monitor_type) {
        Some(k) => k,
        None => {
            e.insert(
                "monitor_type",
                crate::i18n::t("validation.monitor_type").into(),
            );
            base.monitor_type
        }
    };
    if base.monitor_type == MonitorType::Group && kind != MonitorType::Group && cx.child_count > 0 {
        e.insert(
            "monitor_type",
            crate::i18n::tn("validation.group_children", cx.child_count as i64),
        );
    }
    m.monitor_type = kind;

    let name = v.name.trim();
    if name.is_empty() {
        e.insert("name", crate::i18n::t("validation.name").into());
    } else if name.chars().count() > MAX_NAME_LEN {
        e.insert(
            "name",
            crate::i18n::tf(
                "validation.name_long",
                &[("max", &MAX_NAME_LEN.to_string())],
            ),
        );
    }
    m.name = name.to_string();
    if v.description.chars().count() > MAX_DESCRIPTION_LEN {
        e.insert(
            "description",
            crate::i18n::tf(
                "validation.description_long",
                &[("max", &MAX_DESCRIPTION_LEN.to_string())],
            ),
        );
    }
    m.description = v.description.trim().to_string();

    let typed_target = target_opts::merge(kind, &v.target, &v.target_opts);
    match validate_target(kind, &typed_target) {
        Ok(t) => m.target = t,
        Err(msg) => {
            e.insert("target", msg);
            m.target = typed_target.trim().to_string();
        }
    }

    m.parent_id = match v.parent_id.trim() {
        "" => None,
        raw => match raw.parse::<i64>() {
            Ok(id) if cx.allowed_parents.contains(&id) => Some(id),
            _ => {
                e.insert("parent_id", crate::i18n::t("validation.parent").into());
                None
            }
        },
    };

    // -- timing ----------------------------------------------------------
    if kind != MonitorType::Group {
        if let Some(i) = check_interval(&mut e, &v.interval) {
            m.interval_seconds = i;
        }
        m.retries = check_retries(&mut e, &v.retries).unwrap_or(m.retries);
    }
    if kind.is_active_check() {
        m.retry_interval_seconds = match v.retry_interval.trim() {
            "" => None,
            raw => check_retry_interval(&mut e, raw),
        };
        if let Some(t) = check_timeout(&mut e, &v.timeout) {
            m.timeout_seconds = t;
            if !e.contains_key("interval_seconds") {
                check_timeout_fits(&mut e, t, m.interval_seconds);
            }
        }
    }
    if kind != MonitorType::Group {
        m.upside_down = v.upside_down;
    }

    // -- HTTP family -----------------------------------------------------
    if kind.is_http_family() {
        let method = v.http_method.trim().to_ascii_uppercase();
        if HTTP_METHODS.contains(&method.as_str()) {
            m.http_method = method;
        } else {
            e.insert("http_method", crate::i18n::t("validation.method").into());
        }
        let codes = v.accepted_status_codes.trim();
        match checkers::parse_status_codes(codes) {
            Ok(_) => {
                m.accepted_status_codes = if codes.is_empty() {
                    "200-399".into()
                } else {
                    codes.to_string()
                }
            }
            Err(msg) => {
                e.insert(
                    "accepted_status_codes",
                    format!(
                        "{} {}",
                        first_line(&msg),
                        crate::i18n::t("validation.codes_hint")
                    ),
                );
            }
        }
        m.max_redirects = int_in(
            &mut e,
            "max_redirects",
            &v.max_redirects,
            0,
            30,
            crate::i18n::t("validation.what_redirects"),
        )
        .unwrap_or(m.max_redirects);
        m.ignore_tls = v.ignore_tls;
        m.http_headers = match non_empty(&v.http_headers) {
            None => None,
            Some(h) => match checkers::parse_headers(&h) {
                Ok(_) => Some(h),
                Err(msg) => {
                    e.insert("http_headers", first_line(&msg));
                    None
                }
            },
        };
        let encoding = v.http_body_encoding.trim();
        if BODY_ENCODINGS.contains(&encoding) {
            m.http_body_encoding = encoding.to_string();
        } else if !encoding.is_empty() {
            e.insert(
                "http_body_encoding",
                crate::i18n::t("validation.encoding").into(),
            );
        }
        m.http_body = if v.http_body.trim().is_empty() {
            None
        } else {
            Some(v.http_body.clone())
        };
        if let Some(body) = &m.http_body {
            if body.len() > MAX_BODY_LEN {
                e.insert("http_body", crate::i18n::t("validation.body_long").into());
            } else if m.http_body_encoding == "json"
                && serde_json::from_str::<serde_json::Value>(body).is_err()
            {
                e.insert("http_body", crate::i18n::t("validation.body_json").into());
            }
        }
        m.proxy_url = non_empty(&v.proxy_url);
        if let Some(proxy) = &m.proxy_url
            && let Err(msg) = validate_proxy(proxy)
        {
            e.insert("proxy_url", msg);
        }
        validate_auth(v, base, &mut m, &mut e);
    }

    // -- service monitors (Redis, PostgreSQL, MQTT, …) ---------------------
    let svc = checkers::endpoint::fields(kind);
    if svc.tls {
        m.ignore_tls = v.ignore_tls;
    }
    let keep_ok = same_service(base, &m);
    if svc.credentials {
        m.basic_auth_user = non_empty(&v.basic_auth_user);
        m.basic_auth_pass = resolve_secret(
            &mut e,
            "basic_auth_pass",
            &v.basic_auth_pass,
            base.basic_auth_pass.as_ref(),
            keep_ok,
            "validation.secret_reenter",
        );
        if m.basic_auth_user.is_none() {
            if kind == MonitorType::Postgres
                || kind == MonitorType::Rabbitmq && m.basic_auth_pass.is_some()
            {
                e.insert(
                    "basic_auth_user",
                    crate::i18n::t("validation.user_required").into(),
                );
            } else if m.basic_auth_pass.is_some() && !svc.password_only {
                e.insert(
                    "basic_auth_user",
                    crate::i18n::t("validation.basic_user").into(),
                );
            }
        }
    }
    if svc.secret {
        m.secret = resolve_secret(
            &mut e,
            "secret",
            &v.secret,
            base.secret.as_ref(),
            keep_ok,
            "validation.secret_reenter",
        );
        if m.secret.is_none() && kind == MonitorType::Radius {
            e.insert(
                "secret",
                crate::i18n::t("validation.secret_required").into(),
            );
        }
    }
    if svc.keyword {
        m.keyword = (!v.keyword.trim().is_empty()).then(|| v.keyword.trim().to_string());
        m.keyword_invert = v.keyword_invert;
        if kind == MonitorType::Mqtt
            && m.keyword.is_some()
            && checkers::endpoint::parse(kind, &m.target).is_ok()
            && checkers::mqtt::topic(&m.target).is_none()
        {
            e.insert(
                "keyword",
                crate::i18n::t("validation.mqtt_keyword_topic").into(),
            );
        }
    }
    if kind == MonitorType::Snmp {
        target_opts::snmp_security(&mut m, &mut e);
    }

    if kind == MonitorType::Keyword {
        if v.keyword.trim().is_empty() {
            e.insert("keyword", crate::i18n::t("validation.keyword").into());
        }
        m.keyword = (!v.keyword.trim().is_empty()).then(|| v.keyword.clone());
        m.keyword_invert = v.keyword_invert;
    }

    if kind == MonitorType::JsonQuery {
        m.json_path = non_empty(&v.json_path);
        match &m.json_path {
            None => {
                e.insert("json_path", crate::i18n::t("validation.json_path").into());
            }
            Some(path) => {
                if let Err(err) = serde_json_path::JsonPath::parse(path) {
                    e.insert(
                        "json_path",
                        crate::i18n::tf(
                            "validation.json_path_invalid",
                            &[("err", &err.to_string())],
                        ),
                    );
                }
            }
        }
        let op = v.json_operator.trim();
        if JSON_OPERATORS.contains(&op) {
            m.json_operator = op.to_string();
        } else {
            e.insert(
                "json_operator",
                crate::i18n::t("validation.json_operator").into(),
            );
        }
        m.json_expected = non_empty(&v.json_expected);
        if m.json_expected.is_none() {
            e.insert(
                "json_expected",
                crate::i18n::t("validation.json_expected").into(),
            );
        }
    }

    if kind == MonitorType::Dns {
        let rt = v.dns_record_type.trim().to_ascii_uppercase();
        if DNS_RECORD_TYPES.contains(&rt.as_str()) {
            m.dns_record_type = rt;
        } else {
            e.insert(
                "dns_record_type",
                crate::i18n::t("validation.record_type").into(),
            );
        }
        m.dns_resolver = non_empty(&v.dns_resolver);
        if let Some(r) = &m.dns_resolver
            && let Err(msg) = checkers::parse_dns_resolver(r)
        {
            e.insert(
                "dns_resolver",
                format!(
                    "{} {}",
                    first_line(&msg),
                    crate::i18n::t("validation.resolver_hint")
                ),
            );
        }
        m.dns_expected = non_empty(&v.dns_expected);
    }

    if kind == MonitorType::Docker {
        m.docker_host = non_empty(&v.docker_host);
        if let Err(msg) = validate_docker_host(&v.docker_host) {
            e.insert("docker_host", msg);
        }
    }

    if reports_expiry(kind) {
        m.expiry_notify = v.expiry_notify;
        m.expiry_days = int_in(
            &mut e,
            "expiry_days",
            &v.expiry_days,
            1,
            365,
            crate::i18n::t("validation.what_days"),
        )
        .unwrap_or(m.expiry_days);
    }

    if kind == MonitorType::RealBrowser {
        browser::validate(v, base, cx, &mut m, &mut e);
    }

    if kind == MonitorType::GrpcKeyword {
        grpc::validate(v, &mut m, &mut e);
    }

    // -- alerting --------------------------------------------------------
    m.resend_interval = int_in(
        &mut e,
        "resend_interval",
        &v.resend_interval,
        0,
        10_000,
        crate::i18n::t("validation.what_checks"),
    )
    .unwrap_or(m.resend_interval);
    m.escalation_minutes = int_in(
        &mut e,
        "escalation_minutes",
        &v.escalation_minutes,
        0,
        100_000,
        crate::i18n::t("validation.what_minutes"),
    )
    .unwrap_or(m.escalation_minutes);
    m.flap_detection = v.flap_detection;

    let channels: Vec<(i64, bool)> = v
        .channels
        .iter()
        .filter(|(id, _)| cx.channel_ids.contains(id))
        .filter_map(|(id, mode)| match mode {
            ChannelMode::Off => None,
            ChannelMode::Alert => Some((*id, false)),
            ChannelMode::Escalation => Some((*id, true)),
        })
        .collect();
    if channels.iter().any(|(_, esc)| *esc)
        && m.escalation_minutes == 0
        && !e.contains_key("escalation_minutes")
    {
        e.insert(
            "escalation_minutes",
            crate::i18n::t("validation.escalation_minutes").into(),
        );
    }

    // -- remote agents ---------------------------------------------------
    if kind.is_active_check() {
        m.run_on_agents = v.run_on_agents;
        m.confirm_with_agents = v.confirm_with_agents;
    } else {
        m.run_on_agents = false;
        m.confirm_with_agents = false;
    }

    // -- push ------------------------------------------------------------
    if kind != MonitorType::Push {
        m.push_token = None;
    }

    // -- tags ------------------------------------------------------------
    let mut tags = Vec::new();
    for (id, value) in &v.tags {
        if !cx.tag_ids.contains(id) {
            continue;
        }
        if let Some(msg) = tag_value_error(value) {
            e.insert("tags", msg);
        }
        tags.push((*id, value.trim().to_string()));
    }
    let new_tag = match v.new_tag_name.trim() {
        "" => None,
        name => match tag_name_error(name) {
            Some(msg) => {
                e.insert("new_tag_name", msg);
                None
            }
            None => {
                if let Some(msg) = tag_value_error(&v.new_tag_value) {
                    e.insert("new_tag_name", msg);
                }
                Some((
                    name.to_string(),
                    db_tags::normalize_color(&v.new_tag_color).to_string(),
                    v.new_tag_value.trim().to_string(),
                ))
            }
        },
    };

    if e.is_empty() {
        Ok(Validated {
            monitor: m,
            channels,
            tags,
            new_tag,
        })
    } else {
        Err(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Pairs {
        Pairs(
            list.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn base_pairs(kind: &str, target: &str) -> Vec<(&'static str, String)> {
        vec![
            ("name", "Website".into()),
            ("monitor_type", kind.into()),
            ("target", target.into()),
            ("interval_seconds", "60".into()),
            ("timeout_seconds", "10".into()),
            ("retries", "1".into()),
            ("http_method", "GET".into()),
            ("accepted_status_codes", "200-299".into()),
            ("max_redirects", "5".into()),
            ("http_body_encoding", "json".into()),
            ("json_operator", "==".into()),
            ("dns_record_type", "A".into()),
            ("expiry_days", "14".into()),
            ("resend_interval", "0".into()),
            ("escalation_minutes", "0".into()),
            ("flap_detection", "true".into()),
        ]
    }

    fn run(list: Vec<(&'static str, String)>, cx: &FormContext) -> Result<Validated, Errors> {
        let p = Pairs(list.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
        let v = FormValues::from_pairs(&p, &cx.channel_ids);
        validate(&v, &NewMonitor::default(), cx)
    }

    fn with(
        mut list: Vec<(&'static str, String)>,
        key: &'static str,
        value: &str,
    ) -> Vec<(&'static str, String)> {
        list.retain(|(k, _)| *k != key);
        list.push((key, value.to_string()));
        list
    }

    #[test]
    fn valid_http_monitor() {
        let ok = run(
            base_pairs("http", "https://example.com/health"),
            &FormContext::default(),
        )
        .unwrap();
        assert_eq!(ok.monitor.name, "Website");
        assert_eq!(ok.monitor.monitor_type, MonitorType::Http);
        assert_eq!(ok.monitor.accepted_status_codes, "200-299");
        assert_eq!(ok.monitor.max_redirects, 5);
        assert_eq!(ok.monitor.retries, 1);
        assert!(ok.monitor.flap_detection);
        assert!(ok.channels.is_empty() && ok.tags.is_empty() && ok.new_tag.is_none());
    }

    #[test]
    fn required_fields_and_numbers_report_inline_errors() {
        let list = with(
            with(base_pairs("http", ""), "name", "  "),
            "interval_seconds",
            "abc",
        );
        let errors = run(with(list, "timeout_seconds", "0"), &FormContext::default()).unwrap_err();
        assert!(errors.contains_key("name"));
        assert!(errors.contains_key("target"));
        assert!(errors["interval_seconds"].contains("whole number"));
        assert!(errors["timeout_seconds"].contains("between 1 and"));
    }

    #[test]
    fn timeout_must_fit_in_interval() {
        let list = with(
            with(base_pairs("http", "https://x.io"), "timeout_seconds", "90"),
            "interval_seconds",
            "30",
        );
        assert!(
            run(list, &FormContext::default())
                .unwrap_err()
                .contains_key("timeout_seconds")
        );
    }

    #[test]
    fn targets_are_validated_per_type() {
        let cases: &[(MonitorType, &str, bool)] = &[
            (MonitorType::Http, "https://example.com", true),
            (MonitorType::Http, "example.com", false),
            (MonitorType::Http, "ftp://example.com", false),
            (MonitorType::Keyword, "http://10.0.0.1:8080/x", true),
            (MonitorType::Tcp, "db.internal:5432", true),
            (MonitorType::Tcp, "[::1]:22", true),
            (MonitorType::Tcp, "db.internal", false),
            (MonitorType::Tcp, "db:99999", false),
            (MonitorType::Ping, "192.0.2.1", true),
            (MonitorType::Ping, "https://example.com", false),
            (MonitorType::Ping, "a host", false),
            (MonitorType::Dns, "example.com", true),
            (MonitorType::Ntp, "pool.ntp.org", true),
            (MonitorType::Ntp, "pool.ntp.org:123", true),
            (MonitorType::Steam, "203.0.113.5", true),
            (MonitorType::Websocket, "wss://example.com/socket", true),
            (MonitorType::Websocket, "not a url", false),
            (MonitorType::Docker, "postgres", true),
            (MonitorType::Docker, "my container", false),
            (
                MonitorType::DomainExpiry,
                "https://www.example.co.uk/path",
                true,
            ),
            (MonitorType::Push, "", true),
            (MonitorType::Group, "ignored", true),
        ];
        for (kind, target, ok) in cases {
            assert_eq!(
                validate_target(*kind, target).is_ok(),
                *ok,
                "{kind:?} {target:?}"
            );
        }
        assert_eq!(validate_target(MonitorType::Group, "x").unwrap(), "");
    }

    #[test]
    fn http_options_use_checker_parsers() {
        let list = with(
            base_pairs("http", "https://x.io"),
            "accepted_status_codes",
            "200-199",
        );
        let list = with(list, "http_headers", "[1,2]");
        let list = with(list, "http_body", "{not json");
        let list = with(list, "proxy_url", "ftp://proxy");
        let list = with(list, "http_method", "BREW");
        let errors = run(list, &FormContext::default()).unwrap_err();
        for key in [
            "accepted_status_codes",
            "http_headers",
            "http_body",
            "proxy_url",
            "http_method",
        ] {
            assert!(
                errors.contains_key(key),
                "missing error for {key}: {errors:?}"
            );
        }
        // Raw body encoding accepts anything; headers JSON object is kept.
        let list = with(
            base_pairs("http", "https://x.io"),
            "http_body_encoding",
            "raw",
        );
        let list = with(list, "http_body", "{not json");
        let list = with(list, "http_headers", r#"{"X-Token":"abc"}"#);
        let ok = run(list, &FormContext::default()).unwrap();
        assert_eq!(
            ok.monitor.http_headers.as_deref(),
            Some(r#"{"X-Token":"abc"}"#)
        );
        assert_eq!(ok.monitor.http_body.as_deref(), Some("{not json"));
    }

    #[test]
    fn type_specific_requirements() {
        let kw = run(
            base_pairs("keyword", "https://x.io"),
            &FormContext::default(),
        )
        .unwrap_err();
        assert!(kw.contains_key("keyword"));
        let json = run(
            with(
                base_pairs("json_query", "https://x.io"),
                "json_path",
                "$[bad",
            ),
            &FormContext::default(),
        )
        .unwrap_err();
        assert!(json.contains_key("json_path") && json.contains_key("json_expected"));
        let dns = run(
            with(
                base_pairs("dns", "example.com"),
                "dns_resolver",
                "not-an-ip",
            ),
            &FormContext::default(),
        )
        .unwrap_err();
        assert!(dns.contains_key("dns_resolver"));
        let docker = run(
            with(
                base_pairs("docker", "web"),
                "docker_host",
                "https://docker:2376",
            ),
            &FormContext::default(),
        )
        .unwrap_err();
        assert!(docker.contains_key("docker_host"));
        let ok = run(
            with(
                with(base_pairs("dns", "example.com"), "dns_resolver", "1.1.1.1"),
                "dns_record_type",
                "mx",
            ),
            &FormContext::default(),
        )
        .unwrap();
        assert_eq!(ok.monitor.dns_record_type, "MX");
    }

    #[test]
    fn stored_basic_auth_password_is_masked_and_only_kept_for_the_same_destination() {
        let stored = NewMonitor {
            target: "https://api.example.com/health".into(),
            basic_auth_user: Some("bot".into()),
            basic_auth_pass: Some("hunter2".into()),
            ..NewMonitor::default()
        };
        // The edit form never contains the real password.
        let shown = FormValues::from_new(&stored, Vec::new(), &[]).masked();
        assert_eq!(shown.basic_auth_pass, SECRET_MASK);

        let submit = |target: &str, proxy: &str, pass: &str| {
            let list = with(
                with(
                    with(base_pairs("http", target), "basic_auth_user", "bot"),
                    "basic_auth_pass",
                    pass,
                ),
                "proxy_url",
                proxy,
            );
            let p = Pairs(list.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
            validate(
                &FormValues::from_pairs(&p, &[]),
                &stored,
                &FormContext::default(),
            )
        };
        // Untouched mask, same server (other path): the stored password stays.
        let kept = submit("https://API.example.com:443/other", "", SECRET_MASK).unwrap();
        assert_eq!(kept.monitor.basic_auth_pass.as_deref(), Some("hunter2"));
        // New host, port, scheme or proxy: must be typed again.
        for (target, proxy) in [
            ("https://attacker.example/health", ""),
            ("https://api.example.com:8443/health", ""),
            ("http://api.example.com/health", ""),
            ("https://api.example.com/health", "http://proxy.evil:3128"),
        ] {
            let err = submit(target, proxy, SECRET_MASK).unwrap_err();
            assert!(err.contains_key("basic_auth_pass"), "{target} {proxy}");
        }
        // Typing a new password is always fine; clearing it removes it.
        let fresh = submit("https://attacker.example/health", "", "new-pass").unwrap();
        assert_eq!(fresh.monitor.basic_auth_pass.as_deref(), Some("new-pass"));
        assert_eq!(
            submit("https://api.example.com/health", "", "")
                .unwrap()
                .monitor
                .basic_auth_pass,
            None
        );
        // A mask on a new monitor (clone) has nothing to restore.
        let p = Pairs(
            with(
                with(
                    base_pairs("http", "https://api.example.com/"),
                    "basic_auth_user",
                    "bot",
                ),
                "basic_auth_pass",
                SECRET_MASK,
            )
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        );
        let err = validate(
            &FormValues::from_pairs(&p, &[]),
            &NewMonitor::default(),
            &FormContext::default(),
        )
        .unwrap_err();
        assert!(err.contains_key("basic_auth_pass"));
    }

    #[test]
    fn fields_of_other_types_keep_their_stored_values() {
        let base = NewMonitor {
            keyword: Some("keep".into()),
            http_method: "POST".into(),
            expiry_notify: true,
            ..NewMonitor::default()
        };
        let p = Pairs(
            base_pairs("tcp", "db:5432")
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        );
        let v = FormValues::from_pairs(&p, &[]);
        let ok = validate(&v, &base, &FormContext::default()).unwrap();
        assert_eq!(ok.monitor.keyword.as_deref(), Some("keep"));
        assert_eq!(ok.monitor.http_method, "POST");
        assert!(
            ok.monitor.expiry_notify,
            "hidden switch must not reset the stored value"
        );
    }

    #[test]
    fn service_types_save_their_expiry_settings() {
        let base = NewMonitor {
            expiry_notify: true,
            expiry_days: 14,
            ..NewMonitor::default()
        };
        for (kind, target) in [
            ("redis", "rediss://cache:6380"),
            ("mongodb", "mongodb://db"),
            ("sqlserver", "sqlserver://db"),
        ] {
            let mut pairs: Vec<(String, String)> = base_pairs(kind, target)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            pairs.retain(|(k, _)| k != "expiry_notify" && k != "expiry_days");
            pairs.push(("expiry_days".into(), "7".into()));
            let v = FormValues::from_pairs(&Pairs(pairs), &[]);
            let ok = validate(&v, &base, &FormContext::default())
                .unwrap_or_else(|e| panic!("{kind}: {e:?}"));
            assert!(
                !ok.monitor.expiry_notify,
                "{kind}: switching expiry alerts off must stick"
            );
            assert_eq!(
                ok.monitor.expiry_days, 7,
                "{kind}: expiry days must be saved"
            );
        }
    }

    #[test]
    fn parent_channels_and_tags_are_checked_against_context() {
        let cx = FormContext {
            allowed_parents: vec![5],
            channel_ids: vec![1, 2, 3],
            tag_ids: vec![10, 11],
            ..FormContext::default()
        };
        let list = with(base_pairs("http", "https://x.io"), "parent_id", "5");
        let list = with(list, "channel_1", "alert");
        let list = with(list, "channel_2", "escalation");
        let list = with(list, "channel_9", "alert");
        let list = with(list, "escalation_minutes", "15");
        let mut list = with(list, "tag", "10");
        list.push(("tag", "99".into()));
        list.push(("tag", "10".into()));
        list.push(("tag_value_10", " prod ".into()));
        list.push(("new_tag_name", "Team".into()));
        list.push(("new_tag_color", "#ff0000".into()));
        let ok = run(list, &cx).unwrap();
        assert_eq!(ok.monitor.parent_id, Some(5));
        assert_eq!(ok.channels, vec![(1, false), (2, true)]);
        assert_eq!(ok.tags, vec![(10, "prod".to_string())]);
        assert_eq!(
            ok.new_tag,
            Some(("Team".into(), "red".into(), String::new()))
        );

        let bad = run(
            with(base_pairs("http", "https://x.io"), "parent_id", "6"),
            &cx,
        )
        .unwrap_err();
        assert!(bad.contains_key("parent_id"));
        // Escalation channels need an escalation delay.
        let esc = run(
            with(
                base_pairs("http", "https://x.io"),
                "channel_3",
                "escalation",
            ),
            &cx,
        )
        .unwrap_err();
        assert!(esc.contains_key("escalation_minutes"));
    }

    #[test]
    fn group_with_children_cannot_change_type() {
        let base = NewMonitor {
            monitor_type: MonitorType::Group,
            ..NewMonitor::default()
        };
        let cx = FormContext {
            child_count: 2,
            ..FormContext::default()
        };
        let p = Pairs(
            base_pairs("http", "https://x.io")
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        );
        let errors = validate(&FormValues::from_pairs(&p, &[]), &base, &cx).unwrap_err();
        assert!(errors["monitor_type"].contains("2 monitors"));
    }

    #[test]
    fn push_and_group_need_no_target_and_clear_push_token_otherwise() {
        let ok = run(base_pairs("push", ""), &FormContext::default()).unwrap();
        assert_eq!(ok.monitor.target, "");
        let base = NewMonitor {
            push_token: Some("tok".into()),
            monitor_type: MonitorType::Push,
            ..NewMonitor::default()
        };
        let p = Pairs(
            base_pairs("http", "https://x.io")
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        );
        let ok = validate(
            &FormValues::from_pairs(&p, &[]),
            &base,
            &FormContext::default(),
        )
        .unwrap();
        assert_eq!(ok.monitor.push_token, None);
    }

    #[test]
    fn target_comes_from_the_field_of_the_chosen_type() {
        let p = pairs(&[
            ("monitor_type", "tcp"),
            ("target_url", "https://wrong.example"),
            ("target_hostport", " db:5432 "),
        ]);
        assert_eq!(FormValues::from_pairs(&p, &[]).target, "db:5432");
        let fallback = pairs(&[("monitor_type", "http"), ("target", "https://x.io")]);
        assert_eq!(
            FormValues::from_pairs(&fallback, &[]).target,
            "https://x.io"
        );
    }

    #[test]
    fn form_values_round_trip_a_monitor() {
        let m = NewMonitor {
            name: "API".into(),
            http_headers: Some(r#"{"A":"1","B":2}"#.into()),
            ..NewMonitor::default()
        };
        let v = FormValues::from_new(&m, vec![(3, ChannelMode::Escalation)], &[]);
        assert_eq!(v.interval, "60");
        assert_eq!(v.channel_mode(3), ChannelMode::Escalation);
        assert_eq!(v.channel_mode(4), ChannelMode::Off);
        assert_eq!(
            v.header_rows(),
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string())
            ]
        );
        let p = pairs(&[
            ("name", "x"),
            ("channel_3", "alert"),
            ("tag_4", "true"),
            ("tag_value_4", "v"),
            ("tag_6", ""),
            ("tag_value_6", "ignored"),
        ]);
        let parsed = FormValues::from_pairs(&p, &[3, 8]);
        assert_eq!(
            parsed.channels,
            vec![(3, ChannelMode::Alert), (8, ChannelMode::Off)]
        );
        assert_eq!(parsed.tag_value(4), Some("v"));
        assert_eq!(parsed.tag_value(5), None);
    }

    // -- HTTP authentication methods ---------------------------------------

    fn submit_with(
        base: &NewMonitor,
        list: Vec<(&'static str, String)>,
    ) -> Result<Validated, Errors> {
        let p = Pairs(list.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
        validate(
            &FormValues::from_pairs(&p, &[]),
            base,
            &FormContext::default(),
        )
    }

    fn oauth_pairs(secret: &str) -> Vec<(&'static str, String)> {
        let mut list = base_pairs("http", "https://api.example.com/health");
        for (k, v) in [
            ("auth_method", "oauth2"),
            ("oauth_token_url", "https://login.example.com/oauth/token"),
            ("oauth_client_id", "vole"),
            ("oauth_client_secret", secret),
            ("oauth_scope", "  read   write "),
            ("oauth_audience", "https://api.example.com"),
            ("oauth_client_auth", "client_secret_post"),
        ] {
            list = with(list, k, v);
        }
        list
    }

    fn stored_oauth() -> NewMonitor {
        NewMonitor {
            target: "https://api.example.com/health".into(),
            auth_method: "oauth2".into(),
            oauth_token_url: Some("https://login.example.com/oauth/token".into()),
            oauth_client_id: Some("vole".into()),
            oauth_client_secret: Some("the-client-secret".into()),
            ..NewMonitor::default()
        }
    }

    #[test]
    fn oauth2_settings_are_validated_and_normalised() {
        let ok = submit_with(&NewMonitor::default(), oauth_pairs("sekret"))
            .unwrap()
            .monitor;
        assert_eq!(ok.auth_method, "oauth2");
        assert_eq!(
            ok.oauth_token_url.as_deref(),
            Some("https://login.example.com/oauth/token")
        );
        assert_eq!(ok.oauth_client_id.as_deref(), Some("vole"));
        assert_eq!(ok.oauth_client_secret.as_deref(), Some("sekret"));
        assert_eq!(ok.oauth_scope.as_deref(), Some("read write"));
        assert_eq!(
            ok.oauth_audience.as_deref(),
            Some("https://api.example.com")
        );
        assert_eq!(ok.oauth_client_auth, "client_secret_post");

        let mut bad = with(
            oauth_pairs(""),
            "oauth_token_url",
            "login.example.com/token",
        );
        bad = with(bad, "oauth_client_id", " ");
        let err = submit_with(&NewMonitor::default(), bad).unwrap_err();
        for key in ["oauth_token_url", "oauth_client_id", "oauth_client_secret"] {
            assert!(err.contains_key(key), "{key}: {err:?}");
        }
        let err = submit_with(
            &NewMonitor::default(),
            with(oauth_pairs("x"), "oauth_token_url", "ftp://idp/token"),
        )
        .unwrap_err();
        assert!(err["oauth_token_url"].contains("http://"), "{err:?}");
        let err = submit_with(
            &NewMonitor::default(),
            with(oauth_pairs("x"), "oauth_client_auth", "private_key_jwt"),
        )
        .unwrap_err();
        assert!(err.contains_key("oauth_client_auth"));
        let err = submit_with(
            &NewMonitor::default(),
            with(oauth_pairs("x"), "auth_method", "kerberos"),
        )
        .unwrap_err();
        assert!(err.contains_key("auth_method"));
    }

    #[test]
    fn stored_oauth_secret_is_masked_and_only_kept_for_the_same_token_url() {
        let stored = stored_oauth();
        let shown = FormValues::from_new(&stored, Vec::new(), &[]).masked();
        assert_eq!(shown.oauth_client_secret, SECRET_MASK);
        assert!(!format!("{shown:?}").contains("the-client-secret"));

        // Untouched mask: kept, even when the monitored URL itself changes.
        let kept = submit_with(
            &stored,
            with(
                with(
                    oauth_pairs(SECRET_MASK),
                    "target_url",
                    "https://other.example.com/",
                ),
                "target",
                "https://other.example.com/",
            ),
        )
        .unwrap();
        assert_eq!(
            kept.monitor.oauth_client_secret.as_deref(),
            Some("the-client-secret")
        );
        // A different token endpoint or proxy: the secret must be typed again.
        for (field, value) in [
            ("oauth_token_url", "https://attacker.example/token"),
            ("oauth_token_url", "http://login.example.com/oauth/token"),
            ("proxy_url", "http://proxy.evil:3128"),
        ] {
            let err =
                submit_with(&stored, with(oauth_pairs(SECRET_MASK), field, value)).unwrap_err();
            assert!(
                err.contains_key("oauth_client_secret"),
                "{field}={value}: {err:?}"
            );
        }
        // Typing a new one is always fine; so is a new endpoint with it.
        let fresh = submit_with(
            &stored,
            with(
                oauth_pairs("new"),
                "oauth_token_url",
                "https://attacker.example/token",
            ),
        )
        .unwrap();
        assert_eq!(fresh.monitor.oauth_client_secret.as_deref(), Some("new"));
        // A mask with nothing stored (clone / import) has nothing to restore.
        let err = submit_with(&NewMonitor::default(), oauth_pairs(SECRET_MASK)).unwrap_err();
        assert!(err.contains_key("oauth_client_secret"));
        // A re-rendered rejected form never echoes a typed secret, but keeps the mask.
        let typed = FormValues::from_pairs(
            &Pairs(
                oauth_pairs("typed-secret")
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            ),
            &[],
        );
        assert_eq!(
            typed.clone().without_typed_secrets().oauth_client_secret,
            ""
        );
        let masked = FormValues {
            oauth_client_secret: SECRET_MASK.into(),
            ..typed
        };
        assert_eq!(
            masked.without_typed_secrets().oauth_client_secret,
            SECRET_MASK
        );
    }

    #[test]
    fn switching_method_clears_the_other_methods_settings() {
        let basic = NewMonitor {
            target: "https://api.example.com/health".into(),
            auth_method: "basic".into(),
            basic_auth_user: Some("bot".into()),
            basic_auth_pass: Some("hunter2".into()),
            ..NewMonitor::default()
        };
        let out = submit_with(&basic, oauth_pairs("sekret")).unwrap().monitor;
        assert_eq!((out.basic_auth_user, out.basic_auth_pass), (None, None));
        let none = submit_with(
            &stored_oauth(),
            with(
                base_pairs("http", "https://api.example.com/health"),
                "auth_method",
                "none",
            ),
        )
        .unwrap()
        .monitor;
        assert_eq!(none.auth_method, "none");
        assert_eq!(
            (none.oauth_token_url, none.oauth_client_secret),
            (None, None)
        );
        assert_eq!(none.oauth_client_auth, "client_secret_basic");
    }

    #[test]
    fn forms_without_auth_method_keep_meaning_basic_auth() {
        // Older clients (and the existing form tests) only post the basic fields.
        let list = with(
            with(base_pairs("http", "https://x.io"), "basic_auth_user", "bot"),
            "basic_auth_pass",
            "pw",
        );
        let ok = submit_with(&NewMonitor::default(), list).unwrap().monitor;
        assert_eq!(
            (
                ok.auth_method.as_str(),
                ok.basic_auth_user.as_deref(),
                ok.basic_auth_pass.as_deref()
            ),
            ("basic", Some("bot"), Some("pw"))
        );
        let ok = submit_with(&NewMonitor::default(), base_pairs("http", "https://x.io"))
            .unwrap()
            .monitor;
        assert_eq!(ok.auth_method, "none");
        // A stored method is shown as it is: "none" stays none even with a
        // leftover user name (migration 0096 turned old rows into "basic").
        let off = NewMonitor {
            basic_auth_user: Some("bot".into()),
            ..NewMonitor::default()
        };
        assert_eq!(
            FormValues::from_new(&off, Vec::new(), &[]).auth_method,
            "none"
        );
        let ok = submit_with(
            &off,
            with(base_pairs("http", "https://x.io"), "auth_method", "none"),
        )
        .unwrap()
        .monitor;
        assert_eq!(
            (ok.auth_method.as_str(), ok.basic_auth_user.as_deref()),
            ("none", None)
        );
        // Basic auth chosen explicitly needs a user name.
        let err = submit_with(
            &NewMonitor::default(),
            with(base_pairs("http", "https://x.io"), "auth_method", "basic"),
        )
        .unwrap_err();
        assert!(err.contains_key("basic_auth_user"));
    }

    #[test]
    fn stored_service_secrets_are_not_kept_when_tls_is_dropped() {
        let stored = |kind: MonitorType, target: &str| NewMonitor {
            monitor_type: kind,
            target: target.into(),
            basic_auth_user: Some("vole".into()),
            basic_auth_pass: Some("hunter2".into()),
            ..NewMonitor::default()
        };
        let submit = |base: &NewMonitor, kind: &str, target: &str| {
            let list = with(
                with(base_pairs(kind, target), "basic_auth_user", "vole"),
                "basic_auth_pass",
                SECRET_MASK,
            );
            submit_with(base, list)
        };
        for (kind, from, to) in [
            (
                MonitorType::Redis,
                "rediss://cache.local:6380",
                "redis://cache.local:6380",
            ),
            (
                MonitorType::Mqtt,
                "mqtts://broker.local:8883",
                "mqtt://broker.local:8883",
            ),
            (
                MonitorType::Rabbitmq,
                "amqps://mq.local:5671",
                "amqp://mq.local:5671",
            ),
            (
                MonitorType::Postgres,
                "postgres://db.local/app?sslmode=require",
                "postgres://db.local/app?sslmode=disable",
            ),
            (
                MonitorType::Postgres,
                "postgres://db.local/app?sslmode=verify-full",
                "postgres://db.local/app",
            ),
            (
                MonitorType::Mysql,
                "mysql://db.local/app",
                "mysql://db.local/app?sslmode=disable",
            ),
            (
                MonitorType::SqlServer,
                "sqlserver://db.local",
                "sqlserver://db.local?encrypt=disable",
            ),
            (
                MonitorType::SqlServer,
                "sqlserver://db.local?encrypt=true",
                "sqlserver://db.local?encrypt=false",
            ),
            (
                MonitorType::SqlServer,
                "sqlserver://db.local?encrypt=true&trustServerCertificate=false",
                "sqlserver://db.local?encrypt=true",
            ),
            (
                MonitorType::Mongodb,
                "mongodb://m.local/?tls=true",
                "mongodb://m.local/",
            ),
            (
                MonitorType::Mongodb,
                "mongodb://m.local/?tls=true",
                "mongodb://m.local/?tls=true&tlsAllowInvalidCertificates=true",
            ),
            (
                MonitorType::Mongodb,
                "mongodb://a.local,b.local",
                "mongodb://a.local,evil.example",
            ),
            (
                MonitorType::Mongodb,
                "mongodb://a.local,b.local",
                "mongodb://a.local",
            ),
            (
                MonitorType::Mongodb,
                "mongodb+srv://c.example.net",
                "mongodb://c.example.net/?tls=true",
            ),
        ] {
            let base = stored(kind, from);
            let err = submit(&base, kind.as_str(), to).unwrap_err();
            assert_eq!(
                err.get("basic_auth_pass").map(String::as_str),
                Some(crate::i18n::t("validation.secret_reenter")),
                "{from} -> {to}"
            );
            // The same protection keeps the stored secret.
            let kept = submit(&base, kind.as_str(), from).unwrap().monitor;
            assert_eq!(kept.basic_auth_pass.as_deref(), Some("hunter2"), "{from}");
        }
        // More protection keeps it too.
        for (kind, from, to) in [
            (
                MonitorType::Redis,
                "redis://cache.local:6380",
                "rediss://cache.local:6380",
            ),
            (
                MonitorType::Postgres,
                "postgres://db.local/app",
                "postgres://db.local/app?sslmode=verify-full",
            ),
            (
                MonitorType::SqlServer,
                "sqlserver://db.local?encrypt=false",
                "sqlserver://db.local/app",
            ),
            (
                MonitorType::Mongodb,
                "mongodb://A.local,b.local/app",
                "mongodb://a.local:27017,b.local/other?tls=true",
            ),
        ] {
            let kept = submit(&stored(kind, from), kind.as_str(), to)
                .unwrap()
                .monitor;
            assert_eq!(
                kept.basic_auth_pass.as_deref(),
                Some("hunter2"),
                "{from} -> {to}"
            );
        }
    }

    #[test]
    fn auth_fields_of_other_monitor_types_are_left_alone() {
        let stored = stored_oauth();
        let ok = submit_with(&stored, base_pairs("tcp", "db:5432"))
            .unwrap()
            .monitor;
        assert_eq!(ok.auth_method, "oauth2");
        assert_eq!(ok.oauth_client_secret.as_deref(), Some("the-client-secret"));
    }

    type TestPki = crate::monitor::checkers::http_auth::test_pki::Pki;

    fn mtls_pairs(
        pki: &TestPki,
        key: &str,
        target: &str,
        proxy: &str,
    ) -> Vec<(&'static str, String)> {
        let mut list = base_pairs("http", target);
        // Browsers post textareas with CRLF line endings.
        for (k, v) in [
            ("auth_method", "mtls"),
            (
                "tls_client_cert",
                &pki.client_cert_pem.replace('\n', "\r\n"),
            ),
            ("tls_client_key", key),
            ("tls_ca_cert", &pki.ca_pem.replace('\n', "\r\n")),
            ("proxy_url", proxy),
        ] {
            list = with(list, k, v);
        }
        list
    }

    const API: &str = "https://api.example.com/health";

    #[test]
    fn mtls_material_is_validated_with_messages_on_the_field() {
        use crate::monitor::checkers::http_auth::test_pki::unrelated_key_pem;
        let pki = TestPki::new();
        let crlf_key = pki.client_key_pem.replace('\n', "\r\n");
        let ok = submit_with(&NewMonitor::default(), mtls_pairs(&pki, &crlf_key, API, ""))
            .unwrap()
            .monitor;
        assert_eq!(ok.auth_method, "mtls");
        assert_eq!(
            ok.tls_client_cert.as_deref(),
            Some(pki.client_cert_pem.trim())
        );
        assert_eq!(
            ok.tls_client_key.as_deref(),
            Some(pki.client_key_pem.trim())
        );
        assert_eq!(ok.tls_ca_cert.as_deref(), Some(pki.ca_pem.trim()));
        assert!(!ok.tls_client_cert.unwrap().contains('\r'));

        let good = || mtls_pairs(&pki, &pki.client_key_pem, API, "");
        let err = submit_with(
            &NewMonitor::default(),
            mtls_pairs(&pki, &unrelated_key_pem(), API, ""),
        )
        .unwrap_err();
        assert!(
            err["tls_client_key"].contains("can’t be used together"),
            "{err:?}"
        );
        let err = submit_with(
            &NewMonitor::default(),
            with(good(), "tls_client_cert", "hello"),
        )
        .unwrap_err();
        assert!(
            err["tls_client_cert"].contains("BEGIN CERTIFICATE"),
            "{err:?}"
        );
        let err =
            submit_with(&NewMonitor::default(), with(good(), "tls_client_key", "")).unwrap_err();
        assert!(err.contains_key("tls_client_key"));
        let encrypted = "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA";
        let err = submit_with(
            &NewMonitor::default(),
            with(good(), "tls_client_key", encrypted),
        )
        .unwrap_err();
        assert!(err["tls_client_key"].contains("passphrase"), "{err:?}");
        let err =
            submit_with(&NewMonitor::default(), with(good(), "tls_ca_cert", "nope")).unwrap_err();
        assert!(err.contains_key("tls_ca_cert"));
        // The CA is optional.
        assert!(submit_with(&NewMonitor::default(), with(good(), "tls_ca_cert", "")).is_ok());
        // Client certificates need https.
        let http = mtls_pairs(&pki, &pki.client_key_pem, "http://api.example.com/", "");
        assert!(
            submit_with(&NewMonitor::default(), http)
                .unwrap_err()
                .contains_key("auth_method")
        );
    }

    #[test]
    fn stored_private_key_is_masked_and_only_kept_for_the_same_destination() {
        let pki = TestPki::new();
        let stored = NewMonitor {
            target: API.into(),
            auth_method: "mtls".into(),
            tls_client_cert: Some(pki.client_cert_pem.trim().into()),
            tls_client_key: Some(pki.client_key_pem.trim().into()),
            ..NewMonitor::default()
        };
        let shown = FormValues::from_new(&stored, Vec::new(), &[]).masked();
        assert_eq!(shown.tls_client_key, SECRET_MASK);
        assert!(
            shown.tls_client_cert.contains("BEGIN CERTIFICATE"),
            "the certificate is public and stays visible"
        );
        assert!(!format!("{shown:?}").contains("PRIVATE KEY"));

        let kept = submit_with(
            &stored,
            mtls_pairs(&pki, SECRET_MASK, "https://API.example.com:443/other", ""),
        )
        .unwrap();
        assert_eq!(
            kept.monitor.tls_client_key.as_deref(),
            Some(pki.client_key_pem.trim())
        );
        for (target, proxy) in [
            ("https://attacker.example/", ""),
            (API, "http://proxy.evil:3128"),
        ] {
            let err =
                submit_with(&stored, mtls_pairs(&pki, SECRET_MASK, target, proxy)).unwrap_err();
            assert!(
                err.contains_key("tls_client_key"),
                "{target} {proxy}: {err:?}"
            );
        }
        let err = submit_with(
            &NewMonitor::default(),
            mtls_pairs(&pki, SECRET_MASK, API, ""),
        )
        .unwrap_err();
        assert!(err.contains_key("tls_client_key"));
        let typed = submit_with(
            &stored,
            mtls_pairs(&pki, &pki.client_key_pem, "https://attacker.example/", ""),
        )
        .unwrap();
        assert_eq!(
            typed.monitor.tls_client_key.as_deref(),
            Some(pki.client_key_pem.trim())
        );
        // A rejected re-render doesn't put a typed key back into the page.
        let p = Pairs(
            mtls_pairs(&pki, &pki.client_key_pem, API, "")
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        );
        assert_eq!(
            FormValues::from_pairs(&p, &[])
                .without_typed_secrets()
                .tls_client_key,
            ""
        );
    }

    #[test]
    fn every_auth_method_round_trips_through_the_form() {
        let pki = TestPki::new();
        let cases = [
            NewMonitor {
                target: "https://x.io".into(),
                ..NewMonitor::default()
            },
            NewMonitor {
                target: "https://x.io".into(),
                auth_method: "basic".into(),
                basic_auth_user: Some("bot".into()),
                basic_auth_pass: Some("pw".into()),
                ..NewMonitor::default()
            },
            NewMonitor {
                oauth_scope: Some("a b".into()),
                oauth_audience: Some("aud".into()),
                oauth_client_auth: "client_secret_post".into(),
                ..stored_oauth()
            },
            NewMonitor {
                target: "https://x.io".into(),
                auth_method: "mtls".into(),
                tls_client_cert: Some(pki.client_cert_pem.trim().into()),
                tls_client_key: Some(pki.client_key_pem.trim().into()),
                tls_ca_cert: Some(pki.ca_pem.trim().into()),
                ..NewMonitor::default()
            },
        ];
        for stored in cases {
            // Edit without touching anything: what the browser posts back.
            let mut v = FormValues::from_new(&stored, Vec::new(), &[]).masked();
            v.name = "Website".into();
            let out = validate(&v, &stored, &FormContext::default())
                .unwrap()
                .monitor;
            let (before, after) = (
                serde_json::to_value(&stored).unwrap(),
                serde_json::to_value(&out).unwrap(),
            );
            for field in [
                "auth_method",
                "basic_auth_user",
                "basic_auth_pass",
                "oauth_token_url",
                "oauth_client_id",
                "oauth_client_secret",
                "oauth_scope",
                "oauth_audience",
                "oauth_client_auth",
                "tls_client_cert",
                "tls_client_key",
                "tls_ca_cert",
            ] {
                assert_eq!(
                    after[field], before[field],
                    "{} / {field}",
                    stored.auth_method
                );
            }
        }
    }
}
