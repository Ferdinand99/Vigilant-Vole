//! Configuration as code: export the whole setup as one declarative YAML/JSON
//! document, and import such a document back (merge or replace), with a
//! dry-run plan first. Owned by the api agent.
//!
//! The document is keyed by stable, human-readable keys instead of database
//! ids, so it can live in git and be applied to a fresh instance:
//!
//! - tags by `name`, notification channels by `name`, status pages by `slug`,
//!   maintenance windows by `title`;
//! - monitors by `key` (defaults to `name`; exported as the monitor's
//!   `external_id` for config-managed monitors). Parents, tags, channels and
//!   status-page sections refer to these keys.
//!
//! Sections missing from a document are left alone in both modes. In
//! `replace` mode, objects of a *present* section that the document doesn't
//! list are deleted (Docker-discovered monitors are never touched).
//!
//! Secrets (channel credentials, basic-auth passwords, sensitive HTTP headers,
//! push tokens) are exported as [`SECRET_MASK`] unless asked for; importing a
//! masked value keeps whatever is stored.
//!
//! Validation reuses the same rules as the UI: monitors go through
//! [`crate::api::monitors::form::validate`], channels through the provider
//! schemas, status pages and maintenance windows through the db helpers.
//!
//! Startup hooks for `server.rs`: [`apply_pending_restore`] (swap in a
//! database uploaded on the backup page) and [`spawn_startup_import`]
//! (`VV_CONFIG_FILE`, GitOps style).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::api::AppState;
use crate::api::monitors::form::{self, ChannelMode, FormContext, FormValues};
use crate::db::maintenance as db_maint;
use crate::db::models::{Monitor, MonitorType, NewMonitor};
use crate::db::status_pages::{self as db_pages, NewStatusPage};
use crate::db::tags::{self as db_tags, MonitorTag};
use crate::db::{monitors as db_monitors, notifications as db_notifications, settings as db_settings};
use crate::notification::{self, SECRET_MASK, schema};

/// Current document format version.
pub const DOC_VERSION: u32 = 1;

/// Environment variable naming a config file to merge at startup.
pub const CONFIG_FILE_ENV: &str = "VV_CONFIG_FILE";

/// File name (inside `DATA_DIR`) of a database waiting to replace the live one.
pub const RESTORE_PENDING_FILE: &str = "restore-pending.db";

// ===========================================================================
// Document
// ===========================================================================

/// The whole configuration. Every section is optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    #[serde(default = "doc_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<SettingsDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<TagDoc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_channels: Option<Vec<ChannelDoc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitors: Option<Vec<MonitorDoc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_pages: Option<Vec<StatusPageDoc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<Vec<MaintenanceDoc>>,
}

fn doc_version() -> u32 {
    DOC_VERSION
}

/// Non-secret instance settings. Missing keys keep their current value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    /// Empty string clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badges_public: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_enabled: Option<bool>,
    /// Empty string clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_docker_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_confirmation_default: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagDoc {
    pub name: String,
    /// Palette name (`blue`, `green`, …); hex values map to the nearest one.
    #[serde(default = "default_tag_color")]
    pub color: String,
}

fn default_tag_color() -> String {
    db_tags::DEFAULT_COLOR.to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelDoc {
    pub name: String,
    /// Provider kind (`discord`, `slack`, `email`, …).
    pub kind: String,
    #[serde(default = "yes")]
    pub active: bool,
    /// Attach to new monitors automatically.
    #[serde(default, rename = "default")]
    pub is_default: bool,
    /// Provider settings; secrets may be [`SECRET_MASK`] to keep the stored value.
    #[serde(default = "empty_object")]
    pub config: Value,
}

fn yes() -> bool {
    true
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// A tag on a monitor: `"production"` or `{name: env, value: prod}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TagRef {
    Name(String),
    Full {
        name: String,
        #[serde(default)]
        value: String,
    },
}

impl TagRef {
    pub fn name(&self) -> &str {
        match self {
            TagRef::Name(n) | TagRef::Full { name: n, .. } => n,
        }
    }
    pub fn value(&self) -> &str {
        match self {
            TagRef::Name(_) => "",
            TagRef::Full { value, .. } => value,
        }
    }
}

/// A channel on a monitor: `"Ops Discord"` or `{name: PagerDuty, escalation: true}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChannelRef {
    Name(String),
    Full {
        name: String,
        #[serde(default)]
        escalation: bool,
    },
}

impl ChannelRef {
    pub fn name(&self) -> &str {
        match self {
            ChannelRef::Name(n) | ChannelRef::Full { name: n, .. } => n,
        }
    }
    pub fn escalation(&self) -> bool {
        matches!(self, ChannelRef::Full { escalation: true, .. })
    }
}

/// A monitor. Besides the fields below, any writable monitor column
/// (`interval_seconds`, `http_method`, `keyword`, …) may appear; missing
/// ones take the schema default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorDoc {
    /// Stable key; defaults to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub name: String,
    pub monitor_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// Key of the parent group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(flatten)]
    pub options: Map<String, Value>,
    /// `None` keeps the current tags (none for new monitors).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<TagRef>>,
    /// `None` keeps the current links (default channels for new monitors).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifications: Option<Vec<ChannelRef>>,
    /// Push monitors only. Missing or masked keeps the current token (or
    /// generates one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_token: Option<String>,
}

impl MonitorDoc {
    pub fn key(&self) -> &str {
        self.key.as_deref().map(str::trim).filter(|k| !k.is_empty()).unwrap_or(self.name.trim())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionDoc {
    pub name: String,
    /// Monitor keys, in display order.
    #[serde(default)]
    pub monitors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusPageDoc {
    pub slug: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_domain: Option<String>,
    #[serde(default = "yes")]
    pub published: bool,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub show_tags: bool,
    #[serde(default = "yes")]
    pub show_uptime: bool,
    #[serde(default = "yes")]
    pub show_response: bool,
    #[serde(default = "yes")]
    pub show_powered_by: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_css: Option<String>,
    #[serde(default = "default_refresh")]
    pub refresh_seconds: i64,
    /// `None` keeps the current layout (one empty "Services" section for new pages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<SectionDoc>>,
}

fn default_theme() -> String {
    "auto".into()
}

fn default_refresh() -> i64 {
    60
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceDoc {
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// `manual` | `once` | `daily` | `weekly`.
    pub strategy: String,
    /// Manual: in maintenance right now. Scheduled: enabled.
    #[serde(default = "yes")]
    pub active: bool,
    /// UTC `YYYY-MM-DD HH:MM:SS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_at: Option<String>,
    /// Weekly: ISO weekdays, `"1,3,5"` (Mon = 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekdays: Option<String>,
    /// Local `HH:MM` in `timezone`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_to: Option<String>,
    #[serde(default = "default_tz")]
    pub timezone: String,
    #[serde(default)]
    pub monitors: Vec<String>,
    /// Status page slugs.
    #[serde(default)]
    pub status_pages: Vec<String>,
}

fn default_tz() -> String {
    "UTC".into()
}

// ===========================================================================
// Parsing and serialising
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Yaml,
    Json,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "yaml" | "yml" => Some(Format::Yaml),
            "json" => Some(Format::Json),
            _ => None,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Format::Yaml => "application/yaml; charset=utf-8",
            Format::Json => "application/json",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Yaml => "yaml",
            Format::Json => "json",
        }
    }
}

/// Parses a document; JSON when it starts with `{`, YAML otherwise (YAML is a
/// superset of JSON anyway, but JSON errors are clearer from serde_json).
pub fn parse_document(text: &str) -> Result<Document, String> {
    let trimmed = text.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return Err("The document is empty.".into());
    }
    let doc: Document = if trimmed.starts_with('{') {
        serde_json::from_str(trimmed).map_err(|e| format!("Invalid JSON: {e}"))?
    } else {
        serde_yaml_ng::from_str(trimmed).map_err(|e| format!("Invalid YAML: {e}"))?
    };
    if doc.version > DOC_VERSION {
        return Err(format!(
            "This document uses format version {}, but this server understands up to {DOC_VERSION}. Upgrade Vigilant Vole first.",
            doc.version
        ));
    }
    Ok(doc)
}

pub fn render(doc: &Document, format: Format) -> Result<String, String> {
    match format {
        Format::Json => serde_json::to_string_pretty(doc).map_err(|e| e.to_string()),
        Format::Yaml => serde_yaml_ng::to_string(doc)
            .map(|y| format!("# Vigilant Vole configuration (format version {DOC_VERSION})\n{y}"))
            .map_err(|e| e.to_string()),
    }
}

// ===========================================================================
// Keys
// ===========================================================================

/// Gives each item a unique key: the natural key, or `"{key} #{id}"` for
/// later duplicates.
fn unique_keys(items: impl Iterator<Item = (i64, String)>) -> HashMap<i64, String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = HashMap::new();
    for (id, key) in items {
        let k = if seen.insert(key.to_lowercase()) { key } else { format!("{key} #{id}") };
        out.insert(id, k);
    }
    out
}

/// Stable key of every monitor (by id). Config-managed monitors use their
/// `external_id`, everything else its name.
pub fn monitor_keys(monitors: &[Monitor]) -> HashMap<i64, String> {
    let mut sorted: Vec<&Monitor> = monitors.iter().collect();
    sorted.sort_by_key(|m| m.id);
    unique_keys(sorted.into_iter().map(|m| {
        let key = match (&m.external_id, m.source.as_str()) {
            (Some(ext), "config") if !ext.trim().is_empty() => ext.clone(),
            _ => m.name.clone(),
        };
        (m.id, key)
    }))
}

fn norm(key: &str) -> String {
    key.trim().to_lowercase()
}

// ===========================================================================
// Secrets in monitor fields
// ===========================================================================

/// Header names whose values are treated as secrets.
pub fn is_sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(n.as_str(), "authorization" | "proxy-authorization" | "cookie")
        || ["token", "secret", "key", "password", "auth", "session", "signature"]
            .iter()
            .any(|w| n.contains(w))
}

/// `http_headers` JSON text with sensitive values masked.
pub fn redact_headers(headers: &str) -> Value {
    match serde_json::from_str::<Value>(headers) {
        Ok(Value::Object(map)) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let v = if is_sensitive_header(&k) { Value::String(SECRET_MASK.into()) } else { v };
                    (k, v)
                })
                .collect(),
        ),
        Ok(other) => other,
        Err(_) => Value::String(headers.to_string()),
    }
}

