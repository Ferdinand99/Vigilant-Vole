//! Monitor pages: create / edit / clone / detail / delete, pause & resume,
//! check now, push-token regeneration and bulk actions. Owned by the
//! monitors agent.
//!
//! - `form` parses and validates the big monitor form (pure, unit-tested).
//! - `view` builds the SVG chart, the 90-day bar and formats numbers/times.

pub mod bulk_edit;
pub mod form;
pub mod view;

use crate::i18n::Tr as _;
use std::fmt::Write as _;

use askama::Template;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use rusqlite::Connection;
use serde::Deserialize;

use self::form::{ChannelMode, Errors, FormContext, FormValues, Pairs, Validated};
use self::view::{Chart, Range, UptimeDay};
use super::dashboard::{DashQuery, build_dashboard};
use super::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::{CurrentUser, Role};
use crate::db::events::{self as db_events, MonitorEvent, MonitorEventKind};
use crate::db::heartbeats::{self as db_heartbeats, AgentResult};
use crate::db::models::{Heartbeat, HeartbeatStatus, Monitor, MonitorType, NewMonitor};
use crate::db::monitors::{self as db_monitors, ExpiryRecord};
use crate::db::tags::{self as db_tags, MonitorTag, Tag};
use crate::db::{notifications as db_notifications, rollups, settings as db_settings};
use crate::monitor::checkers::{AUTH_METHODS, OAUTH_CLIENT_AUTHS};
use crate::monitor::checkers::{BODY_ENCODINGS, DNS_RECORD_TYPES, HTTP_METHODS, JSON_OPERATORS};

/// Heartbeats per page in the detail page's table.
const HEARTBEAT_PAGE: i64 = 25;
/// Timeline entries on the detail page.
const EVENT_LIMIT: i64 = 30;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/monitors/new", get(new_form).post(create))
        .route("/monitors/form-fields", get(form_fields))
        .route("/monitors/bulk", post(bulk))
        .route("/monitors/bulk/edit", post(bulk_edit))
        .route("/monitors/bulk/edit/summary", post(bulk_edit_summary))
        .route("/monitors/{id}", get(detail).delete(delete))
        .route("/monitors/{id}/edit", get(edit_form).post(update))
        .route("/monitors/{id}/clone", get(clone_form))
        .route("/monitors/{id}/pause", post(pause))
        .route("/monitors/{id}/resume", post(resume))
        .route("/monitors/{id}/check", post(check_now))
        .route("/monitors/{id}/push-token", post(regenerate_push_token))
        .route("/monitors/{id}/chart", get(chart_partial))
        .route("/monitors/{id}/heartbeats", get(heartbeats_partial))
}

/// `HX-Trigger` value that shows a toast. Non-ASCII is `\u`-escaped so the
/// header stays valid (curly quotes, emoji in names, …).
pub fn toast_header(message: &str, kind: &str) -> Option<HeaderValue> {
    trigger_header(&serde_json::json!({ "vv:toast": { "message": message, "kind": kind } }))
}

/// `HX-Trigger` value for any set of events (escaped like [`toast_header`]).
pub fn trigger_header(events: &serde_json::Value) -> Option<HeaderValue> {
    let json = events.to_string();
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() && !c.is_ascii_control() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    HeaderValue::from_str(&out).ok()
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.get("hx-request").and_then(|v| v.to_str().ok()) == Some("true")
}

/// Local redirect target from a form field: must be a plain same-site path
/// (the login `?next=` rules); anything else falls back to `fallback`.
fn safe_return(to: &str, fallback: &str) -> String {
    let next = crate::auth::session::sanitize_next(Some(to));
    if next == "/" && to.trim() != "/" {
        fallback.to_string()
    } else {
        next
    }
}

/// `{base}/api/push/{token}?status=up&msg=OK&ping=`. The base is the
/// configured base URL, or the request's own host.
fn push_url(base_url: Option<&str>, headers: &HeaderMap, token: &str) -> String {
    let base = match base_url.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => b.trim_end_matches('/').to_string(),
        None => {
            let host = headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .filter(|h| {
                    !h.is_empty()
                        && h.chars()
                            .all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))
                })
                .unwrap_or("localhost:3001");
            let scheme = headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .filter(|p| *p == "https")
                .unwrap_or("http");
            format!("{scheme}://{host}")
        }
    };
    format!("{base}/api/push/{token}?status=up&msg=OK&ping=")
}

// ===========================================================================
// Form
// ===========================================================================

/// One radio card in the type picker.
pub struct TypeChoice {
    pub value: &'static str,
    pub label: &'static str,
    pub sub: &'static str,
    pub icon: &'static str,
}

fn type_choices() -> Vec<TypeChoice> {
    MonitorType::all()
        .iter()
        .map(|t| {
            let (sub, icon) = match t {
                MonitorType::Http => (crate::i18n::t("monitor_type.sub_http"), "globe"),
                MonitorType::Keyword => (crate::i18n::t("monitor_type.sub_keyword"), "search"),
                MonitorType::JsonQuery => (crate::i18n::t("monitor_type.sub_json_query"), "code"),
                MonitorType::Tcp => (crate::i18n::t("monitor_type.sub_tcp"), "plug"),
                MonitorType::Ping => (crate::i18n::t("monitor_type.sub_ping"), "pulse"),
                MonitorType::TailscalePing => {
                    (crate::i18n::t("monitor_type.sub_tailscale_ping"), "link")
                }
                MonitorType::Dns => (crate::i18n::t("monitor_type.sub_dns"), "dns"),
                MonitorType::Push => (crate::i18n::t("monitor_type.sub_push"), "bolt"),
                MonitorType::Docker => (crate::i18n::t("monitor_type.sub_docker"), "box"),
                MonitorType::Websocket => (crate::i18n::t("monitor_type.sub_websocket"), "link"),
                MonitorType::DomainExpiry => {
                    (crate::i18n::t("monitor_type.sub_domain_expiry"), "calendar")
                }
                MonitorType::Ntp => (crate::i18n::t("monitor_type.sub_ntp"), "clock"),
                MonitorType::Steam => (crate::i18n::t("monitor_type.sub_steam"), "server"),
                MonitorType::GameServer => {
                    (crate::i18n::t("monitor_type.sub_game_server"), "gamepad")
                }
                MonitorType::Tls => (crate::i18n::t("monitor_type.sub_tls"), "lock"),
                MonitorType::Smtp => (crate::i18n::t("monitor_type.sub_smtp"), "mail"),
                MonitorType::Redis => (crate::i18n::t("monitor_type.sub_redis"), "database"),
                MonitorType::Postgres => (crate::i18n::t("monitor_type.sub_postgres"), "database"),
                MonitorType::Mysql => (crate::i18n::t("monitor_type.sub_mysql"), "database"),
                MonitorType::Mqtt => (crate::i18n::t("monitor_type.sub_mqtt"), "signal"),
                MonitorType::Kafka => (crate::i18n::t("monitor_type.sub_kafka"), "queue"),
                MonitorType::Rabbitmq => (crate::i18n::t("monitor_type.sub_rabbitmq"), "queue"),
                MonitorType::Grpc => (crate::i18n::t("monitor_type.sub_grpc"), "bolt"),
                MonitorType::GrpcKeyword => {
                    (crate::i18n::t("monitor_type.sub_grpc_keyword"), "search")
                }
                MonitorType::Radius => (crate::i18n::t("monitor_type.sub_radius"), "key"),
                MonitorType::Snmp => (crate::i18n::t("monitor_type.sub_snmp"), "chart"),
                MonitorType::SqlServer => {
                    (crate::i18n::t("monitor_type.sub_sqlserver"), "database")
                }
                MonitorType::Mongodb => (crate::i18n::t("monitor_type.sub_mongodb"), "database"),
                MonitorType::RealBrowser => {
                    (crate::i18n::t("monitor_type.sub_real_browser"), "browser")
                }
                MonitorType::Group => (crate::i18n::t("monitor_type.sub_group"), "dashboard"),
            };
            TypeChoice {
                value: t.as_str(),
                label: t.label(),
                sub,
                icon,
            }
        })
        .collect()
}

/// One flavour of the target input (see `form::target_field`).
pub struct TargetSpec {
    pub field: &'static str,
    /// Comma-separated monitor types for `data-show-when`.
    pub types: &'static str,
    pub label: &'static str,
    pub placeholder: &'static str,
    pub help: &'static str,
    pub url: bool,
}