/// Replaces masked header values in `new` with the stored ones (by name,
/// case-insensitive). Returns the header names that couldn't be restored.
fn restore_headers(new: &mut Map<String, Value>, old: Option<&str>) -> Vec<String> {
    let old: Map<String, Value> = old
        .and_then(|o| serde_json::from_str::<Value>(o).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut missing = Vec::new();
    for (k, v) in new.iter_mut() {
        if v.as_str() != Some(SECRET_MASK) {
            continue;
        }
        match old.iter().find(|(ok, _)| ok.eq_ignore_ascii_case(k)) {
            Some((_, prev)) => *v = prev.clone(),
            None => missing.push(k.clone()),
        }
    }
    missing
}

// ===========================================================================
// Export
// ===========================================================================

/// Monitor columns that are document fields of their own or never exported.
/// Monitor fields exported as [`SECRET_MASK`] unless secrets are included.
const MONITOR_SECRETS: &[&str] = &["basic_auth_pass", "secret", "oauth_client_secret", "tls_client_key"];

const MONITOR_SPECIAL: &[&str] = &["name", "monitor_type", "target", "parent_id", "push_token", "source", "external_id"];

fn new_monitor_value(m: &NewMonitor) -> Map<String, Value> {
    match serde_json::to_value(m) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Builds the document for the current database.
pub fn export(conn: &Connection, include_secrets: bool) -> rusqlite::Result<Document> {
    let settings = db_settings::load(conn)?;
    let tags = db_tags::list_tags(conn)?;
    let channels = db_notifications::list_channels(conn)?;
    let all_monitors = db_monitors::list_monitors(conn)?;
    let keys = monitor_keys(&all_monitors);
    let channel_keys = unique_keys(channels.iter().map(|c| (c.id, c.name.clone())));
    let tags_by_monitor = db_tags::tags_by_monitor(conn)?;
    let defaults = new_monitor_value(&NewMonitor::default());

    let mut monitors: Vec<&Monitor> = all_monitors.iter().filter(|m| m.source != "docker").collect();
    monitors.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then(a.id.cmp(&b.id)));
    // Parents before children reads better and imports in one pass.
    let monitors = parents_first(&monitors);

    let mut monitor_docs = Vec::with_capacity(monitors.len());
    for m in monitors {
        let mut options = new_monitor_value(&m.to_new());
        for k in MONITOR_SPECIAL {
            options.remove(*k);
        }
        options.retain(|k, v| defaults.get(k) != Some(v));
        if let Some(Value::String(h)) = options.get("http_headers").cloned() {
            let value = if include_secrets {
                serde_json::from_str::<Value>(&h).unwrap_or(Value::String(h))
            } else {
                redact_headers(&h)
            };
            options.insert("http_headers".into(), value);
        }
        for secret in MONITOR_SECRETS {
            if !include_secrets && options.get(*secret).is_some_and(|v| !v.is_null()) {
                options.insert((*secret).into(), Value::String(SECRET_MASK.into()));
            }
        }
        let links = db_notifications::monitor_channel_links(conn, m.id)?;
        let notifications = links
            .iter()
            .filter_map(|(cid, esc)| {
                let name = channel_keys.get(cid)?.clone();
                Some(if *esc { ChannelRef::Full { name, escalation: true } } else { ChannelRef::Name(name) })
            })
            .collect();
        let tags = tags_by_monitor
            .get(&m.id)
            .map(|list| {
                list.iter()
                    .map(|t| {
                        if t.value.is_empty() {
                            TagRef::Name(t.name.clone())
                        } else {
                            TagRef::Full { name: t.name.clone(), value: t.value.clone() }
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let key = keys.get(&m.id).cloned().unwrap_or_else(|| m.name.clone());
        monitor_docs.push(MonitorDoc {
            key: (key != m.name).then_some(key),
            name: m.name.clone(),
            monitor_type: m.monitor_type.as_str().into(),
            target: m.target.clone(),
            parent: m.parent_id.and_then(|p| keys.get(&p).cloned()),
            options,
            tags: Some(tags),
            notifications: Some(notifications),
            push_token: match (&m.push_token, m.monitor_type) {
                (Some(t), MonitorType::Push) if include_secrets => Some(t.clone()),
                _ => None,
            },
        });
    }

    let channel_docs = channels
        .iter()
        .map(|c| {
            let config = if include_secrets {
                serde_json::from_str(&c.config).unwrap_or_else(|_| empty_object())
            } else {
                notification::redacted_config(c)
            };
            ChannelDoc {
                name: channel_keys.get(&c.id).cloned().unwrap_or_else(|| c.name.clone()),
                kind: c.kind.clone(),
                active: c.active,
                is_default: c.is_default,
                config,
            }
        })
        .collect();

    let mut page_docs = Vec::new();
    for summary in db_pages::list(conn)? {
        let p = summary.page;
        let sections = db_pages::sections(conn, p.id)?
            .into_iter()
            .map(|s| SectionDoc {
                name: s.name,
                monitors: s.monitors.iter().filter_map(|sm| keys.get(&sm.monitor_id).cloned()).collect(),
            })
            .collect();
        page_docs.push(StatusPageDoc {
            slug: p.slug,
            title: p.title,
            description: p.description,
            custom_domain: p.custom_domain,
            published: p.published,
            theme: p.theme,
            show_tags: p.show_tags,
            show_uptime: p.show_uptime,
            show_response: p.show_response,
            show_powered_by: p.show_powered_by,
            footer_text: p.footer_text,
            custom_css: p.custom_css,
            refresh_seconds: p.refresh_seconds,
            sections: Some(sections),
        });
    }
    let page_slugs: HashMap<i64, String> = db_pages::list(conn)?.into_iter().map(|s| (s.page.id, s.page.slug)).collect();

    let mut windows = db_maint::list(conn)?;
    windows.sort_by_key(|w| w.id);
    let window_keys = unique_keys(windows.iter().map(|w| (w.id, w.title.clone())));
    let mut maintenance_docs = Vec::new();
    for w in windows {
        let monitors = db_maint::monitor_ids(conn, w.id)?.iter().filter_map(|id| keys.get(id).cloned()).collect();
        let status_pages =
            db_maint::status_page_ids(conn, w.id)?.iter().filter_map(|id| page_slugs.get(id).cloned()).collect();
        maintenance_docs.push(MaintenanceDoc {
            title: window_keys.get(&w.id).cloned().unwrap_or(w.title),
            description: w.description,
            strategy: w.strategy,
            active: w.active,
            start_at: w.start_at,
            end_at: w.end_at,
            weekdays: w.weekdays.filter(|d| !d.is_empty()),
            time_from: w.time_from,
            time_to: w.time_to,
            timezone: w.timezone,
            monitors,
            status_pages,
        });
    }

    Ok(Document {
        version: DOC_VERSION,
        settings: Some(SettingsDoc {
            site_name: Some(settings.site_name),
            base_url: Some(settings.base_url.unwrap_or_default()),
            timezone: Some(settings.timezone),
            language: Some(settings.language),
            retention_days: Some(settings.retention_days),
            badges_public: Some(settings.badges_public),
            metrics_enabled: Some(settings.metrics_enabled),
            discovery_enabled: Some(settings.discovery_enabled),
            discovery_docker_host: Some(settings.discovery_docker_host.unwrap_or_default()),
            agent_confirmation_default: Some(settings.agent_confirmation_default),
        }),
        tags: Some(tags.into_iter().map(|t| TagDoc { name: t.name, color: t.color }).collect()),
        notification_channels: Some(channel_docs),
        monitors: Some(monitor_docs),
        status_pages: Some(page_docs),
        maintenance: Some(maintenance_docs),
    })
}

/// Orders monitors so every parent comes before its children (stable otherwise).
fn parents_first<'a>(monitors: &[&'a Monitor]) -> Vec<&'a Monitor> {
    let ids: HashSet<i64> = monitors.iter().map(|m| m.id).collect();
    let mut out: Vec<&Monitor> = Vec::with_capacity(monitors.len());
    let mut placed: HashSet<i64> = HashSet::new();
    let mut remaining: Vec<&Monitor> = monitors.to_vec();
    while !remaining.is_empty() {
        let before = remaining.len();
        remaining.retain(|m| {
            let ready = m.parent_id.is_none_or(|p| placed.contains(&p) || !ids.contains(&p));
            if ready {
                placed.insert(m.id);
                out.push(m);
            }
            !ready
        });
        if remaining.len() == before {
            // A cycle (shouldn't exist); emit the rest as they are.
            out.append(&mut remaining);
        }
    }
    out
}

// ===========================================================================
// Plan
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Create and update what the document lists; leave everything else.
    #[default]
    Merge,
    /// Like merge, and delete objects of present sections that the document doesn't list.
    Replace,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "merge" | "" => Some(Mode::Merge),
            "replace" => Some(Mode::Replace),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Merge => "merge",
            Mode::Replace => "replace",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Update,
    Delete,
    Unchanged,
    Skip,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Update => "update",
            Action::Delete => "delete",
            Action::Unchanged => "unchanged",
            Action::Skip => "skip",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Action::Create => crate::i18n::t("backup.new"),
            Action::Update => crate::i18n::t("backup.changed"),
            Action::Delete => crate::i18n::t("backup.deleted"),
            Action::Unchanged => crate::i18n::t("backup.unchanged"),
            Action::Skip => crate::i18n::t("backup.skipped"),
        }
    }
    /// Status class for the plan UI.
    pub fn status_class(self) -> &'static str {
        match self {
            Action::Create => "status-up",
            Action::Update => "status-info",
            Action::Delete => "status-down",
            Action::Unchanged => "status-none",
            Action::Skip => "status-warning",
        }
    }
}

/// What the plan says about one object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    /// `setting` | `tag` | `notification_channel` | `monitor` | `status_page` | `maintenance`.
    pub kind: String,
    pub key: String,
    pub action: Action,
    /// Changed fields (updates).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanError {
    pub kind: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub create: usize,
    pub update: usize,
    pub delete: usize,
    pub unchanged: usize,
    pub skip: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub mode: Mode,
    pub items: Vec<PlanItem>,
    pub errors: Vec<PlanError>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for i in &self.items {
            match i.action {
                Action::Create => s.create += 1,
                Action::Update => s.update += 1,
                Action::Delete => s.delete += 1,
                Action::Unchanged => s.unchanged += 1,
                Action::Skip => s.skip += 1,
            }
        }
        s
    }

    pub fn has_changes(&self) -> bool {
        self.items.iter().any(|i| matches!(i.action, Action::Create | Action::Update | Action::Delete))
    }

    /// The action planned for `(kind, key)`, if any.
    pub fn action_for(&self, kind: &str, key: &str) -> Option<Action> {
        self.items.iter().find(|i| i.kind == kind && i.key == key).map(|i| i.action)
    }

    fn item(&mut self, kind: &str, key: &str, action: Action, changes: Vec<String>) {
        self.items.push(PlanItem { kind: kind.into(), key: key.into(), action, changes, note: None });
    }

    fn skip(&mut self, kind: &str, key: &str, note: impl Into<String>) {
        self.items.push(PlanItem {
            kind: kind.into(),
            key: key.into(),
            action: Action::Skip,
            changes: Vec::new(),
            note: Some(note.into()),
        });
    }

    fn error(&mut self, kind: &str, key: &str, field: Option<&str>, message: impl Into<String>) {
        self.errors.push(PlanError {
            kind: kind.into(),
            key: key.into(),
            field: field.map(str::to_string),
            message: message.into(),
        });
    }
}

pub const KIND_SETTING: &str = "setting";
pub const KIND_TAG: &str = "tag";
pub const KIND_CHANNEL: &str = "notification_channel";
pub const KIND_MONITOR: &str = "monitor";
pub const KIND_PAGE: &str = "status_page";
pub const KIND_MAINTENANCE: &str = "maintenance";

// ===========================================================================
// Prepared operations (resolved ids; negative ids are placeholders for
// objects created earlier in the same apply)
// ===========================================================================

#[derive(Debug, Clone)]
enum TagOp {
    Create { ph: i64, name: String, color: String },
    Update { id: i64, name: String, color: String },
}

#[derive(Debug, Clone)]
struct ChannelWrite {
    name: String,
    kind: String,
    config: String,
    active: bool,
    is_default: bool,
}

#[derive(Debug, Clone)]
enum ChannelOp {
    Create { ph: i64, w: ChannelWrite },
    Update { id: i64, w: ChannelWrite },
}

#[derive(Debug, Clone)]
struct MonitorWrite {
    monitor: NewMonitor,
    /// `None` keeps links (or default channels on create).
    tags: Option<Vec<(i64, String)>>,
    channels: Option<Vec<(i64, bool)>>,
}

#[derive(Debug, Clone)]
enum MonitorOp {
    Create { ph: i64, w: MonitorWrite },
    Update { id: i64, w: MonitorWrite },
}

#[derive(Debug, Clone)]
enum PageOp {
    Create { ph: i64, page: NewStatusPage, sections: Option<Vec<(String, Vec<i64>)>> },
    Update { id: i64, page: NewStatusPage, sections: Option<Vec<(String, Vec<i64>)>> },
}

#[derive(Debug, Clone)]
enum WindowOp {
    Create(db_maint::NewWindow),
    Update(i64, db_maint::NewWindow),
}

#[derive(Debug, Clone, Default)]
struct Ops {
    settings: Vec<(&'static str, Option<String>)>,
    tags: Vec<TagOp>,
    channels: Vec<ChannelOp>,
    monitors: Vec<MonitorOp>,
    pages: Vec<PageOp>,
    windows: Vec<WindowOp>,
    delete_windows: Vec<i64>,
    delete_pages: Vec<i64>,
    delete_monitors: Vec<i64>,
    delete_channels: Vec<i64>,
    delete_tags: Vec<i64>,
}

/// Result of a successful apply.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub plan: Plan,
    /// Monitors created or updated (reload them in the scheduler).
    pub monitor_ids: Vec<i64>,
    /// Monitors deleted (reload stops them).
    pub deleted_monitor_ids: Vec<i64>,
    pub pages_changed: bool,
    pub maintenance_changed: bool,
}

/// Computes the plan for `doc` against the database.
pub fn plan(conn: &Connection, doc: &Document, mode: Mode) -> rusqlite::Result<Plan> {
    Ok(prepare(conn, doc, mode)?.0)
}

/// Applies `doc` in one transaction. `Err(plan)` (nothing written) when the
/// plan has errors.
pub fn apply(conn: &mut Connection, doc: &Document, mode: Mode) -> rusqlite::Result<Result<Applied, Plan>> {
    let tx = conn.transaction()?;
    let (plan, ops) = prepare(&tx, doc, mode)?;
    if !plan.is_ok() {
        return Ok(Err(plan));
    }
    let mut applied = execute(&tx, ops)?;
    tx.commit()?;
    applied.plan = plan;
    Ok(Ok(applied))
}

/// Everything about the current database `prepare` needs.
struct Current {
    settings: db_settings::Settings,
    tags: Vec<db_tags::Tag>,
    channels: Vec<crate::db::models::NotificationChannel>,
    monitors: Vec<Monitor>,
    monitor_keys: HashMap<i64, String>,
    monitor_tags: HashMap<i64, Vec<MonitorTag>>,
    pages: Vec<db_pages::StatusPage>,
    windows: Vec<db_maint::MaintenanceWindow>,
}

impl Current {
    fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let monitors = db_monitors::list_monitors(conn)?;
        Ok(Current {
            settings: db_settings::load(conn)?,
            tags: db_tags::list_tags(conn)?,
            channels: db_notifications::list_channels(conn)?,
            monitor_keys: monitor_keys(&monitors),
            monitors,
            monitor_tags: db_tags::tags_by_monitor(conn)?,
            pages: db_pages::list(conn)?.into_iter().map(|s| s.page).collect(),
            windows: db_maint::list(conn)?,
        })
    }
}

/// Assigns placeholder ids.
#[derive(Default)]
struct Placeholders(i64);

impl Placeholders {
    fn next(&mut self) -> i64 {
        self.0 -= 1;
        self.0
    }
}

fn prepare(conn: &Connection, doc: &Document, mode: Mode) -> rusqlite::Result<(Plan, Ops)> {
    let cur = Current::load(conn)?;
    let mut plan = Plan { mode, ..Plan::default() };
    let mut ops = Ops::default();
    let mut ph = Placeholders::default();

    if let Some(s) = &doc.settings {
        plan_settings(&cur.settings, s, &mut plan, &mut ops);
    }

    // -- tags ---------------------------------------------------------------
    // Final tag set by normalised name → id (placeholder for new ones).
    let mut tag_ids: HashMap<String, i64> = cur.tags.iter().map(|t| (norm(&t.name), t.id)).collect();
    if let Some(tags) = &doc.tags {
        let mut seen = HashSet::new();
        for t in tags {
            let name = t.name.trim();
            if name.is_empty() || name.chars().count() > db_tags::MAX_NAME_LEN {
                plan.error(KIND_TAG, name, Some("name"), format!("Tag names need 1 to {} characters.", db_tags::MAX_NAME_LEN));
                continue;
            }
            if !seen.insert(norm(name)) {
                plan.error(KIND_TAG, name, None, "This tag is listed twice.");
                continue;
            }
            let color = db_tags::normalize_color(&t.color).to_string();
            match cur.tags.iter().find(|x| norm(&x.name) == norm(name)) {
                Some(existing) => {
                    let mut changes = Vec::new();
                    if existing.name != name {
                        changes.push("name".to_string());
                    }
                    if existing.color != color {
                        changes.push("color".to_string());
                    }
                    if changes.is_empty() {
                        plan.item(KIND_TAG, name, Action::Unchanged, changes);
                    } else {
                        plan.item(KIND_TAG, name, Action::Update, changes);
                        ops.tags.push(TagOp::Update { id: existing.id, name: name.into(), color });
                    }
                }
                None => {
                    let id = ph.next();
                    tag_ids.insert(norm(name), id);
                    plan.item(KIND_TAG, name, Action::Create, Vec::new());
                    ops.tags.push(TagOp::Create { ph: id, name: name.into(), color });
                }
            }
        }
        if mode == Mode::Replace {
            for t in &cur.tags {
                if !seen.contains(&norm(&t.name)) {
                    // Tags still used by listed monitors get re-created below if referenced; delete is fine.
                    plan.item(KIND_TAG, &t.name, Action::Delete, Vec::new());
                    ops.delete_tags.push(t.id);
                    tag_ids.remove(&norm(&t.name));
                }
            }
        }
    }

    // -- channels ------------------------------------------------------------
    let channel_keys = unique_keys(cur.channels.iter().map(|c| (c.id, c.name.clone())));
    let mut channel_ids: HashMap<String, i64> = channel_keys.iter().map(|(id, k)| (norm(k), *id)).collect();
    if let Some(channels) = &doc.notification_channels {
        let mut seen = HashSet::new();
        for c in channels {
            let key = c.name.trim().to_string();
            if !seen.insert(norm(&key)) {
                plan.error(KIND_CHANNEL, &key, None, "This channel is listed twice.");
                continue;
            }
            let existing = channel_ids
                .get(&norm(&key))
                .filter(|id| **id > 0)
                .and_then(|id| cur.channels.iter().find(|x| x.id == *id));
            plan_channel(c, &key, existing, &mut plan, &mut ops, &mut ph, &mut channel_ids);
        }
        if mode == Mode::Replace {
            for ch in &cur.channels {
                let key = channel_keys.get(&ch.id).cloned().unwrap_or_default();
                if !seen.contains(&norm(&key)) {
                    plan.item(KIND_CHANNEL, &key, Action::Delete, Vec::new());
                    ops.delete_channels.push(ch.id);
                    channel_ids.remove(&norm(&key));
                }
            }
        }
    }

    // -- monitors ------------------------------------------------------------
    // key (normalised) → id (existing or placeholder) in the final state.
    let mut monitor_ids: HashMap<String, i64> = HashMap::new();
    let mut docker_keys: HashSet<String> = HashSet::new();
    for m in &cur.monitors {
        let key = cur.monitor_keys.get(&m.id).cloned().unwrap_or_default();
        if m.source == "docker" {
            docker_keys.insert(norm(&key));
        }
        monitor_ids.insert(norm(&key), m.id);
    }
    if let Some(monitors) = &doc.monitors {
        plan_monitors(conn, &cur, monitors, mode, &mut tag_ids, &channel_ids, &docker_keys, &mut monitor_ids, &mut plan, &mut ops, &mut ph)?;
    }

    // -- status pages --------------------------------------------------------
    let mut page_ids: HashMap<String, i64> = cur.pages.iter().map(|p| (p.slug.clone(), p.id)).collect();
    if let Some(pages) = &doc.status_pages {
        let mut seen = HashSet::new();
        let mut domains: HashMap<String, String> = HashMap::new();
        for p in pages {
            let slug = p.slug.trim().to_string();
            if !seen.insert(slug.clone()) {
                plan.error(KIND_PAGE, &slug, None, "This status page is listed twice.");
                continue;
            }
            plan_page(conn, &cur, p, &slug, &monitor_ids, &mut domains, &mut plan, &mut ops, &mut ph, &mut page_ids)?;
        }
        // Domains of pages the document doesn't mention must not clash either.
        for existing in &cur.pages {
            if seen.contains(&existing.slug) || (mode == Mode::Replace) {
                continue;
            }
            if let Some(d) = &existing.custom_domain
                && let Some(other) = domains.get(d)
            {
                plan.error(KIND_PAGE, other, Some("custom_domain"), format!("“{}” already uses this domain.", existing.slug));
            }
        }
        if mode == Mode::Replace {
            for existing in &cur.pages {
                if !seen.contains(&existing.slug) {
                    plan.item(KIND_PAGE, &existing.slug, Action::Delete, Vec::new());
                    ops.delete_pages.push(existing.id);
                    page_ids.remove(&existing.slug);
                }
            }
        }
    }

    // -- maintenance ---------------------------------------------------------
    if let Some(windows) = &doc.maintenance {
        let window_keys = unique_keys(cur.windows.iter().map(|w| (w.id, w.title.clone())));
        let by_key: HashMap<String, i64> = window_keys.iter().map(|(id, k)| (norm(k), *id)).collect();
        let mut seen = HashSet::new();
        for w in windows {
            let key = w.title.trim().to_string();
            if !seen.insert(norm(&key)) {
                plan.error(KIND_MAINTENANCE, &key, None, "This maintenance window is listed twice.");
                continue;
            }
            let existing = by_key.get(&norm(&key)).and_then(|id| cur.windows.iter().find(|x| x.id == *id));
            plan_window(conn, w, &key, existing, &monitor_ids, &page_ids, &mut plan, &mut ops)?;
        }
        if mode == Mode::Replace {
            for w in &cur.windows {
                let key = window_keys.get(&w.id).cloned().unwrap_or_default();
                if !seen.contains(&norm(&key)) {
                    plan.item(KIND_MAINTENANCE, &key, Action::Delete, Vec::new());
                    ops.delete_windows.push(w.id);
                }
            }
        }
    }

    Ok((plan, ops))
}