impl TargetSpec {
    /// Label in the current language (`target.<field>.label`).
    pub fn tr_label(&self) -> &'static str {
        crate::i18n::t_or(&format!("target.{}.label", self.field), self.label)
    }
    pub fn tr_help(&self) -> &'static str {
        crate::i18n::t_or(&format!("target.{}.help", self.field), self.help)
    }
    pub fn tr_placeholder(&self) -> &'static str {
        crate::i18n::t_or(
            &format!("target.{}.placeholder", self.field),
            self.placeholder,
        )
    }
}

const TARGETS: &[TargetSpec] = &[
    TargetSpec {
        field: "target_url",
        types: "http,keyword,json_query",
        label: "URL",
        placeholder: "https://example.com/health…",
        help: "The full address, including https://.",
        url: true,
    },
    TargetSpec {
        field: "target_ws",
        types: "websocket",
        label: "URL",
        placeholder: "wss://example.com/socket…",
        help: "A ws:// or wss:// address. http(s):// works too.",
        url: true,
    },
    TargetSpec {
        field: "target_hostport",
        types: "tcp",
        label: "Host and port",
        placeholder: "db.internal:5432…",
        help: "Wrap IPv6 addresses in brackets, like [2001:db8::1]:443.",
        url: false,
    },
    TargetSpec {
        field: "target_host",
        types: "ping",
        label: "Host",
        placeholder: "example.com…",
        help: "A host name or IP address.",
        url: false,
    },
    TargetSpec {
        field: "target_tailscale",
        types: "tailscale_ping",
        label: "Tailscale peer",
        placeholder: "nas…",
        help: "The peer’s machine name (as in the Tailscale admin console), its MagicDNS name or a Tailscale IP like 100.64.0.5.",
        url: false,
    },
    TargetSpec {
        field: "target_dns",
        types: "dns",
        label: "Name to look up",
        placeholder: "example.com…",
        help: "For PTR records, enter the IP address.",
        url: false,
    },
    TargetSpec {
        field: "target_docker",
        types: "docker",
        label: "Container",
        placeholder: "postgres…",
        help: "The container’s name or ID.",
        url: false,
    },
    TargetSpec {
        field: "target_domain",
        types: "domain_expiry",
        label: "Domain",
        placeholder: "example.com…",
        help: "Subdomains and URLs are reduced to the registered domain.",
        url: false,
    },
    TargetSpec {
        field: "target_ntp",
        types: "ntp",
        label: "Server",
        placeholder: "pool.ntp.org…",
        help: "Uses port 123 unless you add one.",
        url: false,
    },
    TargetSpec {
        field: "target_steam",
        types: "steam",
        label: "Server",
        placeholder: "203.0.113.5:27015…",
        help: "Uses port 27015 unless you add one.",
        url: false,
    },
    TargetSpec {
        field: "target_game",
        types: "game_server",
        label: "Game server",
        placeholder: "minecraft://mc.example.com…",
        help: "Start with the game: minecraft:// (port 25565; without a port the _minecraft._tcp SRV record is tried first), bedrock:// (19132), satisfactory:// (7777), fivem:// (30120, also RedM) or mumble:// (64738).",
        url: false,
    },
    TargetSpec {
        field: "target_tls",
        types: "tls",
        label: "Server",
        placeholder: "example.com:443…",
        help: "Uses port 443 unless you add one.",
        url: false,
    },
    TargetSpec {
        field: "target_smtp",
        types: "smtp",
        label: "Mail server",
        placeholder: "mail.example.com:587…",
        help: "Uses port 25 unless you add one. Port 465 is treated as implicit TLS.",
        url: false,
    },
    TargetSpec {
        field: "target_redis",
        types: "redis",
        label: "Redis server",
        placeholder: "cache.local:6379…",
        help: "Uses port 6379. Add rediss:// for TLS and /2 to pick a database.",
        url: false,
    },
    TargetSpec {
        field: "target_postgres",
        types: "postgres",
        label: "PostgreSQL server",
        placeholder: "db.local:5432/app…",
        help: "Uses port 5432. The path is the database name. TLS is used when the server offers it; add ?sslmode=verify-full to also check the certificate (or disable, require, verify-ca).",
        url: false,
    },
    TargetSpec {
        field: "target_mysql",
        types: "mysql",
        label: "MySQL or MariaDB server",
        placeholder: "db.local:3306…",
        help: "Uses port 3306. Without a user name only the server greeting is checked. TLS is used when the server offers it; add ?sslmode=verify-full to also check the certificate.",
        url: false,
    },
    TargetSpec {
        field: "target_mqtt",
        types: "mqtt",
        label: "MQTT broker",
        placeholder: "broker.local:1883/home/status…",
        help: "Uses port 1883 (8883 for mqtts://). Add a topic as the path to subscribe to it; wildcards (+ and #) work as written.",
        url: false,
    },
    TargetSpec {
        field: "target_kafka",
        types: "kafka",
        label: "Kafka broker",
        placeholder: "broker.local:9092/orders…",
        help: "Uses port 9092. Add a topic as the path to check that it exists.",
        url: false,
    },
    TargetSpec {
        field: "target_rabbitmq",
        types: "rabbitmq",
        label: "RabbitMQ broker",
        placeholder: "mq.local:5672/vhost…",
        help: "Uses port 5672 (5671 for amqps://). The path is the virtual host; the default is /.",
        url: false,
    },
    TargetSpec {
        field: "target_grpc",
        types: "grpc",
        label: "gRPC server",
        placeholder: "api.local:50051/my.Service…",
        help: "Uses port 50051 (443 for grpcs://). The path is the service name for the standard health check; leave it empty for the whole server.",
        url: false,
    },
    TargetSpec {
        field: "target_grpc_keyword",
        types: "grpc_keyword",
        label: "gRPC method",
        placeholder: "grpc://api.local:50051/my.pkg.Greeter/SayHello…",
        help: "Uses port 50051 (443 for grpcs://, which also checks the certificate). The path is the service and the unary method to call; a short service name works when it is unique.",
        url: false,
    },
    TargetSpec {
        field: "target_radius",
        types: "radius",
        label: "RADIUS server",
        placeholder: "radius.local:1812…",
        help: "Uses port 1812.",
        url: false,
    },
    TargetSpec {
        field: "target_snmp",
        types: "snmp",
        label: "SNMP device",
        placeholder: "router.local:161/1.3.6.1.2.1.1.3.0…",
        help: "Uses port 161. The path is the OID to read; the default is sysUpTime (1.3.6.1.2.1.1.3.0).",
        url: false,
    },
    TargetSpec {
        field: "target_sqlserver",
        types: "sqlserver",
        label: "SQL Server",
        placeholder: "sqlserver://db.local:1433/app…",
        help: "Uses port 1433. The path is the database. Without a user name only the pre-login is checked. Encrypted when the server can; add ?encrypt=true&trustServerCertificate=false to also check the certificate (or encrypt=false, disable, strict).",
        url: false,
    },
    TargetSpec {
        field: "target_mongodb",
        types: "mongodb",
        label: "MongoDB server",
        placeholder: "mongodb://db1.local,db2.local/?replicaSet=rs0…",
        help: "Uses port 27017. List several hosts with commas, or use mongodb+srv://. Add ?tls=true for TLS (the certificate is checked; tlsAllowInvalidCertificates=true skips that) and ?replicaSet= to require a primary. The path or authSource is the authentication database (default admin).",
        url: false,
    },
    TargetSpec {
        field: "target_browser",
        types: "real_browser",
        label: "Page URL",
        placeholder: "https://shop.example.com/…",
        help: "The page the remote browser opens, including https://.",
        url: true,
    },
];

/// Palette colours as `(value, "Label")` for colour pickers.
pub fn palette_options() -> Vec<(&'static str, String)> {
    db_tags::PALETTE
        .iter()
        .map(|c| {
            let mut fallback = c.to_string();
            if let Some(first) = fallback.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            (
                *c,
                crate::i18n::t_or(&format!("color.{c}"), &fallback).to_string(),
            )
        })
        .collect()
}

/// A notification channel row in the form.
pub struct ChannelOption {
    pub id: i64,
    pub name: String,
    pub kind: String,
    /// Provider name for display ("Discord"), falling back to the raw kind.
    pub kind_label: String,
    pub active: bool,
}