fn plan_settings(cur: &db_settings::Settings, s: &SettingsDoc, plan: &mut Plan, ops: &mut Ops) {
    let mut push = |plan: &mut Plan, key: &'static str, current: String, desired: Option<String>, store: Option<String>| {
        let Some(desired) = desired else { return };
        if current == desired {
            plan.item(KIND_SETTING, key, Action::Unchanged, Vec::new());
        } else {
            plan.item(KIND_SETTING, key, Action::Update, vec![key.to_string()]);
            ops.settings.push((key, store));
        }
    };
    if let Some(name) = &s.site_name {
        let name = name.trim();
        if name.chars().count() > 80 {
            plan.error(KIND_SETTING, db_settings::SITE_NAME, None, "Use at most 80 characters.");
        } else {
            let name = if name.is_empty() { db_settings::Settings::default().site_name } else { name.to_string() };
            push(plan, db_settings::SITE_NAME, cur.site_name.clone(), Some(name.clone()), Some(name));
        }
    }
    if let Some(url) = &s.base_url {
        match crate::api::settings::normalize_base_url(url) {
            Ok(u) => push(plan, db_settings::BASE_URL, cur.base_url.clone().unwrap_or_default(), Some(u.clone().unwrap_or_default()), u),
            Err(msg) => plan.error(KIND_SETTING, db_settings::BASE_URL, None, msg),
        }
    }
    if let Some(tz) = &s.timezone {
        let tz = tz.trim();
        if crate::api::settings::valid_timezone(tz) {
            push(plan, db_settings::TIMEZONE, cur.timezone.clone(), Some(tz.into()), Some(tz.into()));
        } else {
            plan.error(KIND_SETTING, db_settings::TIMEZONE, None, format!("“{tz}” isn’t a time zone this server knows."));
        }
    }
    if let Some(lang) = &s.language {
        if crate::i18n::Lang::from_exact_code(lang).is_some() {
            push(plan, db_settings::LANGUAGE, cur.language.clone(), Some(lang.clone()), Some(lang.clone()));
        } else {
            plan.error(KIND_SETTING, db_settings::LANGUAGE, None, "Unsupported language.");
        }
    }
    if let Some(days) = s.retention_days {
        if (1..=3650).contains(&days) {
            push(plan, db_settings::RETENTION_DAYS, cur.retention_days.to_string(), Some(days.to_string()), Some(days.to_string()));
        } else {
            plan.error(KIND_SETTING, db_settings::RETENTION_DAYS, None, "Enter a number of days between 1 and 3650.");
        }
    }
    let flag = |b: bool| if b { "1".to_string() } else { "0".to_string() };
    for (key, current, desired) in [
        (db_settings::BADGES_PUBLIC, cur.badges_public, s.badges_public),
        (db_settings::METRICS_ENABLED, cur.metrics_enabled, s.metrics_enabled),
        (db_settings::DISCOVERY_ENABLED, cur.discovery_enabled, s.discovery_enabled),
        (db_settings::AGENT_CONFIRMATION_DEFAULT, cur.agent_confirmation_default, s.agent_confirmation_default),
    ] {
        push(plan, key, flag(current), desired.map(flag), desired.map(flag));
    }
    if let Some(host) = &s.discovery_docker_host {
        let host = host.trim();
        match form::validate_docker_host(host) {
            Ok(()) => push(
                plan,
                db_settings::DISCOVERY_DOCKER_HOST,
                cur.discovery_docker_host.clone().unwrap_or_default(),
                Some(host.into()),
                (!host.is_empty()).then(|| host.to_string()),
            ),
            Err(msg) => plan.error(KIND_SETTING, db_settings::DISCOVERY_DOCKER_HOST, None, msg),
        }
    }
}

/// Validates a channel's name, kind and config; returns the config to store.
/// `stored` is the current config (secrets merge from it).
pub fn validate_channel(name: &str, kind: &str, config: &Value, stored: Option<&Value>) -> Result<Value, Vec<(String, String)>> {
    let mut errors = Vec::new();
    if name.trim().is_empty() {
        errors.push(("name".into(), "Give the channel a name, like “Ops Discord”.".into()));
    } else if name.trim().chars().count() > 100 {
        errors.push(("name".into(), "Keep the name under 100 characters.".into()));
    }
    let Some(provider) = notification::provider(kind) else {
        errors.push(("kind".into(), format!("“{kind}” isn’t a supported channel kind.")));
        return Err(errors);
    };
    if !config.is_object() {
        errors.push(("config".into(), "The config must be an object.".into()));
        return Err(errors);
    }
    let mut merged = config.clone();
    // Values of unknown keys are dropped; numbers given as strings stay strings
    // so validation can flag them.
    if let Some(obj) = merged.as_object_mut() {
        obj.retain(|k, _| provider.field(k).is_some());
    }
    // A changed destination (server URL, SMTP host…) never reuses stored
    // secrets: they would be sent to the new place.
    if let Err(field_errors) = schema::merge_secrets_checked(provider, &mut merged, stored.unwrap_or(&Value::Null), &[]) {
        errors.extend(field_errors.into_iter().map(|(field, msg)| (format!("config.{field}"), msg)));
        return Err(errors);
    }
    if let Err(field_errors) = schema::validate(provider, &merged) {
        for (field, msg) in field_errors {
            let secret = provider.field(&field).is_some_and(|f| f.secret);
            let msg = if secret && stored.is_none() && config.get(&field).and_then(Value::as_str) == Some(SECRET_MASK) {
                format!("{msg} (the export didn’t include secrets; export with secrets or fill it in)")
            } else {
                msg
            };
            errors.push((format!("config.{field}"), msg));
        }
    }
    if errors.is_empty() { Ok(merged) } else { Err(errors) }
}

fn plan_channel(
    c: &ChannelDoc,
    key: &str,
    existing: Option<&crate::db::models::NotificationChannel>,
    plan: &mut Plan,
    ops: &mut Ops,
    ph: &mut Placeholders,
    channel_ids: &mut HashMap<String, i64>,
) {
    let stored: Option<Value> = existing.map(|e| serde_json::from_str(&e.config).unwrap_or_else(|_| empty_object()));
    if let Some(e) = existing
        && e.kind != c.kind
    {
        plan.error(KIND_CHANNEL, key, Some("kind"), format!("This channel is a {} channel; its kind can’t change. Delete it or use another name.", e.kind));
        return;
    }
    let config = match validate_channel(&c.name, &c.kind, &c.config, stored.as_ref()) {
        Ok(cfg) => cfg,
        Err(errors) => {
            for (field, msg) in errors {
                plan.error(KIND_CHANNEL, key, Some(&field), msg);
            }
            return;
        }
    };
    let w = ChannelWrite {
        name: c.name.trim().into(),
        kind: c.kind.clone(),
        config: config.to_string(),
        active: c.active,
        is_default: c.is_default,
    };
    match existing {
        Some(e) => {
            let mut changes = Vec::new();
            if e.name != w.name {
                changes.push("name".to_string());
            }
            if e.active != w.active {
                changes.push("active".to_string());
            }
            if e.is_default != w.is_default {
                changes.push("default".to_string());
            }
            if stored.as_ref() != Some(&config) {
                changes.push("config".to_string());
            }
            if changes.is_empty() {
                plan.item(KIND_CHANNEL, key, Action::Unchanged, changes);
            } else {
                plan.item(KIND_CHANNEL, key, Action::Update, changes);
                ops.channels.push(ChannelOp::Update { id: e.id, w });
            }
        }
        None => {
            let id = ph.next();
            channel_ids.insert(norm(key), id);
            plan.item(KIND_CHANNEL, key, Action::Create, Vec::new());
            ops.channels.push(ChannelOp::Create { ph: id, w });
        }
    }
}

/// Builds the desired `NewMonitor` from a document entry (defaults for
/// missing fields), before validation. `Err` = (field, message).
pub fn monitor_from_doc(m: &MonitorDoc) -> Result<NewMonitor, (String, String)> {
    let mut obj = new_monitor_value(&NewMonitor::default());
    for (k, v) in &m.options {
        if MONITOR_SPECIAL.contains(&k.as_str()) || !obj.contains_key(k) {
            return Err((k.clone(), format!("Unknown monitor field “{k}”.")));
        }
        let v = match (k.as_str(), v) {
            // Headers may be written as a mapping.
            ("http_headers", Value::Object(_)) => Value::String(v.to_string()),
            _ => v.clone(),
        };
        obj.insert(k.clone(), v);
    }
    let Some(kind) = MonitorType::parse(m.monitor_type.trim()) else {
        let kinds: Vec<&str> = MonitorType::all().iter().map(|t| t.as_str()).collect();
        return Err(("monitor_type".into(), format!("Unknown monitor type “{}”. Use one of: {}.", m.monitor_type, kinds.join(", "))));
    };
    // Documents written before `auth_method` existed (or with an empty one):
    // a user name means basic auth. An explicit method is respected.
    if m.options.get("auth_method").is_none_or(|a| a.is_null() || a.as_str().is_some_and(|a| a.trim().is_empty())) {
        let user = m.options.get("basic_auth_user").and_then(Value::as_str);
        obj.insert("auth_method".into(), Value::String(crate::monitor::checkers::http_auth::implied_method(user).into()));
    }
    obj.insert("name".into(), Value::String(m.name.clone()));
    obj.insert("monitor_type".into(), serde_json::to_value(kind).unwrap_or(Value::Null));
    obj.insert("target".into(), Value::String(m.target.clone()));
    serde_json::from_value::<NewMonitor>(Value::Object(obj)).map_err(|e| ("options".into(), format!("Invalid monitor field: {e}")))
}

#[allow(clippy::too_many_arguments)]
fn plan_monitors(
    conn: &Connection,
    cur: &Current,
    monitors: &[MonitorDoc],
    mode: Mode,
    tag_ids: &mut HashMap<String, i64>,
    channel_ids: &HashMap<String, i64>,
    docker_keys: &HashSet<String>,
    monitor_ids: &mut HashMap<String, i64>,
    plan: &mut Plan,
    ops: &mut Ops,
    ph: &mut Placeholders,
) -> rusqlite::Result<()> {
    let by_id: HashMap<i64, &Monitor> = cur.monitors.iter().map(|m| (m.id, m)).collect();

    // Pass 1: decide identity (existing id or placeholder) for every entry.
    let mut entries: Vec<(&MonitorDoc, String, i64)> = Vec::new();
    let mut seen = HashSet::new();
    for m in monitors {
        let key = m.key().to_string();
        if key.is_empty() {
            plan.error(KIND_MONITOR, &m.name, Some("name"), "Give the monitor a name.");
            continue;
        }
        if !seen.insert(norm(&key)) {
            plan.error(KIND_MONITOR, &key, None, "This monitor key is listed twice. Give one of them a different `key`.");
            continue;
        }
        if docker_keys.contains(&norm(&key)) {
            plan.skip(KIND_MONITOR, &key, "Managed by Docker discovery");
            continue;
        }
        let id = match monitor_ids.get(&norm(&key)) {
            Some(id) => *id,
            None => {
                let id = ph.next();
                monitor_ids.insert(norm(&key), id);
                id
            }
        };
        entries.push((m, key, id));
    }

    // Replace mode: monitors not listed go away (never Docker-managed ones).
    let mut deleted: HashSet<i64> = HashSet::new();
    if mode == Mode::Replace {
        for m in &cur.monitors {
            let key = cur.monitor_keys.get(&m.id).cloned().unwrap_or_default();
            if m.source != "docker" && !seen.contains(&norm(&key)) {
                plan.item(KIND_MONITOR, &key, Action::Delete, Vec::new());
                ops.delete_monitors.push(m.id);
                deleted.insert(m.id);
                monitor_ids.remove(&norm(&key));
            }
        }
    }

    // Final-state type and parent of every monitor, for group checks.
    let mut final_type: HashMap<i64, MonitorType> = HashMap::new();
    let mut final_parent: HashMap<i64, Option<i64>> = HashMap::new();
    for m in &cur.monitors {
        if !deleted.contains(&m.id) {
            final_type.insert(m.id, m.monitor_type);
            final_parent.insert(m.id, m.parent_id.filter(|p| !deleted.contains(p)));
        }
    }
    let mut desired: Vec<Option<NewMonitor>> = Vec::with_capacity(entries.len());
    for (m, key, id) in &entries {
        match monitor_from_doc(m) {
            Ok(nm) => {
                final_type.insert(*id, nm.monitor_type);
                desired.push(Some(nm));
            }
            Err((field, msg)) => {
                plan.error(KIND_MONITOR, key, Some(&field), msg);
                desired.push(None);
            }
        }
        let parent = match &m.parent {
            None => None,
            Some(p) if p.trim().is_empty() => None,
            Some(p) => match monitor_ids.get(&norm(p)) {
                Some(pid) => Some(*pid),
                None => {
                    plan.error(KIND_MONITOR, key, Some("parent"), format!("No monitor with key “{p}”."));
                    None
                }
            },
        };
        final_parent.insert(*id, parent);
    }
    let groups: Vec<i64> = final_type.iter().filter(|(_, t)| **t == MonitorType::Group).map(|(id, _)| *id).collect();

    // Cycles.
    for (_, key, id) in &entries {
        let mut cursor = final_parent.get(id).copied().flatten();
        let mut steps = 0;
        while let Some(p) = cursor {
            if p == *id || steps > final_parent.len() {
                plan.error(KIND_MONITOR, key, Some("parent"), "A group can’t contain itself.");
                break;
            }
            steps += 1;
            cursor = final_parent.get(&p).copied().flatten();
        }
    }

    // Pass 2: validate each entry with the UI's rules and diff it.
    let mut planned: Vec<(i64, Option<i64>, MonitorOp)> = Vec::new();
    for ((m, key, id), nm) in entries.iter().zip(desired) {
        let Some(mut nm) = nm else { continue };
        let existing = by_id.get(id).copied();
        let parent = final_parent.get(id).copied().flatten();
        nm.parent_id = parent;

        // Secrets the export masked. A masked password stays masked:
        // `form::validate` keeps the stored one only while the target and
        // proxy are unchanged.
        if nm.basic_auth_pass.as_deref() == Some(SECRET_MASK) && existing.is_none_or(|e| e.basic_auth_pass.is_none()) {
            plan.error(KIND_MONITOR, key, Some("basic_auth_pass"), "The password was exported without secrets; fill it in.");
            continue;
        }
        if nm.secret.as_deref() == Some(SECRET_MASK) && existing.is_none_or(|e| e.secret.is_none()) {
            plan.error(KIND_MONITOR, key, Some("secret"), "The secret was exported without secrets; fill it in.");
            continue;
        }
        if nm.oauth_client_secret.as_deref() == Some(SECRET_MASK) && existing.is_none_or(|e| e.oauth_client_secret.is_none()) {
            plan.error(KIND_MONITOR, key, Some("oauth_client_secret"), "The client secret was exported without secrets; fill it in.");
            continue;
        }
        if nm.tls_client_key.as_deref() == Some(SECRET_MASK) && existing.is_none_or(|e| e.tls_client_key.is_none()) {
            plan.error(KIND_MONITOR, key, Some("tls_client_key"), "The private key was exported without secrets; fill it in.");
            continue;
        }
        if let Some(h) = nm.http_headers.clone()
            && h.contains(SECRET_MASK)
            && let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(&h)
        {
            let missing = restore_headers(&mut map, existing.and_then(|e| e.http_headers.as_deref()));
            if !missing.is_empty() {
                plan.error(KIND_MONITOR, key, Some("http_headers"), format!("Header values were exported without secrets: {}.", missing.join(", ")));
                continue;
            }
            nm.http_headers = Some(Value::Object(map).to_string());
        }

        // Tags.
        let tags: Option<Vec<(i64, String)>> = match &m.tags {
            None => None,
            Some(list) => {
                let mut out: Vec<(i64, String)> = Vec::new();
                for t in list {
                    match tag_ids.get(&norm(t.name())) {
                        Some(tid) => out.push((*tid, t.value().trim().to_string())),
                        None => {
                            // Referenced but not declared: create it with the default colour.
                            let name = t.name().trim();
                            if name.is_empty() || name.chars().count() > db_tags::MAX_NAME_LEN {
                                plan.error(KIND_MONITOR, key, Some("tags"), format!("Tag names need 1 to {} characters.", db_tags::MAX_NAME_LEN));
                                continue;
                            }
                            let tid = ph.next();
                            tag_ids.insert(norm(name), tid);
                            ops.tags.push(TagOp::Create { ph: tid, name: name.to_string(), color: db_tags::DEFAULT_COLOR.to_string() });
                            plan.items.push(PlanItem {
                                kind: KIND_TAG.into(),
                                key: name.to_string(),
                                action: Action::Create,
                                changes: Vec::new(),
                                note: Some(format!("Used by “{key}”")),
                            });
                            out.push((tid, t.value().trim().to_string()));
                        }
                    }
                }
                Some(out)
            }
        };
        let channels: Option<Vec<(i64, bool)>> = match &m.notifications {
            None => None,
            Some(list) => {
                let mut out = Vec::new();
                for c in list {
                    match channel_ids.get(&norm(c.name())) {
                        Some(cid) => out.push((*cid, c.escalation())),
                        None => plan.error(KIND_MONITOR, key, Some("notifications"), format!("No notification channel named “{}”.", c.name())),
                    }
                }
                Some(out)
            }
        };
        let current_links = match existing {
            Some(e) => db_notifications::monitor_channel_links(conn, e.id)?,
            None => Vec::new(),
        };
        let effective_channels = channels.clone().unwrap_or_else(|| current_links.clone());

        // The UI's validation, on top of the stored monitor (or defaults).
        let base = existing.map(Monitor::to_new).unwrap_or_default();
        let modes: Vec<(i64, ChannelMode)> = effective_channels
            .iter()
            .map(|(cid, esc)| (*cid, if *esc { ChannelMode::Escalation } else { ChannelMode::Alert }))
            .collect();
        let tag_views: Vec<MonitorTag> = tags
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|(tag_id, value)| MonitorTag { tag_id, name: String::new(), color: String::new(), value })
            .collect();
        let values = FormValues::from_new(&nm, modes, &tag_views);
        let child_count = final_parent.values().filter(|p| **p == Some(*id)).count();
        let cx = FormContext {
            editing_id: existing.map(|e| e.id),
            allowed_parents: groups.iter().copied().filter(|g| g != id).collect(),
            channel_ids: effective_channels.iter().map(|(c, _)| *c).collect(),
            tag_ids: tag_views.iter().map(|t| t.tag_id).collect(),
            child_count,
        };
        let valid = match form::validate(&values, &base, &cx) {
            Ok(v) => v,
            Err(errors) => {
                for (field, msg) in errors {
                    let field = if field == "parent_id" { "parent" } else { field };
                    plan.error(KIND_MONITOR, key, Some(field), msg);
                }
                continue;
            }
        };
        let mut out = valid.monitor;
        // Not part of the monitor form (pause/resume are separate actions there).
        out.sort_order = nm.sort_order;
        out.active = nm.active;

        // Push tokens: keep / set / generate.
        if out.monitor_type == MonitorType::Push {
            match m.push_token.as_deref().map(str::trim) {
                Some(t) if !t.is_empty() && t != SECRET_MASK => {
                    if !valid_push_token(t) {
                        plan.error(KIND_MONITOR, key, Some("push_token"), "Push tokens are 8 to 64 letters, digits, - or _.");
                        continue;
                    }
                    let taken: Option<i64> = conn
                        .query_row("SELECT id FROM monitors WHERE push_token = ?1", params![t], |r| r.get(0))
                        .optional()?;
                    if taken.is_some_and(|other| Some(other) != existing.map(|e| e.id) && !deleted.contains(&other)) {
                        plan.error(KIND_MONITOR, key, Some("push_token"), "Another monitor already uses this push token.");
                        continue;
                    }
                    out.push_token = Some(t.to_string());
                }
                _ => {
                    if out.push_token.is_none() {
                        out.push_token = Some(crate::api::push::generate_push_token());
                    }
                }
            }
        }

        let tags = tags.map(|_| valid.tags.clone());
        let channels = channels.map(|_| valid.channels.clone());
        match existing {
            Some(e) => {
                let mut changes = diff_fields(&e.to_new(), &out);
                if let Some(t) = &tags {
                    let mut now: Vec<(i64, String)> =
                        cur.monitor_tags.get(&e.id).map(|v| v.iter().map(|x| (x.tag_id, x.value.clone())).collect()).unwrap_or_default();
                    let mut want = t.clone();
                    now.sort();
                    want.sort();
                    if now != want {
                        changes.push("tags".into());
                    }
                }
                if let Some(c) = &channels {
                    let mut want = c.clone();
                    want.sort();
                    let mut now = current_links.clone();
                    now.sort();
                    if now != want {
                        changes.push("notifications".into());
                    }
                }
                if changes.is_empty() {
                    plan.item(KIND_MONITOR, key, Action::Unchanged, changes);
                } else {
                    plan.item(KIND_MONITOR, key, Action::Update, changes);
                    planned.push((*id, parent, MonitorOp::Update { id: e.id, w: MonitorWrite { monitor: out, tags, channels } }));
                }
            }
            None => {
                out.source = "config".into();
                out.external_id = Some(key.clone());
                plan.item(KIND_MONITOR, key, Action::Create, Vec::new());
                planned.push((*id, parent, MonitorOp::Create { ph: *id, w: MonitorWrite { monitor: out, tags, channels } }));
            }
        }
    }

    // Parents (created in this run) before children.
    let mut placed: HashSet<i64> = HashSet::new();
    let creating: HashSet<i64> = planned
        .iter()
        .filter_map(|(id, _, op)| matches!(op, MonitorOp::Create { .. }).then_some(*id))
        .collect();
    while !planned.is_empty() {
        let before = planned.len();
        let mut rest = Vec::new();
        for (id, parent, op) in planned {
            if parent.is_none_or(|p| !creating.contains(&p) || placed.contains(&p)) {
                placed.insert(id);
                ops.monitors.push(op);
            } else {
                rest.push((id, parent, op));
            }
        }
        if rest.len() == before {
            // Only possible with a cycle, which is already a plan error.
            break;
        }
        planned = rest;
    }
    Ok(())
}