/// Everything the form templates need (shared by the page and the
/// `/monitors/form-fields` partial).
pub struct FormData {
    pub heading: String,
    pub action: String,
    pub cancel_href: String,
    pub submit_label: &'static str,
    pub is_edit: bool,
    pub v: FormValues,
    pub errors: Errors,
    /// Callout text above the form when there are errors.
    pub summary: String,
    pub types: Vec<TypeChoice>,
    pub targets: &'static [TargetSpec],
    pub groups: Vec<(i64, String)>,
    pub channels: Vec<ChannelOption>,
    pub tags: Vec<Tag>,
    pub palette: Vec<(&'static str, String)>,
    pub methods: Vec<(&'static str, &'static str)>,
    pub encodings: Vec<(&'static str, &'static str)>,
    pub operators: Vec<(&'static str, &'static str)>,
    pub record_types: Vec<(&'static str, &'static str)>,
    pub header_rows: Vec<(String, String)>,
    pub push_url: Option<String>,
    pub has_agents: bool,
    /// "Check from": `(value, label)`, this server first, then the enabled
    /// agents (and the chosen one, even when it is disabled).
    pub check_from_choices: Vec<(String, String)>,
    /// `(value, label)` for the authentication method select.
    pub auth_methods: Vec<(&'static str, String)>,
    pub oauth_client_auths: Vec<(&'static str, String)>,
    /// Real browser: `(id, name)` of every remote browser (the first is the default).
    pub remote_browsers: Vec<(String, String)>,
    pub browser_waits: Vec<(&'static str, String)>,
    pub browser_viewports: Vec<(&'static str, String)>,
}

impl FormData {
    /// Error for a field, or "".
    pub fn err(&self, field: &str) -> &str {
        self.errors.get(field).map(String::as_str).unwrap_or("")
    }

    pub fn has_err(&self, field: &str) -> bool {
        self.errors.contains_key(field)
    }

    /// Whether a `data-show-when` block for `types` is visible for the current type.
    pub fn shows(&self, types: &str) -> bool {
        types.split(',').any(|t| t == self.v.monitor_type)
    }

    /// Whether the chosen authentication method is `method` (for pre-hiding
    /// the `data-show-when="auth_method=…"` blocks).
    pub fn auth_is(&self, method: &str) -> bool {
        self.v.auth_method == method
    }

    /// Whether any authentication field has an error (keeps the section open).
    pub fn has_auth_err(&self) -> bool {
        [
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
        ]
        .iter()
        .any(|f| self.errors.contains_key(f))
    }

    pub fn is_type(&self, t: &str) -> bool {
        self.v.monitor_type == t
    }

    /// Whether an agent checks the monitor instead of this server.
    pub fn agent_checked(&self) -> bool {
        !self.v.check_from.trim().is_empty()
    }

    /// The game picker of a game server monitor: (scheme, name, default
    /// port, selected), the target's game selected.
    pub fn game_presets(&self) -> Vec<(&'static str, &'static str, u16, bool)> {
        use crate::monitor::checkers::game_server::Game;
        let current = self
            .v
            .target
            .trim()
            .split_once("://")
            .and_then(|(scheme, _)| Game::from_scheme(scheme))
            .unwrap_or(Game::Minecraft);
        Game::ALL
            .iter()
            .map(|g| (g.scheme(), g.label(), g.default_port(), *g == current))
            .collect()
    }

    pub fn tag_checked(&self, id: &i64) -> bool {
        self.v.tag_value(*id).is_some()
    }

    pub fn tag_value(&self, id: &i64) -> &str {
        self.v.tag_value(*id).unwrap_or("")
    }

    pub fn channel_mode(&self, id: &i64) -> &'static str {
        self.v.channel_mode(*id).as_str()
    }

    pub fn parent_selected(&self, id: &i64) -> bool {
        self.v.parent_id == id.to_string()
    }
}

#[derive(Template)]
#[template(path = "monitor_form.html")]
struct MonitorFormTemplate {
    ctx: PageCtx,
    f: FormData,
}

#[derive(Template)]
#[template(path = "monitor_form_fields.html")]
struct FormFieldsTemplate {
    f: FormData,
}

/// Database-backed context for one form render/validation.
struct FormEnv {
    cx: FormContext,
    groups: Vec<(i64, String)>,
    channels: Vec<ChannelOption>,
    tags: Vec<Tag>,
    has_agents: bool,
    /// `(id, name, active)` of every agent.
    agents: Vec<(i64, String, bool)>,
    base_url: Option<String>,
    agent_confirmation_default: bool,
    remote_browsers: Vec<(i64, String)>,
}

/// Every notification channel, for the channel pickers.
fn channel_options(conn: &Connection) -> rusqlite::Result<Vec<ChannelOption>> {
    Ok(db_notifications::list_channels(conn)?
        .into_iter()
        .map(|c| ChannelOption {
            id: c.id,
            name: c.name,
            kind_label: crate::notification::provider(&c.kind)
                .map_or_else(|| c.kind.clone(), |p| p.tr_label().to_string()),
            kind: c.kind,
            active: c.active,
        })
        .collect())
}

fn load_env(
    conn: &Connection,
    editing: Option<&Monitor>,
    admin: bool,
) -> rusqlite::Result<FormEnv> {
    let excluded: Vec<i64> = match editing {
        Some(m) => {
            let mut ids = db_monitors::descendant_ids(conn, m.id)?;
            ids.push(m.id);
            ids
        }
        None => Vec::new(),
    };
    let groups: Vec<(i64, String)> = db_monitors::list_monitors(conn)?
        .into_iter()
        .filter(|m| m.monitor_type == MonitorType::Group && !excluded.contains(&m.id))
        .map(|m| (m.id, m.name))
        .collect();
    let channels = channel_options(conn)?;
    let tags = db_tags::list_tags(conn)?;
    let agents = crate::db::agents::list_agent_names(conn)?;
    let has_agents = !agents.is_empty();
    let child_count = match editing {
        Some(m) => db_monitors::children(conn, m.id)?.len(),
        None => 0,
    };
    let settings = db_settings::load(conn)?;
    let remote_browsers: Vec<(i64, String)> = crate::db::remote_browsers::list(conn)?
        .into_iter()
        .map(|b| (b.id, b.name))
        .collect();
    Ok(FormEnv {
        cx: FormContext {
            editing_id: editing.map(|m| m.id),
            allowed_parents: groups.iter().map(|(id, _)| *id).collect(),
            channel_ids: channels.iter().map(|c| c.id).collect(),
            tag_ids: tags.iter().map(|t| t.id).collect(),
            child_count,
            remote_browser_ids: remote_browsers.iter().map(|(id, _)| *id).collect(),
            browser_optional: false,
            agent_ids: agents.iter().map(|(id, _, _)| *id).collect(),
            admin,
        },
        agents,
        remote_browsers,
        groups,
        channels,
        tags,
        has_agents,
        base_url: settings.base_url,
        agent_confirmation_default: settings.agent_confirmation_default,
    })
}

struct FormMeta {
    heading: String,
    action: String,
    cancel_href: String,
    submit_label: &'static str,
    is_edit: bool,
    push_token: Option<String>,
}

fn form_data(
    env: FormEnv,
    meta: FormMeta,
    v: FormValues,
    errors: Errors,
    headers: &HeaderMap,
) -> FormData {
    let v = v.without_typed_secrets();
    let header_rows = v.header_rows();
    let summary = match errors.len() {
        0 => String::new(),
        n => crate::i18n::tn("monitor_form.fix_fields", n as i64),
    };
    FormData {
        summary,
        heading: meta.heading,
        action: meta.action,
        cancel_href: meta.cancel_href,
        submit_label: meta.submit_label,
        is_edit: meta.is_edit,
        header_rows,
        errors,
        types: type_choices(),
        targets: TARGETS,
        groups: env.groups,
        channels: env.channels,
        tags: env.tags,
        palette: palette_options(),
        methods: HTTP_METHODS.iter().map(|m| (*m, *m)).collect(),
        encodings: BODY_ENCODINGS
            .iter()
            .map(|e| {
                let label = match *e {
                    "json" => "JSON",
                    "form" => crate::i18n::t("monitor_form.enc_form"),
                    "raw" => crate::i18n::t("monitor_form.enc_raw"),
                    other => other,
                };
                (*e, label)
            })
            .collect(),
        operators: JSON_OPERATORS
            .iter()
            .map(|o| {
                let label = match *o {
                    "==" => crate::i18n::t("monitor_form.op_eq"),
                    "!=" => crate::i18n::t("monitor_form.op_ne"),
                    "contains" => crate::i18n::t("monitor_form.op_contains"),
                    "<" => crate::i18n::t("monitor_form.op_lt"),
                    "<=" => crate::i18n::t("monitor_form.op_le"),
                    ">" => crate::i18n::t("monitor_form.op_gt"),
                    ">=" => crate::i18n::t("monitor_form.op_ge"),
                    other => other,
                };
                (*o, label)
            })
            .collect(),
        record_types: DNS_RECORD_TYPES.iter().map(|r| (*r, *r)).collect(),
        push_url: meta
            .push_token
            .as_deref()
            .map(|t| push_url(env.base_url.as_deref(), headers, t)),
        has_agents: env.has_agents,
        check_from_choices: std::iter::once((
            String::new(),
            crate::i18n::t("form.check_from_server").to_string(),
        ))
        .chain(
            env.agents
                .iter()
                .filter(|(id, _, active)| *active || v.check_from == id.to_string())
                .map(|(id, name, active)| {
                    let label = if *active {
                        name.clone()
                    } else {
                        crate::i18n::tf("form.check_from_disabled", &[("name", name)])
                    };
                    (id.to_string(), label)
                }),
        )
        .collect(),
        auth_methods: AUTH_METHODS
            .iter()
            .map(|m| {
                (
                    *m,
                    crate::i18n::t_or(&format!("httpauth.{m}"), m).to_string(),
                )
            })
            .collect(),
        oauth_client_auths: OAUTH_CLIENT_AUTHS
            .iter()
            .map(|m| {
                (
                    *m,
                    crate::i18n::t_or(&format!("httpauth.{m}"), m).to_string(),
                )
            })
            .collect(),
        remote_browsers: env
            .remote_browsers
            .iter()
            .map(|(id, name)| (id.to_string(), name.clone()))
            .collect(),
        browser_waits: crate::monitor::checkers::real_browser::WAITS
            .iter()
            .map(|w| {
                (
                    *w,
                    crate::i18n::t_or(&format!("browser.wait_{w}"), w).to_string(),
                )
            })
            .collect(),
        browser_viewports: crate::monitor::checkers::real_browser::VIEWPORTS
            .iter()
            .map(|w| {
                (
                    *w,
                    crate::i18n::t_or(&format!("browser.viewport_{w}"), w).to_string(),
                )
            })
            .collect(),
        v,
    }
}

fn channel_modes(env: &FormEnv, links: &[(i64, bool)]) -> Vec<(i64, ChannelMode)> {
    env.channels
        .iter()
        .map(|c| {
            let mode = match links.iter().find(|(id, _)| *id == c.id) {
                Some((_, true)) => ChannelMode::Escalation,
                Some((_, false)) => ChannelMode::Alert,
                None => ChannelMode::Off,
            };
            (c.id, mode)
        })
        .collect()
}

#[derive(Deserialize, Default)]
struct NewParams {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    parent: Option<i64>,
}

async fn new_form(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Query(params): Query<NewParams>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let admin = user.is_admin();
    let (env, defaults) = state
        .with_conn(move |c| {
            let env = load_env(c, None, admin)?;
            // New monitors get the default channels (what insert_monitor would attach).
            let defaults: Vec<(i64, bool)> = db_notifications::default_channel_ids(c)?
                .into_iter()
                .map(|id| (id, false))
                .collect();
            Ok((env, defaults))
        })
        .await?;
    let mut base = NewMonitor {
        confirm_with_agents: env.agent_confirmation_default,
        ..NewMonitor::default()
    };
    if let Some(t) = MonitorType::parse(&params.kind) {
        base.monitor_type = t;
    }
    if let Some(p) = params.parent.filter(|p| env.cx.allowed_parents.contains(p)) {
        base.parent_id = Some(p);
    }
    let v = FormValues::from_new(&base, channel_modes(&env, &defaults), &[]);
    let meta = FormMeta {
        heading: crate::i18n::t("dashboard.new_monitor").into(),
        action: "/monitors/new".into(),
        cancel_href: "/".into(),
        submit_label: crate::i18n::t("monitor_form.create"),
        is_edit: false,
        push_token: None,
    };
    let f = form_data(env, meta, v, Errors::new(), &headers);
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        crate::i18n::t("dashboard.new_monitor"),
        "dashboard",
        "",
    )
    .await;
    Ok((jar, HtmlTemplate(MonitorFormTemplate { ctx, f })))
}

/// Loads a monitor with its form context, tags and channel links.
async fn load_for_form(
    state: &AppState,
    id: i64,
    admin: bool,
) -> ApiResult<(Monitor, FormEnv, Vec<MonitorTag>, Vec<(i64, bool)>)> {
    state
        .with_conn(move |c| {
            let Some(m) = db_monitors::get_monitor(c, id)? else {
                return Ok(None);
            };
            let env = load_env(c, Some(&m), admin)?;
            let tags = db_tags::tags_for_monitor(c, id)?;
            let links = db_notifications::monitor_channel_links(c, id)?;
            Ok(Some((m, env, tags, links)))
        })
        .await?
        .ok_or(ApiError::NotFound)
}

async fn edit_form(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let (m, env, tags, links) = load_for_form(&state, id, user.is_admin()).await?;
    let v = FormValues::from_monitor(&m, channel_modes(&env, &links), &tags).masked();
    let heading = crate::i18n::tf("monitor_form.edit_heading", &[("name", &m.name)]);
    let meta = FormMeta {
        heading: heading.clone(),
        action: format!("/monitors/{id}/edit"),
        cancel_href: format!("/monitors/{id}"),
        submit_label: crate::i18n::t("monitor_form.save_changes"),
        is_edit: true,
        push_token: m
            .push_token
            .clone()
            .filter(|_| m.monitor_type == MonitorType::Push),
    };
    let f = form_data(env, meta, v, Errors::new(), &headers);
    let (ctx, jar) = PageCtx::build(&state, Some(&user), &jar, heading, "dashboard", "").await;
    Ok((jar, HtmlTemplate(MonitorFormTemplate { ctx, f })))
}

/// "Name (copy)", trimmed to the name limit.
fn copy_name(name: &str) -> String {
    let suffix = crate::i18n::t("monitor_form.copy_suffix");
    let keep = form::MAX_NAME_LEN.saturating_sub(suffix.chars().count());
    let base: String = name.chars().take(keep).collect();
    format!("{}{suffix}", base.trim_end())
}

async fn clone_form(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let admin = user.is_admin();
    let (m, _, tags, links) = load_for_form(&state, id, admin).await?;
    // A clone is a new monitor: its parent options must not exclude the source.
    let env = state.with_conn(move |c| load_env(c, None, admin)).await?;
    let mut new = m.to_new();
    new.name = copy_name(&m.name);
    new.push_token = None;
    new.source = "manual".into();
    new.external_id = None;
    let v = FormValues::from_new(&new, channel_modes(&env, &links), &tags).masked();
    let heading = crate::i18n::tf("monitor_form.clone_heading", &[("name", &m.name)]);
    let meta = FormMeta {
        heading: heading.clone(),
        action: "/monitors/new".into(),
        cancel_href: format!("/monitors/{id}"),
        submit_label: crate::i18n::t("monitor_form.create"),
        is_edit: false,
        push_token: None,
    };
    let f = form_data(env, meta, v, Errors::new(), &headers);
    let (ctx, jar) = PageCtx::build(&state, Some(&user), &jar, heading, "dashboard", "").await;
    Ok((jar, HtmlTemplate(MonitorFormTemplate { ctx, f })))
}

#[derive(Deserialize, Default)]
struct FieldsParams {
    #[serde(default, rename = "type")]
    kind: String,
}

/// htmx partial: the type-specific fields for `?type=` with default values.
async fn form_fields(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Query(params): Query<FieldsParams>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let admin = user.is_admin();
    let env = state.with_conn(move |c| load_env(c, None, admin)).await?;
    let base = NewMonitor {
        monitor_type: MonitorType::parse(&params.kind).unwrap_or_default(),
        ..NewMonitor::default()
    };
    let v = FormValues::from_new(&base, channel_modes(&env, &[]), &[]);
    let meta = FormMeta {
        heading: String::new(),
        action: String::new(),
        cancel_href: String::new(),
        submit_label: "",
        is_edit: false,
        push_token: None,
    };
    Ok(HtmlTemplate(FormFieldsTemplate {
        f: form_data(env, meta, v, Errors::new(), &headers),
    }))
}

/// Writes a validated form in one transaction; returns the monitor id.
fn persist(conn: &mut Connection, id: Option<i64>, valid: &Validated) -> rusqlite::Result<i64> {
    let tx = conn.transaction()?;
    let id = match id {
        Some(id) => {
            db_monitors::update_monitor(&tx, id, &valid.monitor)?;
            id
        }
        None => db_monitors::insert_monitor(&tx, &valid.monitor)?,
    };
    db_notifications::set_monitor_channels(&tx, id, &valid.channels)?;
    let mut tags = valid.tags.clone();
    if let Some((name, color, value)) = &valid.new_tag {
        let tag_id = db_tags::get_or_create(&tx, name, color)?;
        tags.retain(|(t, _)| *t != tag_id);
        tags.push((tag_id, value.clone()));
    }
    db_tags::set_monitor_tags(&tx, id, &tags)?;
    tx.commit()?;
    Ok(id)
}

/// Re-renders the form with inline errors (422).
async fn render_invalid(
    state: &AppState,
    user: &CurrentUser,
    jar: SignedCookieJar,
    f: FormData,
) -> Response {
    let (ctx, jar) =
        PageCtx::build(state, Some(user), &jar, f.heading.clone(), "dashboard", "").await;
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        jar,
        HtmlTemplate(MonitorFormTemplate { ctx, f }),
    )
        .into_response()
}

async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let admin = user.is_admin();
    let env = state.with_conn(move |c| load_env(c, None, admin)).await?;
    let v = FormValues::from_pairs(&Pairs(pairs), &env.cx.channel_ids);
    let base = NewMonitor::default();
    let mut valid = match form::validate(&v, &base, &env.cx) {
        Ok(valid) => valid,
        Err(errors) => {
            let meta = FormMeta {
                heading: crate::i18n::t("dashboard.new_monitor").into(),
                action: "/monitors/new".into(),
                cancel_href: "/".into(),
                submit_label: crate::i18n::t("monitor_form.create"),
                is_edit: false,
                push_token: None,
            };
            let f = form_data(env, meta, v, errors, &headers);
            return Ok(render_invalid(&state, &user, jar, f).await);
        }
    };
    if valid.monitor.monitor_type == MonitorType::Push {
        valid.monitor.push_token = Some(super::push::generate_push_token());
    }
    let name = valid.monitor.name.clone();
    let id = state.with_conn(move |c| persist(c, None, &valid)).await?;
    crate::maintenance::invalidate(&state);
    state.scheduler.reload(id).await;
    state.notify_update();
    let jar = flash(
        jar,
        "success",
        crate::i18n::tf("monitor_form.created", &[("name", &name)]),
    );
    Ok((jar, Redirect::to(&format!("/monitors/{id}"))).into_response())
}

async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let (existing, env, _, _) = load_for_form(&state, id, user.is_admin()).await?;
    let v = FormValues::from_pairs(&Pairs(pairs), &env.cx.channel_ids);
    let base = existing.to_new();
    let mut valid = match form::validate(&v, &base, &env.cx) {
        Ok(valid) => valid,
        Err(errors) => {
            let meta = FormMeta {
                heading: crate::i18n::tf("monitor_form.edit_heading", &[("name", &existing.name)]),
                action: format!("/monitors/{id}/edit"),
                cancel_href: format!("/monitors/{id}"),
                submit_label: crate::i18n::t("monitor_form.save_changes"),
                is_edit: true,
                push_token: existing
                    .push_token
                    .clone()
                    .filter(|_| existing.monitor_type == MonitorType::Push),
            };
            let f = form_data(env, meta, v, errors, &headers);
            return Ok(render_invalid(&state, &user, jar, f).await);
        }
    };
    if valid.monitor.monitor_type == MonitorType::Push && valid.monitor.push_token.is_none() {
        valid.monitor.push_token = Some(super::push::generate_push_token());
    }
    let name = valid.monitor.name.clone();
    state
        .with_conn(move |c| persist(c, Some(id), &valid))
        .await?;
    crate::maintenance::invalidate(&state);
    state.scheduler.reload(id).await;
    state.notify_update();
    let jar = flash(
        jar,
        "success",
        crate::i18n::tf("monitor_form.saved", &[("name", &name)]),
    );
    Ok((jar, Redirect::to(&format!("/monitors/{id}"))).into_response())
}