fn valid_push_token(t: &str) -> bool {
    (8..=64).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Names of the monitor fields that differ.
fn diff_fields(a: &NewMonitor, b: &NewMonitor) -> Vec<String> {
    let (a, b) = (new_monitor_value(a), new_monitor_value(b));
    let mut out: Vec<String> = a
        .iter()
        .filter(|(k, v)| b.get(*k) != Some(*v))
        .map(|(k, _)| if k == "parent_id" { "parent".to_string() } else { k.clone() })
        .collect();
    out.sort();
    out
}

#[allow(clippy::too_many_arguments)]
fn plan_page(
    conn: &Connection,
    cur: &Current,
    p: &StatusPageDoc,
    slug: &str,
    monitor_ids: &HashMap<String, i64>,
    domains: &mut HashMap<String, String>,
    plan: &mut Plan,
    ops: &mut Ops,
    ph: &mut Placeholders,
    page_ids: &mut HashMap<String, i64>,
) -> rusqlite::Result<()> {
    let mut ok = true;
    let mut err = |plan: &mut Plan, field: &str, msg: &str| {
        plan.error(KIND_PAGE, slug, Some(field), msg);
        ok = false;
    };
    let page = match validate_page(p) {
        Ok(page) => Some(page),
        Err(errors) => {
            for (field, msg) in errors {
                err(plan, &field, &msg);
            }
            None
        }
    };
    if let Some(d) = page.as_ref().and_then(|p| p.custom_domain.clone()) {
        if db_pages::is_own_host(&d, cur.settings.base_url.as_deref(), &[]) {
            err(plan, "custom_domain", crate::i18n::t("sp.err_domain_self"));
        }
        if let Some(other) = domains.insert(d, slug.to_string()) {
            err(plan, "custom_domain", &format!("“{other}” already uses this domain."));
        }
    }
    let sections: Option<Vec<(String, Vec<i64>)>> = match &p.sections {
        None => None,
        Some(list) => {
            let mut out = Vec::new();
            for s in list {
                let name = s.name.trim().to_string();
                if name.chars().count() > 80 {
                    err(plan, "sections", "Keep section names to 80 characters or fewer.");
                }
                let mut ids = Vec::new();
                for k in &s.monitors {
                    match monitor_ids.get(&norm(k)) {
                        Some(id) if !ids.contains(id) => ids.push(*id),
                        Some(_) => {}
                        None => err(plan, "sections", &format!("No monitor with key “{k}”.")),
                    }
                }
                out.push((name, ids));
            }
            Some(out)
        }
    };
    let Some(page) = page.filter(|_| ok) else {
        return Ok(());
    };
    match cur.pages.iter().find(|x| x.slug == slug) {
        Some(existing) => {
            let mut changes = Vec::new();
            let old = existing.to_new();
            for (name, differs) in [
                ("title", old.title != page.title),
                ("description", old.description != page.description),
                ("custom_domain", old.custom_domain != page.custom_domain),
                ("published", old.published != page.published),
                ("theme", old.theme != page.theme),
                ("show_tags", old.show_tags != page.show_tags),
                ("show_uptime", old.show_uptime != page.show_uptime),
                ("show_response", old.show_response != page.show_response),
                ("show_powered_by", old.show_powered_by != page.show_powered_by),
                ("footer_text", old.footer_text != page.footer_text),
                ("custom_css", old.custom_css != page.custom_css),
                ("refresh_seconds", old.refresh_seconds != page.refresh_seconds),
            ] {
                if differs {
                    changes.push(name.to_string());
                }
            }
            let sections = match sections {
                Some(want) => {
                    let now: Vec<(String, Vec<i64>)> = db_pages::sections(conn, existing.id)?
                        .into_iter()
                        .map(|s| (s.name, s.monitors.iter().map(|m| m.monitor_id).collect()))
                        .collect();
                    if now == want {
                        None
                    } else {
                        changes.push("sections".into());
                        Some(want)
                    }
                }
                None => None,
            };
            if changes.is_empty() {
                plan.item(KIND_PAGE, slug, Action::Unchanged, changes);
            } else {
                plan.item(KIND_PAGE, slug, Action::Update, changes);
                ops.pages.push(PageOp::Update { id: existing.id, page, sections });
            }
        }
        None => {
            let id = ph.next();
            page_ids.insert(slug.to_string(), id);
            plan.item(KIND_PAGE, slug, Action::Create, Vec::new());
            ops.pages.push(PageOp::Create { ph: id, page, sections });
        }
    }
    Ok(())
}

/// Validates a status page's settings (not its sections, and not slug or
/// domain uniqueness, which need the database). Same rules as the editor.
pub fn validate_page(p: &StatusPageDoc) -> Result<NewStatusPage, Vec<(String, String)>> {
    let mut errors: Vec<(String, String)> = Vec::new();
    let slug = p.slug.trim();
    if let Err(e) = db_pages::validate_slug(slug) {
        errors.push(("slug".into(), e.into()));
    }
    let title = p.title.trim();
    if title.is_empty() || title.chars().count() > 120 {
        errors.push(("title".into(), "Give the page a title of up to 120 characters.".into()));
    }
    let domain = match db_pages::normalize_domain(p.custom_domain.as_deref().unwrap_or_default()) {
        Ok(d) => d,
        Err(e) => {
            errors.push(("custom_domain".into(), e.into()));
            None
        }
    };
    let css = p.custom_css.clone().map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
    if let Some(c) = &css
        && let Err(e) = db_pages::validate_css(c)
    {
        errors.push(("custom_css".into(), e.into()));
    }
    if !db_pages::THEMES.contains(&p.theme.as_str()) {
        errors.push(("theme".into(), "Use auto, light or dark.".into()));
    }
    if p.footer_text.as_deref().is_some_and(|f| f.trim().chars().count() > db_pages::FOOTER_MAX) {
        errors.push((
            "footer_text".into(),
            crate::i18n::tf("sp.err_footer_long", &[("max", &crate::i18n::fmt_int(db_pages::FOOTER_MAX as i64))]),
        ));
    }
    if !(db_pages::REFRESH_MIN..=db_pages::REFRESH_MAX).contains(&p.refresh_seconds) {
        errors.push(("refresh_seconds".into(), format!("Use {} to {} seconds.", db_pages::REFRESH_MIN, db_pages::REFRESH_MAX)));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(NewStatusPage {
        slug: slug.to_string(),
        title: title.to_string(),
        description: p.description.trim().to_string(),
        custom_domain: domain,
        published: p.published,
        theme: p.theme.clone(),
        show_tags: p.show_tags,
        show_uptime: p.show_uptime,
        show_response: p.show_response,
        show_powered_by: p.show_powered_by,
        footer_text: p.footer_text.clone().map(|f| f.trim().to_string()).filter(|f| !f.is_empty()),
        custom_css: css,
        refresh_seconds: p.refresh_seconds,
    })
}

#[allow(clippy::too_many_arguments)]
fn plan_window(
    conn: &Connection,
    w: &MaintenanceDoc,
    key: &str,
    existing: Option<&db_maint::MaintenanceWindow>,
    monitor_ids: &HashMap<String, i64>,
    page_ids: &HashMap<String, i64>,
    plan: &mut Plan,
    ops: &mut Ops,
) -> rusqlite::Result<()> {
    let row = match validate_window(w, monitor_ids, page_ids) {
        Ok(row) => row,
        Err(errors) => {
            for (field, msg) in errors {
                plan.error(KIND_MAINTENANCE, key, Some(&field), msg);
            }
            return Ok(());
        }
    };
    match existing {
        Some(e) => {
            let mut changes = Vec::new();
            for (name, differs) in [
                ("description", e.description != row.description),
                ("strategy", e.strategy != row.strategy),
                ("active", e.active != row.active),
                ("start_at", e.start_at != row.start_at),
                ("end_at", e.end_at != row.end_at),
                ("weekdays", e.weekdays.clone().filter(|d| !d.is_empty()) != row.weekdays),
                ("time_from", e.time_from != row.time_from),
                ("time_to", e.time_to != row.time_to),
                ("timezone", e.timezone != row.timezone),
            ] {
                if differs {
                    changes.push(name.to_string());
                }
            }
            let mut want_m = row.monitor_ids.clone();
            want_m.sort_unstable();
            if db_maint::monitor_ids(conn, e.id)? != want_m {
                changes.push("monitors".into());
            }
            let mut want_p = row.status_page_ids.clone();
            want_p.sort_unstable();
            if db_maint::status_page_ids(conn, e.id)? != want_p {
                changes.push("status_pages".into());
            }
            if changes.is_empty() {
                plan.item(KIND_MAINTENANCE, key, Action::Unchanged, changes);
            } else {
                plan.item(KIND_MAINTENANCE, key, Action::Update, changes);
                ops.windows.push(WindowOp::Update(e.id, row));
            }
        }
        None => {
            plan.item(KIND_MAINTENANCE, key, Action::Create, Vec::new());
            ops.windows.push(WindowOp::Create(row));
        }
    }
    Ok(())
}

/// Validates a maintenance window in its stored form (UTC timestamps, local
/// `HH:MM`). Monitor and page references resolve through the given maps.
pub fn validate_window(
    w: &MaintenanceDoc,
    monitor_ids: &HashMap<String, i64>,
    page_ids: &HashMap<String, i64>,
) -> Result<db_maint::NewWindow, Vec<(String, String)>> {
    use crate::maintenance::{Strategy, fmt_db_ts, parse_db_ts, parse_hhmm, parse_weekdays};
    let mut errors: Vec<(String, String)> = Vec::new();
    let title = w.title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        errors.push(("title".into(), "Give the window a title of up to 200 characters.".into()));
    }
    if w.description.trim().chars().count() > db_maint::DESCRIPTION_MAX {
        errors.push((
            "description".into(),
            crate::i18n::tf("mw.err_description_long", &[("max", &crate::i18n::fmt_int(db_maint::DESCRIPTION_MAX as i64))]),
        ));
    }
    let strategy = match Strategy::parse(w.strategy.trim()) {
        Some(s) => s,
        None => {
            errors.push(("strategy".into(), "Use manual, once, daily or weekly.".into()));
            Strategy::Manual
        }
    };
    let timezone = if w.timezone.trim().is_empty() { "UTC".to_string() } else { w.timezone.trim().to_string() };
    if jiff::tz::TimeZone::get(&timezone).is_err() && timezone != "UTC" {
        errors.push(("timezone".into(), format!("“{timezone}” isn’t a time zone this server knows.")));
    }
    let ts = |errors: &mut Vec<(String, String)>, field: &str, v: &Option<String>| -> Option<String> {
        let v = v.as_deref().map(str::trim).filter(|v| !v.is_empty())?;
        match parse_db_ts(v) {
            Some(t) => Some(fmt_db_ts(t)),
            None => {
                errors.push((field.into(), "Use UTC time like 2026-10-19 22:00:00.".into()));
                None
            }
        }
    };
    let start_at = ts(&mut errors, "start_at", &w.start_at);
    let end_at = ts(&mut errors, "end_at", &w.end_at);
    let hhmm = |errors: &mut Vec<(String, String)>, field: &str, v: &Option<String>| -> Option<String> {
        let v = v.as_deref().map(str::trim).filter(|v| !v.is_empty())?;
        match parse_hhmm(v) {
            Some(t) => Some(t.strftime("%H:%M").to_string()),
            None => {
                errors.push((field.into(), "Use 24-hour time like 22:00.".into()));
                None
            }
        }
    };
    let time_from = hhmm(&mut errors, "time_from", &w.time_from);
    let time_to = hhmm(&mut errors, "time_to", &w.time_to);
    let weekdays = w.weekdays.as_deref().map(parse_weekdays).unwrap_or_default();
    match strategy {
        Strategy::Manual => {}
        Strategy::Once => match (&start_at, &end_at) {
            (Some(s), Some(e)) if e > s => {}
            (Some(_), Some(_)) => errors.push(("end_at".into(), "The end has to be after the start.".into())),
            _ => errors.push(("start_at".into(), "Once windows need start_at and end_at.".into())),
        },
        Strategy::Daily | Strategy::Weekly => {
            if time_from.is_none() || time_to.is_none() {
                errors.push(("time_from".into(), "Recurring windows need time_from and time_to.".into()));
            }
            if strategy == Strategy::Weekly && weekdays.is_empty() {
                errors.push(("weekdays".into(), "Pick at least one day, like \"1,3,5\".".into()));
            }
            if let (Some(a), Some(b)) = (&start_at, &end_at)
                && b <= a
            {
                errors.push(("end_at".into(), "The last date can’t be before the first date.".into()));
            }
        }
    }
    let mut monitor_list = Vec::new();
    for k in &w.monitors {
        match monitor_ids.get(&norm(k)) {
            Some(id) => monitor_list.push(*id),
            None => errors.push(("monitors".into(), format!("No monitor with key “{k}”."))),
        }
    }
    let mut page_list = Vec::new();
    for s in &w.status_pages {
        match page_ids.get(s.trim()) {
            Some(id) => page_list.push(*id),
            None => errors.push(("status_pages".into(), format!("No status page with slug “{s}”."))),
        }
    }
    monitor_list.sort_unstable();
    monitor_list.dedup();
    page_list.sort_unstable();
    page_list.dedup();
    if !errors.is_empty() {
        return Err(errors);
    }
    let recurring = strategy.is_recurring();
    Ok(db_maint::NewWindow {
        title: title.to_string(),
        description: w.description.trim().to_string(),
        strategy: strategy.as_str().into(),
        active: w.active,
        start_at: if strategy == Strategy::Manual { None } else { start_at },
        end_at: if strategy == Strategy::Manual { None } else { end_at },
        weekdays: (strategy == Strategy::Weekly)
            .then(|| weekdays.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(",")),
        time_from: if recurring { time_from } else { None },
        time_to: if recurring { time_to } else { None },
        timezone,
        monitor_ids: monitor_list,
        status_page_ids: page_list,
    })
}

// ===========================================================================
// Execute
// ===========================================================================

fn resolve(map: &HashMap<i64, i64>, id: i64) -> rusqlite::Result<i64> {
    if id > 0 {
        return Ok(id);
    }
    map.get(&id)
        .copied()
        .ok_or_else(|| rusqlite::Error::InvalidParameterName(format!("unresolved placeholder {id}")))
}

fn execute(conn: &Connection, ops: Ops) -> rusqlite::Result<Applied> {
    let mut applied = Applied::default();
    let mut ids: HashMap<i64, i64> = HashMap::new();

    for (key, value) in &ops.settings {
        db_settings::set_opt(conn, key, value.as_deref())?;
    }
    for id in &ops.delete_tags {
        db_tags::delete_tag(conn, *id)?;
    }
    for op in &ops.tags {
        match op {
            TagOp::Create { ph, name, color } => {
                let id = db_tags::insert_tag(conn, name, color)?;
                ids.insert(*ph, id);
            }
            TagOp::Update { id, name, color } => {
                db_tags::update_tag(conn, *id, name, color)?;
            }
        }
    }
    for id in &ops.delete_channels {
        db_notifications::delete_channel(conn, *id)?;
    }
    for op in &ops.channels {
        match op {
            ChannelOp::Create { ph, w } => {
                let id = db_notifications::insert_channel(conn, &w.name, &w.kind, &w.config, false)?;
                if w.is_default {
                    db_notifications::set_default(conn, id, true)?;
                }
                if !w.active {
                    db_notifications::set_active(conn, id, false)?;
                }
                ids.insert(*ph, id);
            }
            ChannelOp::Update { id, w } => {
                db_notifications::update_channel(conn, *id, &w.name, &w.config, w.is_default)?;
                db_notifications::set_active(conn, *id, w.active)?;
            }
        }
    }
    for id in &ops.delete_monitors {
        db_monitors::delete_monitor(conn, *id)?;
        applied.deleted_monitor_ids.push(*id);
    }
    for op in &ops.monitors {
        let (id, w) = match op {
            MonitorOp::Create { ph, w } => {
                let mut m = w.monitor.clone();
                if let Some(p) = m.parent_id {
                    m.parent_id = Some(resolve(&ids, p)?);
                }
                let id = db_monitors::insert_monitor(conn, &m)?;
                ids.insert(*ph, id);
                (id, w)
            }
            MonitorOp::Update { id, w } => {
                let mut m = w.monitor.clone();
                if let Some(p) = m.parent_id {
                    m.parent_id = Some(resolve(&ids, p)?);
                }
                db_monitors::update_monitor(conn, *id, &m)?;
                (*id, w)
            }
        };
        if let Some(tags) = &w.tags {
            let resolved = tags.iter().map(|(t, v)| Ok((resolve(&ids, *t)?, v.clone()))).collect::<rusqlite::Result<Vec<_>>>()?;
            db_tags::set_monitor_tags(conn, id, &resolved)?;
        }
        if let Some(channels) = &w.channels {
            let resolved = channels.iter().map(|(c, e)| Ok((resolve(&ids, *c)?, *e))).collect::<rusqlite::Result<Vec<_>>>()?;
            db_notifications::set_monitor_channels(conn, id, &resolved)?;
        }
        applied.monitor_ids.push(id);
    }
    for id in &ops.delete_pages {
        db_pages::delete(conn, *id)?;
        applied.pages_changed = true;
    }
    for op in &ops.pages {
        applied.pages_changed = true;
        match op {
            PageOp::Create { ph, page, sections } => {
                let id = insert_page(conn, page)?;
                ids.insert(*ph, id);
                let sections = sections.clone().unwrap_or_else(|| vec![("Services".to_string(), Vec::new())]);
                write_sections(conn, id, &sections, &ids)?;
            }
            PageOp::Update { id, page, sections } => {
                db_pages::update(conn, *id, page)?;
                if let Some(sections) = sections {
                    write_sections(conn, *id, sections, &ids)?;
                }
            }
        }
    }
    for id in &ops.delete_windows {
        db_maint::delete(conn, *id)?;
        applied.maintenance_changed = true;
    }
    for op in &ops.windows {
        applied.maintenance_changed = true;
        match op {
            WindowOp::Create(w) => {
                let w = resolve_window(w, &ids)?;
                conn.execute(
                    "INSERT INTO maintenance_windows
                         (title, description, strategy, active, start_at, end_at, weekdays, time_from, time_to, timezone)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![w.title, w.description, w.strategy, w.active, w.start_at, w.end_at, w.weekdays, w.time_from, w.time_to, w.timezone],
                )?;
                write_window_links(conn, conn.last_insert_rowid(), &w)?;
            }
            WindowOp::Update(id, w) => {
                let w = resolve_window(w, &ids)?;
                conn.execute(
                    "UPDATE maintenance_windows SET title = ?2, description = ?3, strategy = ?4, active = ?5, start_at = ?6,
                         end_at = ?7, weekdays = ?8, time_from = ?9, time_to = ?10, timezone = ?11, skip_until = NULL
                     WHERE id = ?1",
                    params![id, w.title, w.description, w.strategy, w.active, w.start_at, w.end_at, w.weekdays, w.time_from, w.time_to, w.timezone],
                )?;
                conn.execute("DELETE FROM maintenance_monitors WHERE maintenance_id = ?1", params![id])?;
                conn.execute("DELETE FROM maintenance_status_pages WHERE maintenance_id = ?1", params![id])?;
                write_window_links(conn, *id, &w)?;
            }
        }
    }
    if !ops.monitors.is_empty() || !ops.delete_monitors.is_empty() {
        // Group membership changes affect maintenance coverage too.
        applied.maintenance_changed = true;
    }
    Ok(applied)
}

fn resolve_window(w: &db_maint::NewWindow, ids: &HashMap<i64, i64>) -> rusqlite::Result<db_maint::NewWindow> {
    let mut w = w.clone();
    w.monitor_ids = w.monitor_ids.iter().map(|id| resolve(ids, *id)).collect::<rusqlite::Result<_>>()?;
    w.status_page_ids = w.status_page_ids.iter().map(|id| resolve(ids, *id)).collect::<rusqlite::Result<_>>()?;
    Ok(w)
}

// `db::status_pages::insert` / `db::maintenance::insert` open their own
// transaction, which can't nest inside the import's, hence these few
// statements of our own.
fn insert_page(conn: &Connection, p: &NewStatusPage) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO status_pages (slug, title, description, custom_domain, published, theme, show_tags, show_uptime,
             show_response, show_powered_by, footer_text, custom_css, refresh_seconds)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            p.slug,
            p.title,
            p.description,
            p.custom_domain,
            p.published,
            p.theme,
            p.show_tags,
            p.show_uptime,
            p.show_response,
            p.show_powered_by,
            p.footer_text,
            p.custom_css,
            p.refresh_seconds
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Replaces a page's sections and their monitors.
pub fn write_sections(conn: &Connection, page_id: i64, sections: &[(String, Vec<i64>)], ids: &HashMap<i64, i64>) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM status_page_sections WHERE status_page_id = ?1", params![page_id])?;
    for (i, (name, monitors)) in sections.iter().enumerate() {
        conn.execute(
            "INSERT INTO status_page_sections (status_page_id, name, sort_order) VALUES (?1, ?2, ?3)",
            params![page_id, name, i as i64],
        )?;
        let sid = conn.last_insert_rowid();
        for (j, mid) in monitors.iter().enumerate() {
            conn.execute(
                "INSERT OR IGNORE INTO status_page_monitors (section_id, monitor_id, sort_order) VALUES (?1, ?2, ?3)",
                params![sid, resolve(ids, *mid)?, j as i64],
            )?;
        }
    }
    conn.execute("UPDATE status_pages SET updated_at = datetime('now') WHERE id = ?1", params![page_id])?;
    Ok(())
}

fn write_window_links(conn: &Connection, id: i64, w: &db_maint::NewWindow) -> rusqlite::Result<()> {
    for mid in &w.monitor_ids {
        conn.execute(
            "INSERT OR IGNORE INTO maintenance_monitors (maintenance_id, monitor_id) VALUES (?1, ?2)",
            params![id, mid],
        )?;
    }
    for pid in &w.status_page_ids {
        conn.execute(
            "INSERT OR IGNORE INTO maintenance_status_pages (maintenance_id, status_page_id) VALUES (?1, ?2)",
            params![id, pid],
        )?;
    }
    Ok(())
}

// ===========================================================================
// Async entry points (scheduler / caches)
// ===========================================================================

/// Why an import didn't happen.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The plan has errors; nothing was written.
    #[error("the document has {} problem(s)", .0.errors.len())]
    Invalid(Plan),
    #[error(transparent)]
    Api(#[from] crate::api::ApiError),
}

/// Plans (and unless `dry_run`, applies) `doc`, then tells the scheduler,
/// maintenance cache and dashboards about the changes.
pub async fn import(state: &AppState, doc: Document, mode: Mode, dry_run: bool) -> Result<Plan, ImportError> {
    if dry_run {
        let plan = state.with_conn(move |c| plan(c, &doc, mode)).await?;
        return Ok(plan);
    }
    let result = state.with_conn(move |c| apply(c, &doc, mode)).await?;
    let applied = result.map_err(ImportError::Invalid)?;
    after_apply(state, &applied).await;
    Ok(applied.plan)
}