// ===========================================================================
// Actions
// ===========================================================================

async fn delete(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let name = state
        .with_conn(move |c| {
            let m = db_monitors::get_monitor(c, id)?;
            if m.is_some() {
                db_monitors::delete_monitor(c, id)?;
            }
            Ok(m.map(|m| m.name))
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::maintenance::invalidate(&state);
    state.scheduler.reload(id).await;
    state.notify_update();
    let jar = flash(
        jar,
        "success",
        crate::i18n::tf("monitor_form.deleted", &[("name", &name)]),
    );
    if is_htmx(&headers) {
        let mut res = (jar, StatusCode::OK).into_response();
        res.headers_mut()
            .insert("hx-redirect", HeaderValue::from_static("/"));
        Ok(res)
    } else {
        Ok((jar, Redirect::to("/")).into_response())
    }
}

#[derive(Deserialize, Default)]
struct ReturnForm {
    #[serde(default)]
    return_to: String,
}

async fn set_paused(state: &AppState, id: i64, paused: bool) -> ApiResult<String> {
    let name = state
        .with_conn(move |c| {
            let Some(m) = db_monitors::get_monitor(c, id)? else {
                return Ok(None);
            };
            if m.active == paused {
                db_monitors::set_active(c, id, !paused)?;
                if paused {
                    db_events::log_paused(c, id)?;
                } else {
                    db_events::log_resumed(c, id)?;
                }
            }
            Ok(Some(m.name))
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    state.scheduler.reload(id).await;
    state.notify_update();
    Ok(name)
}

async fn pause(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(form): Form<ReturnForm>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let name = set_paused(&state, id, true).await?;
    let jar = flash(
        jar,
        "info",
        crate::i18n::tf("monitor_form.paused", &[("name", &name)]),
    );
    Ok((
        jar,
        Redirect::to(&safe_return(&form.return_to, &format!("/monitors/{id}"))),
    ))
}

async fn resume(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(form): Form<ReturnForm>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let name = set_paused(&state, id, false).await?;
    let jar = flash(
        jar,
        "success",
        crate::i18n::tf("monitor_form.resumed", &[("name", &name)]),
    );
    Ok((
        jar,
        Redirect::to(&safe_return(&form.return_to, &format!("/monitors/{id}"))),
    ))
}

async fn check_now(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let exists = state
        .with_conn(move |c| db_monitors::get_monitor(c, id))
        .await?
        .is_some();
    if !exists {
        return Err(ApiError::NotFound);
    }
    let jar = match state.scheduler.check_now(id).await {
        Some(status) => {
            let kind = match status {
                HeartbeatStatus::Up => "success",
                HeartbeatStatus::Down => "error",
                _ => "info",
            };
            let status = status.label().to_lowercase();
            flash(
                jar,
                kind,
                crate::i18n::tf("monitor_form.checked_now", &[("status", &status)]),
            )
        }
        None => flash(jar, "info", crate::i18n::t("monitor_form.cant_check")),
    };
    Ok((jar, Redirect::to(&format!("/monitors/{id}"))))
}

#[derive(Template)]
#[template(path = "partials/monitor_push.html")]
struct PushCardTemplate {
    id: i64,
    push_url: String,
    can_edit: bool,
}

async fn regenerate_push_token(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let token = super::push::generate_push_token();
    let t = token.clone();
    let base_url = state
        .with_conn(move |c| {
            let Some(m) = db_monitors::get_monitor(c, id)? else {
                return Ok(None);
            };
            if m.monitor_type != MonitorType::Push {
                return Ok(Some(Err(())));
            }
            db_monitors::set_push_token(c, id, Some(&t))?;
            Ok(Some(Ok(db_settings::load(c)?.base_url)))
        })
        .await?
        .ok_or(ApiError::NotFound)?
        .map_err(|()| ApiError::bad_request(crate::i18n::t("push.only_push")))?;
    state.scheduler.reload(id).await;
    let mut res = HtmlTemplate(PushCardTemplate {
        id,
        push_url: push_url(base_url.as_deref(), &headers, &token),
        can_edit: true,
    })
    .into_response();
    if let Some(v) = toast_header(crate::i18n::t("push.created_toast"), "success") {
        res.headers_mut().insert("hx-trigger", v);
    }
    Ok(res)
}

#[derive(Deserialize, Default)]
struct BulkForm {
    #[serde(default)]
    ids: Vec<i64>,
    #[serde(default)]
    action: String,
    /// "true": apply to every monitor matching `filter` instead of `ids`.
    #[serde(default)]
    all: Option<String>,
    /// Dashboard query string the "all" option refers to.
    #[serde(default)]
    filter: String,
    #[serde(default)]
    return_to: String,
}

async fn bulk(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    axum_extra::extract::Form(form): axum_extra::extract::Form<BulkForm>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let back = safe_return(&form.return_to, "/");
    let respond = |jar: SignedCookieJar, to: &str| -> Response {
        if is_htmx(&headers) {
            let mut res = (jar, StatusCode::OK).into_response();
            if let Ok(v) = HeaderValue::from_str(to) {
                res.headers_mut().insert("hx-redirect", v);
            }
            res
        } else {
            (jar, Redirect::to(to)).into_response()
        }
    };
    if !matches!(form.action.as_str(), "pause" | "resume" | "delete") {
        return Err(ApiError::bad_request(crate::i18n::t(
            "monitor_form.bulk_bad_action",
        )));
    }

    let ids = selected_ids(
        &state,
        form.all.as_deref() == Some("true"),
        &form.filter,
        &form.ids,
    )
    .await?;
    if ids.is_empty() {
        return Ok(respond(
            flash(jar, "info", crate::i18n::t("monitor_form.bulk_none")),
            &back,
        ));
    }

    let action = form.action.clone();
    let changed: Vec<i64> = state
        .with_conn(move |c| {
            let tx = c.transaction()?;
            let ids = db_monitors::existing_ids(&tx, &ids)?;
            let changed = match action.as_str() {
                "pause" => {
                    let changed = db_monitors::set_active_many(&tx, &ids, false)?;
                    for id in &changed {
                        db_events::log_paused(&tx, *id)?;
                    }
                    changed
                }
                "resume" => {
                    let changed = db_monitors::set_active_many(&tx, &ids, true)?;
                    for id in &changed {
                        db_events::log_resumed(&tx, *id)?;
                    }
                    changed
                }
                _ => {
                    db_monitors::delete_many(&tx, &ids)?;
                    ids
                }
            };
            tx.commit()?;
            Ok(changed)
        })
        .await?;
    crate::maintenance::invalidate(&state);
    for id in &changed {
        state.scheduler.reload(*id).await;
    }
    state.notify_update();

    let n = changed.len();
    let count = n as i64;
    let message = match (form.action.as_str(), n) {
        (_, 0) => crate::i18n::t("monitor_form.bulk_nothing").to_string(),
        ("pause", _) => crate::i18n::tn("monitor_form.bulk_paused", count),
        ("resume", _) => crate::i18n::tn("monitor_form.bulk_resumed", count),
        _ => crate::i18n::tn("monitor_form.bulk_deleted", count),
    };
    let kind = if n == 0 { "info" } else { "success" };
    Ok(respond(flash(jar, kind, message), &back))
}

/// The monitors a bulk action targets: the ticked `ids` (first occurrence
/// order), or with `all` every monitor the dashboard shows for `filter`.
async fn selected_ids(
    state: &AppState,
    all: bool,
    filter: &str,
    ids: &[i64],
) -> ApiResult<Vec<i64>> {
    if all {
        let query = DashQuery::parse(Some(filter));
        let (monitors, tags) = state
            .with_conn(|c| {
                Ok((
                    db_monitors::list_monitors_with_status(c)?,
                    db_tags::tags_by_monitor(c)?,
                ))
            })
            .await?;
        let data = build_dashboard(
            &monitors,
            tags,
            &DashQuery {
                select: true,
                ..query
            },
            true,
        );
        return Ok(data.rows.iter().map(|r| r.id).collect());
    }
    let mut out: Vec<i64> = Vec::with_capacity(ids.len());
    for id in ids {
        if !out.contains(id) {
            out.push(*id);
        }
    }
    Ok(out)
}

// ===========================================================================
// Bulk edit (select mode → "Edit…" sheet)
// ===========================================================================

/// The bulk edit sheet's fields (`partials/bulk_edit_fields.html`).
pub struct BulkSheet {
    pub v: bulk_edit::BulkValues,
    pub errors: Errors,
    /// Callout text when `errors` isn't empty.
    pub error_summary: String,
    pub channels: Vec<ChannelOption>,
    pub tags: Vec<Tag>,
    pub palette: Vec<(&'static str, String)>,
    pub has_agents: bool,
}

impl BulkSheet {
    pub fn err(&self, field: &str) -> &str {
        self.errors.get(field).map(String::as_str).unwrap_or("")
    }

    fn channel_ids(&self) -> Vec<i64> {
        self.channels.iter().map(|c| c.id).collect()
    }

    fn tag_ids(&self) -> Vec<i64> {
        self.tags.iter().map(|t| t.id).collect()
    }
}

/// An empty sheet with the current channels and tags.
pub(crate) fn load_bulk_sheet(conn: &Connection) -> rusqlite::Result<BulkSheet> {
    Ok(BulkSheet {
        v: bulk_edit::BulkValues {
            new_tag_color: db_tags::DEFAULT_COLOR.into(),
            ..bulk_edit::BulkValues::default()
        },
        errors: Errors::new(),
        error_summary: String::new(),
        channels: channel_options(conn)?,
        tags: db_tags::list_tags(conn)?,
        palette: palette_options(),
        has_agents: conn.query_row("SELECT EXISTS(SELECT 1 FROM agents)", [], |r| r.get(0))?,
    })
}

#[derive(Template)]
#[template(path = "partials/bulk_edit_fields.html")]
struct BulkFieldsTemplate {
    b: BulkSheet,
}

/// After a bulk edit: the results without select mode, plus out-of-band
/// swaps for the page header and the (now empty) bulk bar.
#[derive(Template)]
#[template(path = "partials/bulk_edit_done.html")]
struct BulkDoneTemplate {
    data: super::dashboard::DashboardData,
}

/// Selection fields of the bulk form (`ids`, `all`, `filter`, `return_to`).
struct BulkSelection {
    ids: Vec<i64>,
    all: bool,
    filter: String,
    return_to: String,
}

impl BulkSelection {
    fn from_pairs(p: &Pairs) -> Self {
        BulkSelection {
            ids: p
                .all("ids")
                .iter()
                .filter_map(|v| v.trim().parse().ok())
                .collect(),
            all: p.flag("all"),
            filter: p.text("filter"),
            return_to: p.raw("return_to").unwrap_or_default().to_string(),
        }
    }

    /// The dashboard state to go back to (select mode off).
    fn back_query(&self) -> DashQuery {
        let back = safe_return(&self.return_to, "/");
        let query = back.split_once('?').map(|(_, q)| q);
        DashQuery {
            select: false,
            ..DashQuery::parse(query)
        }
    }
}

/// htmx answer that only shows a toast (nothing is swapped).
fn toast_only(message: &str, kind: &str) -> Response {
    let mut res = StatusCode::OK.into_response();
    if let Some(v) = toast_header(message, kind) {
        res.headers_mut().insert("hx-trigger", v);
    }
    res.headers_mut()
        .insert("hx-reswap", HeaderValue::from_static("none"));
    res
}

async fn bulk_edit(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    axum_extra::extract::Form(pairs): axum_extra::extract::Form<Vec<(String, String)>>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let p = Pairs(pairs);
    let sel = BulkSelection::from_pairs(&p);
    let back = sel.back_query();
    let htmx = is_htmx(&headers);
    // Plain form posts (no JavaScript) come back to select mode on problems.
    let retry_href = DashQuery {
        select: true,
        ..back.clone()
    }
    .href();
    let notice = |jar: SignedCookieJar, message: &str, kind: &str| -> Response {
        if htmx {
            toast_only(message, kind)
        } else {
            (flash(jar, kind, message), Redirect::to(&retry_href)).into_response()
        }
    };

    let ids = selected_ids(&state, sel.all, &sel.filter, &sel.ids).await?;
    if ids.is_empty() {
        return Ok(notice(
            jar,
            crate::i18n::t("monitor_form.bulk_none"),
            "info",
        ));
    }
    let mut sheet = state.with_conn(|c| load_bulk_sheet(c)).await?;
    sheet.v = bulk_edit::BulkValues::from_pairs(&p, &sheet.channel_ids(), &sheet.tag_ids());
    if sheet.v.change_count() == 0 {
        return Ok(notice(jar, crate::i18n::t("bulk.nothing_chosen"), "info"));
    }
    let edit = match bulk_edit::validate(&sheet.v) {
        Ok(edit) => edit,
        Err(errors) => {
            if !htmx {
                let first = errors.values().next().cloned().unwrap_or_default();
                return Ok(notice(jar, &first, "error"));
            }
            sheet.error_summary = crate::i18n::t("bulk.fix_fields").to_string();
            sheet.errors = errors;
            return Ok(HtmlTemplate(BulkFieldsTemplate { b: sheet }).into_response());
        }
    };

    // Sharing credentials with every agent is for administrators.
    if edit.run_on_agents == Some(true) && !user.is_admin() {
        return Ok(notice(
            jar,
            crate::i18n::t("validation.run_on_agents_admin"),
            "error",
        ));
    }
    let outcome = state
        .with_conn(move |c| bulk_edit::apply(c, &ids, &edit))
        .await?;
    crate::maintenance::invalidate(&state);
    for id in &outcome.updated {
        state.scheduler.reload(*id).await;
    }
    state.notify_update();
    tracing::info!(
        user = %user.username,
        updated = outcome.updated.len(),
        skipped = outcome.skipped.len(),
        "bulk edit applied"
    );

    let message = outcome.message();
    let kind = if outcome.updated.is_empty() {
        "info"
    } else {
        "success"
    };
    let back_href = back.href();
    if !htmx {
        return Ok((flash(jar, kind, message), Redirect::to(&back_href)).into_response());
    }
    let data = super::dashboard::load_dashboard_data(&state, &back, true).await?;
    let mut res = HtmlTemplate(BulkDoneTemplate { data }).into_response();
    let h = res.headers_mut();
    if let Some(v) = trigger_header(&serde_json::json!({
        "vv:toast": { "message": message, "kind": kind },
        "vv:close-sheet": true,
    })) {
        h.insert("hx-trigger", v);
    }
    h.insert("hx-retarget", HeaderValue::from_static("#dash-results"));
    h.insert("hx-reswap", HeaderValue::from_static("innerHTML"));
    if let Ok(v) = HeaderValue::from_str(&back_href) {
        h.insert("hx-push-url", v);
    }
    Ok(res)
}

/// htmx: the sheet's summary line for the current selection and choices.
async fn bulk_edit_summary(
    State(state): State<AppState>,
    user: CurrentUser,
    axum_extra::extract::Form(pairs): axum_extra::extract::Form<Vec<(String, String)>>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let p = Pairs(pairs);
    let sel = BulkSelection::from_pairs(&p);
    let monitors = selected_ids(&state, sel.all, &sel.filter, &sel.ids)
        .await?
        .len();
    let (channel_ids, tag_ids) = state
        .with_conn(|c| {
            let channels = db_notifications::list_channels(c)?
                .into_iter()
                .map(|ch| ch.id)
                .collect::<Vec<_>>();
            let tags = db_tags::list_tags(c)?
                .into_iter()
                .map(|t| t.id)
                .collect::<Vec<_>>();
            Ok((channels, tags))
        })
        .await?;
    let v = bulk_edit::BulkValues::from_pairs(&p, &channel_ids, &tag_ids);
    let text = bulk_edit::summary(v.change_count(), monitors);
    Ok(axum::response::Html(super::error::escape_html(&text)).into_response())
}

// ===========================================================================
// Detail
// ===========================================================================

/// A timeline entry.
pub struct EventView {
    pub class: &'static str,
    pub title: &'static str,
    pub started_at: String,
    pub text: String,
    pub duration: String,
}

fn event_view(e: &MonitorEvent) -> EventView {
    let class = match e.kind {
        MonitorEventKind::Down => "status-down",
        MonitorEventKind::Up => "status-up",
        MonitorEventKind::Maintenance => "status-maintenance",
        MonitorEventKind::Paused => "status-paused",
        MonitorEventKind::Resumed => "status-info",
        MonitorEventKind::CertExpiring
        | MonitorEventKind::DomainExpiring
        | MonitorEventKind::Flapping => "status-warning",
    };
    let duration = match (e.is_open(), e.duration_seconds) {
        (true, _)
            if matches!(
                e.kind,
                MonitorEventKind::Down
                    | MonitorEventKind::Maintenance
                    | MonitorEventKind::Paused
                    | MonitorEventKind::Flapping
            ) =>
        {
            crate::i18n::t("detail.ongoing").to_string()
        }
        (false, Some(s)) if s > 0 => {
            crate::i18n::tf("detail.lasted", &[("duration", &view::format_duration(s))])
        }
        _ => String::new(),
    };
    EventView {
        class,
        title: e.kind.label(),
        started_at: e.started_at.clone(),
        text: e.message.clone().unwrap_or_default(),
        duration,
    }
}

/// An expiry card row.
pub struct ExpiryView {
    pub title: &'static str,
    /// TLS certificate (vs domain registration).
    pub is_cert: bool,
    pub subject: String,
    pub issuer: String,
    pub expires: String,
    pub remaining: String,
    pub status: &'static str,
    pub checked_at: String,
}

fn expiry_view(r: &ExpiryRecord, warn_days: i64) -> ExpiryView {
    let days = r.days_left;
    let remaining = match days {
        d if d < -1 => crate::i18n::tf("detail.expired_days_ago", &[("n", &(-d).to_string())]),
        -1 => crate::i18n::t("detail.expired_yesterday").into(),
        0 => crate::i18n::t("detail.expires_today").into(),
        1 => crate::i18n::t("detail.one_day_left").into(),
        d => crate::i18n::tf("detail.days_left", &[("n", &d.to_string())]),
    };
    let status = if days < 0 {
        "danger"
    } else if days <= warn_days {
        "warning"
    } else {
        "success"
    };
    ExpiryView {
        title: if r.kind == "domain" {
            crate::i18n::t("detail.domain_registration")
        } else {
            crate::i18n::t("detail.tls_certificate")
        },
        is_cert: r.kind != "domain",
        subject: r.subject.clone().unwrap_or_default(),
        issuer: r.issuer.clone().unwrap_or_default(),
        expires: view::short_date(&r.expires_at),
        remaining,
        status,
        checked_at: r.checked_at.clone(),
    }
}

/// A child of a group monitor.
pub struct ChildView {
    pub id: i64,
    pub name: String,
    pub type_label: &'static str,
    pub status: &'static str,
    pub status_label: &'static str,
}

/// A recent-heartbeat table row.
pub struct HeartbeatView {
    pub status: HeartbeatStatus,
    pub checked_at: String,
    pub response: String,
    pub message: String,
}

fn heartbeat_view(h: &Heartbeat) -> HeartbeatView {
    HeartbeatView {
        status: h.status,
        checked_at: h.checked_at.clone(),
        response: h
            .response_time_ms
            .map(|ms| view::format_ms(ms as f64))
            .unwrap_or_else(|| "—".into()),
        message: h.message.clone().unwrap_or_default(),
    }
}

/// An uptime tile.
pub struct UptimeTile {
    pub label: &'static str,
    pub value: String,
    pub status: &'static str,
}

fn uptime_status(p: Option<f64>) -> &'static str {
    match p {
        None => "",
        Some(p) if p >= 99.0 => "up",
        Some(p) if p >= 95.0 => "pending",
        Some(_) => "down",
    }
}

#[derive(Template)]
#[template(path = "partials/monitor_chart.html")]
struct ChartTemplate {
    id: i64,
    chart: Chart,
    ranges: [Range; 4],
}

#[derive(Template)]
#[template(path = "partials/monitor_heartbeats.html")]
struct HeartbeatsTemplate {
    id: i64,
    rows: Vec<HeartbeatView>,
    next_before: Option<i64>,
}

#[derive(Template)]
#[template(path = "monitor_detail.html")]
struct DetailTemplate {
    ctx: PageCtx,
    monitor: Monitor,
    status: &'static str,
    status_label: &'static str,
    since: Option<String>,
    last_check: Option<String>,
    last_message: String,
    flapping: bool,
    tags: Vec<MonitorTag>,
    parent: Option<ParentLink>,
    uptime: Vec<UptimeTile>,
    response_label: String,
    days: Vec<UptimeDay>,
    days_label: String,
    chart: ChartTemplate,
    expiry: Vec<ExpiryView>,
    events: Vec<EventView>,
    heartbeats: HeartbeatsTemplate,
    agents: Vec<AgentResult>,
    /// Name of the agent that checks the monitor instead of this server.
    checked_by: Option<String>,
    children: Vec<ChildView>,
    push: Option<PushCardTemplate>,
    browser: Option<super::remote_browsers::BrowserPanel>,
    channels: Vec<String>,
    interval_label: String,
    retry_label: String,
    timeout_label: String,
}

impl DetailTemplate {
    fn is_type(&self, t: &str) -> bool {
        self.monitor.monitor_type.as_str() == t
    }
}

/// Link back to the parent group.
pub struct ParentLink {
    pub id: i64,
    pub name: String,
}

#[derive(Deserialize, Default)]
struct RangeParams {
    #[serde(default)]
    range: String,
}

fn load_chart(conn: &Connection, m: &Monitor, range: Range) -> rusqlite::Result<Chart> {
    let buckets = if range.uses_rollups() {
        view::buckets_from_days(&rollups::daily_uptime(conn, m.id, range.buckets())?)
    } else {
        let now: i64 = conn.query_row("SELECT unixepoch('now')", [], |r| r.get(0))?;
        // The newest bucket (index n-1) ends at `now`.
        db_heartbeats::chart_buckets(
            conn,
            m.id,
            now - range.seconds() + 1,
            range.bucket_seconds(),
        )?
    };
    let gap =
        (m.interval_seconds.max(1) + range.bucket_seconds() - 1) / range.bucket_seconds().max(1);
    Ok(view::build_chart(range, &buckets, gap))
}

struct DetailBundle {
    monitor: Monitor,
    tags: Vec<MonitorTag>,
    parent: Option<ParentLink>,
    uptime: [Option<f64>; 5],
    days: Vec<rollups::DayStat>,
    chart: Chart,
    expiry: Vec<ExpiryRecord>,
    events: Vec<MonitorEvent>,
    heartbeats: Vec<Heartbeat>,
    latest: Option<Heartbeat>,
    agents: Vec<AgentResult>,
    checked_by: Option<String>,
    children: Vec<ChildView>,
    channels: Vec<String>,
    base_url: Option<String>,
    browser: Option<super::remote_browsers::BrowserPanel>,
}

fn load_detail(conn: &Connection, id: i64, range: Range) -> rusqlite::Result<Option<DetailBundle>> {
    let Some(monitor) = db_monitors::get_monitor(conn, id)? else {
        return Ok(None);
    };
    let parent = match monitor.parent_id {
        Some(p) => db_monitors::get_monitor(conn, p)?.map(|g| ParentLink {
            id: g.id,
            name: g.name,
        }),
        None => None,
    };
    let mut uptime = [None; 5];
    for (slot, days) in uptime.iter_mut().zip([1, 7, 30, 90, 365]) {
        *slot = rollups::uptime_pct(conn, id, days)?;
    }
    let children = if monitor.monitor_type == MonitorType::Group {
        db_monitors::children(conn, id)?
            .into_iter()
            .map(|c| {
                let status = if !c.active {
                    "paused"
                } else {
                    db_heartbeats::latest_local(conn, c.id)?
                        .map(|h| h.status.as_str())
                        .unwrap_or("pending")
                };
                Ok(ChildView {
                    id: c.id,
                    name: c.name,
                    type_label: c.monitor_type.label(),
                    status,
                    status_label: status_text(status),
                })
            })
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let links = db_notifications::monitor_channel_links(conn, id)?;
    let channels = db_notifications::list_channels(conn)?
        .into_iter()
        .filter_map(|c| {
            links.iter().find(|(cid, _)| *cid == c.id).map(|(_, esc)| {
                if *esc {
                    format!("{} (escalation)", c.name)
                } else {
                    c.name
                }
            })
        })
        .collect();
    Ok(Some(DetailBundle {
        tags: db_tags::tags_for_monitor(conn, id)?,
        parent,
        uptime,
        days: rollups::daily_uptime(conn, id, 90)?,
        chart: load_chart(conn, &monitor, range)?,
        expiry: db_monitors::expiry_records(conn, id)?,
        events: db_events::list_for_monitor(conn, id, EVENT_LIMIT)?,
        heartbeats: db_heartbeats::list_page(conn, id, None, HEARTBEAT_PAGE)?,
        latest: db_heartbeats::latest_local(conn, id)?,
        agents: db_heartbeats::agent_results(conn, id)?,
        checked_by: match monitor
            .check_from_agent_id
            .filter(|_| monitor.is_agent_checked())
        {
            Some(agent_id) => crate::db::agents::get_agent(conn, agent_id)?.map(|a| a.name),
            None => None,
        },
        children,
        channels,
        base_url: db_settings::load(conn)?.base_url,
        browser: super::remote_browsers::detail_panel(conn, &monitor)?,
        monitor,
    }))
}

fn status_text(key: &str) -> &'static str {
    match key {
        "up" => crate::i18n::t("status.up"),
        "down" => crate::i18n::t("status.down"),
        "maintenance" => crate::i18n::t("status.maintenance"),
        "paused" => crate::i18n::t("status.paused"),
        _ => crate::i18n::t("status.pending"),
    }
}

fn heartbeats_template(id: i64, list: &[Heartbeat]) -> HeartbeatsTemplate {
    HeartbeatsTemplate {
        id,
        rows: list.iter().map(heartbeat_view).collect(),
        next_before: if list.len() as i64 >= HEARTBEAT_PAGE {
            list.last().map(|h| h.id)
        } else {
            None
        },
    }
}

async fn detail(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(params): Query<RangeParams>,
) -> ApiResult<impl IntoResponse> {
    let range = Range::parse(&params.range);
    let b = state
        .with_conn(move |c| load_detail(c, id, range))
        .await?
        .ok_or(ApiError::NotFound)?;
    let snapshot = state.scheduler.status_snapshot(id).await;

    let m = &b.monitor;
    let (status, since, last_check, flapping) = if !m.active {
        (
            "paused",
            None,
            b.latest.as_ref().map(|h| h.checked_at.clone()),
            false,
        )
    } else {
        match &snapshot {
            Some(s) if s.status.is_some() => (
                s.status.map(HeartbeatStatus::as_str).unwrap_or("pending"),
                s.since.clone(),
                s.last_check_at.clone(),
                s.flapping,
            ),
            _ => (
                b.latest
                    .as_ref()
                    .map(|h| h.status.as_str())
                    .unwrap_or("pending"),
                None,
                b.latest.as_ref().map(|h| h.checked_at.clone()),
                false,
            ),
        }
    };
    let last_message = b
        .latest
        .as_ref()
        .and_then(|h| h.message.clone())
        .unwrap_or_default();
    let uptime = [
        "detail.tile_24h",
        "detail.tile_7d",
        "detail.tile_30d",
        "detail.tile_90d",
        "detail.tile_1y",
    ]
    .into_iter()
    .map(crate::i18n::t)
    .zip(b.uptime)
    .map(|(label, p)| UptimeTile {
        label,
        value: p.map(view::format_pct).unwrap_or_else(|| "—".into()),
        status: uptime_status(p),
    })
    .collect();
    let push = m
        .push_token
        .as_deref()
        .filter(|_| m.monitor_type == MonitorType::Push)
        .map(|t| PushCardTemplate {
            id,
            push_url: push_url(b.base_url.as_deref(), &headers, t),
            can_edit: user.can_edit(),
        });
    let response_label = b
        .latest
        .as_ref()
        .and_then(|h| h.response_time_ms)
        .map(|ms| view::format_ms(ms as f64))
        .unwrap_or_else(|| "—".into());
    let days_label = b.uptime[3]
        .map(view::format_pct)
        .unwrap_or_else(|| crate::i18n::t("detail.no_data").into());
    let interval_label = view::format_duration(m.interval_seconds);
    let retry_label = view::format_duration(m.effective_retry_interval());
    let timeout_label = view::format_duration(m.timeout_seconds);
    let expiry = b
        .expiry
        .iter()
        .map(|r| expiry_view(r, m.expiry_days))
        .collect();
    let events = b.events.iter().map(event_view).collect();
    let heartbeats = heartbeats_template(id, &b.heartbeats);

    let (ctx, jar) =
        PageCtx::build(&state, Some(&user), &jar, m.name.clone(), "dashboard", "").await;
    Ok((
        jar,
        HtmlTemplate(DetailTemplate {
            ctx,
            status,
            status_label: status_text(status),
            since,
            last_check,
            last_message,
            flapping,
            tags: b.tags,
            parent: b.parent,
            uptime,
            response_label,
            days: view::uptime_days(&b.days),
            days_label,
            chart: ChartTemplate {
                id,
                chart: b.chart,
                ranges: Range::ALL,
            },
            expiry,
            events,
            heartbeats,
            agents: b.agents,
            checked_by: b.checked_by,
            children: b.children,
            push,
            browser: b.browser,
            channels: b.channels,
            interval_label,
            retry_label,
            timeout_label,
            // The page is for every role: no credentials from the URLs.
            monitor: Monitor {
                target: super::metrics::redact_target(&b.monitor.target),
                proxy_url: b
                    .monitor
                    .proxy_url
                    .as_deref()
                    .map(super::metrics::redact_target),
                ..b.monitor
            },
        }),
    ))
}

async fn chart_partial(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<i64>,
    Query(params): Query<RangeParams>,
) -> ApiResult<impl IntoResponse> {
    let range = Range::parse(&params.range);
    let chart = state
        .with_conn(move |c| match db_monitors::get_monitor(c, id)? {
            Some(m) => load_chart(c, &m, range).map(Some),
            None => Ok(None),
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(HtmlTemplate(ChartTemplate {
        id,
        chart,
        ranges: Range::ALL,
    }))
}

async fn heartbeats_partial(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<i64>,
    RawQuery(raw): RawQuery,
) -> ApiResult<impl IntoResponse> {
    let before = url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes())
        .find(|(k, _)| k == "before")
        .and_then(|(_, v)| v.parse::<i64>().ok());
    let list = state
        .with_conn(move |c| {
            if db_monitors::get_monitor(c, id)?.is_none() {
                return Ok(None);
            }
            db_heartbeats::list_page(c, id, before, HEARTBEAT_PAGE).map(Some)
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(HtmlTemplate(heartbeats_template(id, &list)))
}

#[cfg(test)]
mod tests;