/// Post-commit side effects of an import.
/// Whether the plan changes Docker discovery settings.
fn discovery_changed(plan: &Plan) -> bool {
    plan.items.iter().any(|i| {
        i.kind == KIND_SETTING
            && !matches!(i.action, Action::Unchanged)
            && matches!(i.key.as_str(), db_settings::DISCOVERY_ENABLED | db_settings::DISCOVERY_DOCKER_HOST)
    })
}

pub async fn after_apply(state: &AppState, applied: &Applied) {
    for id in applied.deleted_monitor_ids.iter().chain(&applied.monitor_ids) {
        state.scheduler.reload(*id).await;
    }
    if applied.maintenance_changed {
        crate::maintenance::invalidate();
    }
    if applied.pages_changed {
        crate::api::public_status::invalidate_domains();
    }
    if discovery_changed(&applied.plan) {
        // Sync now rather than on the next minute tick.
        crate::discovery::trigger();
    }
    state.notify_update();
    let s = applied.plan.summary();
    tracing::info!(
        mode = applied.plan.mode.as_str(),
        created = s.create,
        updated = s.update,
        deleted = s.delete,
        unchanged = s.unchanged,
        "configuration imported"
    );
}

// ===========================================================================
// Startup hooks
// ===========================================================================

/// If `VV_CONFIG_FILE` is set, merges that file into the database in the
/// background (GitOps style). Problems are logged; the server keeps running.
///
/// Call once after the scheduler has started: `config_io::spawn_startup_import(state.clone());`
pub fn spawn_startup_import(state: AppState) {
    let Some(path) = std::env::var_os(CONFIG_FILE_ENV).filter(|p| !p.is_empty()) else {
        return;
    };
    let path = PathBuf::from(path);
    tokio::spawn(async move {
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(err) => {
                tracing::error!(path = %path.display(), %err, "VV_CONFIG_FILE: can't read the file");
                return;
            }
        };
        let doc = match parse_document(&text) {
            Ok(d) => d,
            Err(err) => {
                tracing::error!(path = %path.display(), %err, "VV_CONFIG_FILE: can't parse the file");
                return;
            }
        };
        match import(&state, doc, Mode::Merge, false).await {
            Ok(plan) => {
                let s = plan.summary();
                tracing::info!(path = %path.display(), created = s.create, updated = s.update, unchanged = s.unchanged, "VV_CONFIG_FILE applied");
            }
            Err(ImportError::Invalid(plan)) => {
                for e in &plan.errors {
                    tracing::error!(kind = %e.kind, key = %e.key, field = ?e.field, message = %e.message, "VV_CONFIG_FILE: invalid entry");
                }
                tracing::error!(path = %path.display(), "VV_CONFIG_FILE not applied: fix the errors above");
            }
            Err(ImportError::Api(err)) => tracing::error!(%err, "VV_CONFIG_FILE: import failed"),
        }
    });
}

/// Checks that `path` is a Vigilant Vole database this build can open:
/// an SQLite file with `schema_migrations`, our core tables, no migrations
/// from a newer version and a passing quick check. Returns the number of monitors.
pub fn validate_backup_db(path: &Path) -> Result<i64, String> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("This isn’t an SQLite database ({e})."))?;
    let has = |table: &str| -> Result<bool, String> {
        conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)", params![table], |r| r.get(0))
            .map_err(|e| format!("This isn’t an SQLite database ({e})."))
    };
    if !has("schema_migrations")? || !has("monitors")? || !has("settings")? || !has("heartbeats")? {
        return Err("This isn’t a Vigilant Vole backup (its tables are missing).".into());
    }
    let applied: Vec<String> = conn
        .prepare("SELECT name FROM schema_migrations")
        .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
        .map_err(|e| format!("Can’t read the migration history ({e})."))?;
    if !applied.iter().any(|n| n == "0002_full_schema.sql") {
        return Err("This backup is from a very old version. Start that version once to upgrade it, then back up again.".into());
    }
    let known: Vec<String> = MigrationFiles::iter().filter(|n| n.ends_with(".sql")).map(|n| n.into_owned()).collect();
    if let Some(newer) = applied.iter().find(|n| !known.contains(n)) {
        return Err(format!("This backup was made by a newer version of Vigilant Vole (migration {newer}). Upgrade first."));
    }
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| format!("The database is damaged ({e})."))?;
    if check != "ok" {
        return Err(format!("The database is damaged ({check})."));
    }
    conn.query_row("SELECT COUNT(*) FROM monitors", [], |r| r.get(0)).map_err(|e| e.to_string())
}

/// Swaps in `DATA_DIR/restore-pending.db` (uploaded on the backup page)
/// before the database is opened. The current database (with its WAL files)
/// is moved to `DATA_DIR/backups/`. Invalid uploads are renamed to
/// `restore-rejected-<ts>.db` and the current database is kept.
///
/// Call at startup before `db::build_pool`: `config_io::apply_pending_restore(&config.data_dir);`
pub fn apply_pending_restore(data_dir: &Path) {
    let pending = data_dir.join(RESTORE_PENDING_FILE);
    if !pending.exists() {
        return;
    }
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
    if let Err(err) = validate_backup_db(&pending) {
        let rejected = data_dir.join(format!("restore-rejected-{stamp}.db"));
        tracing::error!(%err, rejected = %rejected.display(), "pending restore is invalid; keeping the current database");
        if let Err(e) = std::fs::rename(&pending, &rejected) {
            tracing::error!(%e, "couldn't move the rejected restore aside");
        }
        return;
    }
    let db_path = crate::config::Config { data_dir: data_dir.to_path_buf(), bind_addr: String::new() }.db_path();
    let backups = data_dir.join("backups");
    if let Err(err) = std::fs::create_dir_all(&backups) {
        tracing::error!(%err, "can't create the backups folder; restore postponed");
        return;
    }
    if db_path.exists() {
        let base = format!("vigilant-vole-before-restore-{stamp}.db");
        for suffix in ["", "-wal", "-shm"] {
            let from = PathBuf::from(format!("{}{suffix}", db_path.display()));
            if from.exists() {
                let to = backups.join(format!("{base}{suffix}"));
                if let Err(err) = std::fs::rename(&from, &to) {
                    tracing::error!(%err, from = %from.display(), "can't move the current database aside; restore postponed");
                    return;
                }
            }
        }
        tracing::info!(backup = %backups.join(&base).display(), "moved the current database aside");
    }
    match std::fs::rename(&pending, &db_path) {
        Ok(()) => tracing::warn!(path = %db_path.display(), "restored the database from the uploaded backup"),
        Err(err) => tracing::error!(%err, "couldn't move the uploaded backup into place"),
    }
}

/// The migrations this build knows (to reject backups from newer versions).
#[derive(rust_embed::Embed)]
#[folder = "migrations/"]
struct MigrationFiles;

/// Which sections a document has, with their sizes (for previews).
pub fn sections_present(doc: &Document) -> BTreeMap<&'static str, usize> {
    let mut m = BTreeMap::new();
    if doc.settings.is_some() {
        m.insert("settings", 1);
    }
    if let Some(v) = &doc.tags {
        m.insert("tags", v.len());
    }
    if let Some(v) = &doc.notification_channels {
        m.insert("notification_channels", v.len());
    }
    if let Some(v) = &doc.monitors {
        m.insert("monitors", v.len());
    }
    if let Some(v) = &doc.status_pages {
        m.insert("status_pages", v.len());
    }
    if let Some(v) = &doc.maintenance {
        m.insert("maintenance", v.len());
    }
    m
}

#[cfg(test)]
mod tests;
