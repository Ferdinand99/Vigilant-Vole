//! Import from Uptime Kuma. Owned by the api agent.
//!
//! Reads a Kuma `kuma.db` (SQLite, 1.23 and 2.x schemas) or Kuma's legacy
//! JSON backup and turns it into a [`config_io::Document`], so the import gets
//! the same dry-run plan and the same validation as any config import.
//!
//! Everything is read defensively: tables are selected with `SELECT *` and
//! columns looked up by name, so unknown or missing columns don't matter.
//! What can't be mapped is listed in [`KumaImport::skipped`] with a reason.
//! Push tokens are kept, so existing cron jobs keep working once their URL
//! points at this server.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::config_io::{
    self, ChannelDoc, ChannelRef, Document, MaintenanceDoc, Mode, MonitorDoc, Plan, SectionDoc, StatusPageDoc, TagDoc, TagRef,
};
use crate::db::tags as db_tags;

/// Something that was not imported, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    /// `monitor` | `notification` | `status_page` | `maintenance` | `tag`.
    pub kind: String,
    pub name: String,
    pub reason: String,
}

/// The result of reading a Kuma export.
#[derive(Debug, Clone, Default, Serialize)]
pub struct KumaImport {
    /// `"sqlite"` or `"json"`.
    pub source: String,
    pub doc: Document,
    pub skipped: Vec<Skipped>,
    pub warnings: Vec<String>,
}

impl KumaImport {
    fn skip(&mut self, kind: &str, name: &str, reason: impl Into<String>) {
        self.skipped.push(Skipped { kind: kind.into(), name: name.into(), reason: reason.into() });
    }
}

type Row = Map<String, Value>;

// ===========================================================================
// Field access (snake_case DB columns and camelCase JSON alike)
// ===========================================================================

fn get<'a>(row: &'a Row, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().filter_map(|k| row.get(*k)).find(|v| !v.is_null())
}

/// Like [`s`] but keeps the text as stored (secrets and PEM blocks).
fn raw(row: &Row, keys: &[&str]) -> Option<String> {
    get(row, keys).and_then(Value::as_str).filter(|v| !v.trim().is_empty()).map(str::to_string)
}

/// Kuma's `authMethod` and its fields. Basic, OAuth2 client credentials and
/// mTLS map across; NTLM has no equivalent and is reported. Rows without an
/// `auth_method` (older Kuma) mean basic auth when a user name is set.
fn import_auth(row: &Row, opts: &mut Map<String, Value>, warnings: &mut Vec<String>) {
    let basic_user = s(row, &["basic_auth_user", "basicAuthUser"]);
    let method = match s(row, &["auth_method", "authMethod"]).as_deref() {
        Some("basic") => "basic",
        Some("oauth2-cc") => "oauth2",
        Some("mtls") => "mtls",
        Some("ntlm") => {
            warnings.push("NTLM authentication isn’t supported; set up its authentication again.".into());
            return;
        }
        Some(other) => {
            warnings.push(format!("the “{other}” authentication method isn’t supported; set up its authentication again."));
            return;
        }
        None if basic_user.is_some() => "basic",
        None => return,
    };
    opts.insert("auth_method".into(), json!(method));
    match method {
        "basic" => {
            if let Some(user) = basic_user {
                opts.insert("basic_auth_user".into(), json!(user));
            }
            if let Some(pass) = raw(row, &["basic_auth_pass", "basicAuthPass"]) {
                opts.insert("basic_auth_pass".into(), json!(pass));
            }
        }
        "oauth2" => {
            for (ours, kuma) in [
                ("oauth_token_url", ["oauth_token_url", "oauthTokenUrl"]),
                ("oauth_client_id", ["oauth_client_id", "oauthClientId"]),
                ("oauth_audience", ["oauth_audience", "oauthAudience"]),
            ] {
                if let Some(v) = s(row, &kuma) {
                    opts.insert(ours.into(), json!(v));
                }
            }
            if let Some(secret) = raw(row, &["oauth_client_secret", "oauthClientSecret"]) {
                opts.insert("oauth_client_secret".into(), json!(secret));
            }
            // Kuma stores scopes as one space-separated string.
            if let Some(scope) = s(row, &["oauth_scopes", "oauthScopes"]) {
                opts.insert("oauth_scope".into(), json!(scope));
            }
            let style = match s(row, &["oauth_auth_method", "oauthAuthMethod"]).as_deref() {
                Some("client_secret_post") => "client_secret_post",
                _ => "client_secret_basic",
            };
            opts.insert("oauth_client_auth".into(), json!(style));
        }
        _ => {
            for (ours, kuma) in [("tls_client_cert", ["tls_cert", "tlsCert"]), ("tls_ca_cert", ["tls_ca", "tlsCa"])] {
                if let Some(v) = raw(row, &kuma) {
                    opts.insert(ours.into(), json!(v));
                }
            }
            if let Some(key) = raw(row, &["tls_key", "tlsKey"]) {
                opts.insert("tls_client_key".into(), json!(key));
            }
        }
    }
}

fn s(row: &Row, keys: &[&str]) -> Option<String> {
    match get(row, keys)? {
        Value::String(v) => Some(v.trim().to_string()).filter(|v| !v.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn i(row: &Row, keys: &[&str]) -> Option<i64> {
    match get(row, keys)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.round() as i64)),
        Value::String(v) => v.trim().parse::<f64>().ok().map(|f| f.round() as i64),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

fn f(row: &Row, keys: &[&str]) -> Option<f64> {
    match get(row, keys)? {
        Value::Number(n) => n.as_f64(),
        Value::String(v) => v.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn b(row: &Row, keys: &[&str]) -> Option<bool> {
    match get(row, keys)? {
        Value::Bool(v) => Some(*v),
        Value::Number(n) => Some(n.as_f64().is_some_and(|f| f != 0.0)),
        Value::String(v) => Some(matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")),
        _ => None,
    }
}

/// A JSON value that may be stored as JSON text.
fn json_field(row: &Row, keys: &[&str]) -> Option<Value> {
    match get(row, keys)? {
        Value::String(text) => serde_json::from_str(text).ok(),
        other => Some(other.clone()),
    }
}

// ===========================================================================
// Reading the sources
// ===========================================================================

/// SQLite files start with this magic string.
const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

/// Is this (the start of) an SQLite database file?
pub fn is_sqlite(head: &[u8]) -> bool {
    head.starts_with(SQLITE_MAGIC)
}

struct Tables {
    monitors: Vec<Row>,
    tags: Vec<Row>,
    monitor_tags: Vec<Row>,
    notifications: Vec<Row>,
    monitor_notifications: Vec<Row>,
    status_pages: Vec<Row>,
    status_page_domains: Vec<Row>,
    groups: Vec<Row>,
    monitor_groups: Vec<Row>,
    maintenance: Vec<Row>,
    monitor_maintenance: Vec<Row>,
    maintenance_pages: Vec<Row>,
    docker_hosts: Vec<Row>,
    proxies: Vec<Row>,
}

fn read_table(conn: &Connection, table: &str) -> rusqlite::Result<Vec<Row>> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\""))?;
    let names: Vec<String> = stmt.column_names().iter().map(|n| n.to_string()).collect();
    let rows = stmt.query_map([], |r| {
        let mut row = Map::new();
        for (idx, name) in names.iter().enumerate() {
            let v = match r.get_ref(idx)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => Value::from(n),
                ValueRef::Real(x) => Value::from(x),
                ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(_) => Value::Null,
            };
            row.insert(name.clone(), v);
        }
        Ok(row)
    })?;
    rows.collect()
}

/// Reads a Kuma SQLite database (opened read-only).
pub fn read_sqlite(path: &Path) -> Result<KumaImport, String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("Can’t open the database: {e}"))?;
    let load = |t: &str| read_table(&conn, t).map_err(|e| format!("Can’t read the {t} table: {e}"));
    let monitors = load("monitor")?;
    let has_monitor_table: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'monitor')", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if !has_monitor_table {
        return Err("This doesn’t look like an Uptime Kuma database (no monitor table).".into());
    }
    let tables = Tables {
        monitors,
        tags: load("tag")?,
        monitor_tags: load("monitor_tag")?,
        notifications: load("notification")?,
        monitor_notifications: load("monitor_notification")?,
        status_pages: load("status_page")?,
        status_page_domains: load("status_page_cname")?,
        groups: load("group")?,
        monitor_groups: load("monitor_group")?,
        maintenance: load("maintenance")?,
        monitor_maintenance: load("monitor_maintenance")?,
        maintenance_pages: load("maintenance_status_page")?,
        docker_hosts: load("docker_host")?,
        proxies: load("proxy")?,
    };
    Ok(convert(tables, "sqlite"))
}

/// Reads Kuma's legacy JSON backup (`Settings → Backup → Export`, 1.x).
pub fn read_json(text: &str) -> Result<KumaImport, String> {
    let root: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("This isn’t valid JSON: {e}"))?;
    let obj = root.as_object().ok_or("This isn’t an Uptime Kuma backup.")?;
    let list = |key: &str| -> Vec<Row> {
        obj.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_object().cloned()).collect())
            .unwrap_or_default()
    };
    let monitor_list = list("monitorList");
    let notification_list = list("notificationList");
    if monitor_list.is_empty() && notification_list.is_empty() && !obj.contains_key("monitorList") {
        return Err("This isn’t an Uptime Kuma backup (no monitorList).".into());
    }
    // Tags and notification links are embedded in each monitor.
    let mut tags: HashMap<i64, Row> = HashMap::new();
    let mut monitor_tags = Vec::new();
    let mut monitor_notifications = Vec::new();
    for m in &monitor_list {
        let mid = i(m, &["id"]).unwrap_or_default();
        if let Some(Value::Array(list)) = m.get("tags") {
            for t in list.iter().filter_map(Value::as_object) {
                let tid = i(t, &["tag_id", "tagId", "id"]).unwrap_or_default();
                tags.entry(tid).or_insert_with(|| {
                    let mut row = Map::new();
                    row.insert("id".into(), Value::from(tid));
                    row.insert("name".into(), t.get("name").cloned().unwrap_or(Value::Null));
                    row.insert("color".into(), t.get("color").cloned().unwrap_or(Value::Null));
                    row
                });
                let mut link = Map::new();
                link.insert("monitor_id".into(), Value::from(mid));
                link.insert("tag_id".into(), Value::from(tid));
                link.insert("value".into(), t.get("value").cloned().unwrap_or(Value::Null));
                monitor_tags.push(link);
            }
        }
        if let Some(Value::Object(ids)) = m.get("notificationIDList") {
            for (nid, on) in ids {
                if on.as_bool().unwrap_or(false) || on.as_i64() == Some(1) {
                    let mut link = Map::new();
                    link.insert("monitor_id".into(), Value::from(mid));
                    link.insert("notification_id".into(), Value::from(nid.parse::<i64>().unwrap_or_default()));
                    monitor_notifications.push(link);
                }
            }
        }
    }
    let mut tag_rows: Vec<Row> = tags.into_values().collect();
    tag_rows.sort_by_key(|t| i(t, &["id"]));
    let tables = Tables {
        monitors: monitor_list,
        tags: tag_rows,
        monitor_tags,
        notifications: notification_list,
        monitor_notifications,
        status_pages: Vec::new(),
        status_page_domains: Vec::new(),
        groups: Vec::new(),
        monitor_groups: Vec::new(),
        maintenance: Vec::new(),
        monitor_maintenance: Vec::new(),
        maintenance_pages: Vec::new(),
        docker_hosts: Vec::new(),
        proxies: Vec::new(),
    };
    let mut out = convert(tables, "json");
    out.warnings.push("Kuma’s JSON backup has no status pages or maintenance windows; import kuma.db to bring those too.".into());
    Ok(out)
}

/// Reads either format from a file (sniffed by content).
pub fn read_file(path: &Path) -> Result<KumaImport, String> {
    let mut head = [0u8; 16];
    let n = {
        use std::io::Read;
        let mut file = std::fs::File::open(path).map_err(|e| format!("Can’t read the upload: {e}"))?;
        file.read(&mut head).map_err(|e| format!("Can’t read the upload: {e}"))?
    };
    if is_sqlite(&head[..n]) {
        read_sqlite(path)
    } else {
        let text = std::fs::read_to_string(path).map_err(|_| "This is neither an SQLite database nor a JSON backup.".to_string())?;
        read_json(&text)
    }
}

// ===========================================================================
// Conversion
// ===========================================================================

/// Kuma monitor types we can't run, with the reason shown in the plan.
fn unsupported_reason(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "sqlserver" | "mongodb" => "SQL Server and MongoDB monitors aren’t supported yet.",
        "grpc-keyword" => "gRPC keyword monitors call any method and match text in the reply; the gRPC type here runs the standard health check, so add it by hand.",
        "gamedig" => "GameDig monitors aren’t supported; Source-engine servers can use the Steam type.",
        "tailscale-ping" => "Tailscale ping isn’t supported; use a ping monitor on the Tailscale address.",
        "manual" => "Manual monitors aren’t supported; use a push monitor.",
        _ => return None,
    })
}

/// User name and password into the options (the monitor's credential fields).
fn insert_credentials(opts: &mut Map<String, Value>, user: Option<String>, pass: Option<String>) {
    if let Some(user) = user {
        opts.insert("basic_auth_user".into(), json!(user));
    }
    if let Some(pass) = pass {
        opts.insert("basic_auth_pass".into(), json!(pass));
    }
}

/// A database monitor's connection string as our target plus its login:
/// `postgres://user:pass@host:5432/db` becomes `postgres://host:5432/db`.
fn connection_string(row: &Row, kind: &str, warnings: &mut Vec<String>) -> Result<(String, Option<String>, Option<String>), String> {
    let raw = s(row, &["database_connection_string", "databaseConnectionString"]).ok_or("It has no connection string.")?;
    let url = url::Url::parse(raw.trim()).map_err(|_| "Its connection string couldn’t be read.".to_string())?;
    let allowed: &[&str] = match kind {
        "postgres" => &["postgres", "postgresql"],
        "mysql" => &["mysql", "mariadb"],
        _ => &["redis", "rediss"],
    };
    if !allowed.contains(&url.scheme()) {
        return Err(format!("Its connection string uses “{}://”, which this type can’t check.", url.scheme()));
    }
    let host = url.host_str().filter(|h| !h.is_empty()).ok_or("Its connection string has no host.")?;
    let user = Some(url.username()).filter(|u| !u.is_empty()).map(crate::monitor::checkers::endpoint::percent_decode);
    let pass = url.password().map(crate::monitor::checkers::endpoint::percent_decode);
    let path = url.path().trim_matches('/');
    let mut target = format!("{}://{}", url.scheme(), host_port(host, url.port().map(i64::from)));
    if !path.is_empty() {
        target.push('/');
        target.push_str(path);
    }
    if let Some(q) = url.query() {
        target.push('?');
        target.push_str(q);
    }
    if let Some(query) = s(row, &["database_query", "databaseQuery"]).filter(|q| !q.trim().is_empty() && !q.trim().eq_ignore_ascii_case("select 1")) {
        warnings.push(format!("its query “{}” isn’t run; the check is a login and SELECT 1 (or PING).", query.chars().take(60).collect::<String>()));
    }
    Ok((target, user, pass))
}

fn host_port(host: &str, port: Option<i64>) -> String {
    let host = host.trim();
    let bracketed = host.contains(':') && !host.starts_with('[');
    match (port, bracketed) {
        (Some(p), true) => format!("[{host}]:{p}"),
        (Some(p), false) => format!("{host}:{p}"),
        (None, _) => host.to_string(),
    }
}

/// Unique keys for Kuma objects by name (later duplicates get `#<kuma id>`).
fn keyed_names(rows: &[Row]) -> HashMap<i64, String> {
    let mut seen = HashSet::new();
    let mut out = HashMap::new();
    for r in rows {
        let id = i(r, &["id"]).unwrap_or_default();
        let name = s(r, &["name", "title"]).unwrap_or_else(|| format!("Kuma #{id}"));
        let key = if seen.insert(name.to_lowercase()) { name } else { format!("{name} #{id}") };
        out.insert(id, key);
    }
    out
}

fn convert(t: Tables, source: &str) -> KumaImport {
    let mut out = KumaImport { source: source.into(), ..KumaImport::default() };

    // -- tags -----------------------------------------------------------------
    let mut tag_names: HashMap<i64, String> = HashMap::new();
    let mut tag_docs: Vec<TagDoc> = Vec::new();
    let mut seen_tags = HashSet::new();
    for row in &t.tags {
        let id = i(row, &["id"]).unwrap_or_default();
        let Some(name) = s(row, &["name"]) else { continue };
        let name: String = name.chars().take(db_tags::MAX_NAME_LEN).collect();
        tag_names.insert(id, name.clone());
        if seen_tags.insert(name.to_lowercase()) {
            let color = db_tags::normalize_color(&s(row, &["color"]).unwrap_or_default()).to_string();
            tag_docs.push(TagDoc { name, color });
        }
    }

    // -- notifications ----------------------------------------------------------
    let channel_keys = keyed_names(&t.notifications);
    let mut channel_docs = Vec::new();
    let mut channel_names: HashMap<i64, String> = HashMap::new();
    for row in &t.notifications {
        let id = i(row, &["id"]).unwrap_or_default();
        let key = channel_keys.get(&id).cloned().unwrap_or_default();
        let config = match json_field(row, &["config"]) {
            Some(Value::Object(c)) => c,
            _ => {
                out.skip("notification", &key, "Its settings couldn’t be read.");
                continue;
            }
        };
        match map_notification(&config) {
            Ok((kind, cfg, warning)) => {
                if let Some(w) = warning {
                    out.warnings.push(format!("“{key}”: {w}"));
                }
                channel_names.insert(id, key.clone());
                channel_docs.push(ChannelDoc {
                    name: key,
                    kind: kind.into(),
                    active: b(row, &["active"]).unwrap_or(true),
                    is_default: b(row, &["is_default", "isDefault"]).unwrap_or(false),
                    config: Value::Object(cfg),
                });
            }
            Err(reason) => out.skip("notification", &key, reason),
        }
    }

    // -- monitors ---------------------------------------------------------------
    let monitor_keys = keyed_names(&t.monitors);
    let docker_hosts: HashMap<i64, String> = t
        .docker_hosts
        .iter()
        .filter_map(|r| Some((i(r, &["id"])?, s(r, &["docker_daemon", "dockerDaemon"])?)))
        .collect();
    let proxies: HashMap<i64, String> = t
        .proxies
        .iter()
        .filter_map(|r| {
            let protocol = s(r, &["protocol"]).unwrap_or_else(|| "http".into());
            let host = s(r, &["host"])?;
            let port = i(r, &["port"]);
            let auth = if b(r, &["auth"]).unwrap_or(false) {
                match (s(r, &["username"]), s(r, &["password"])) {
                    (Some(u), Some(p)) => format!(
                        "{}:{}@",
                        url::form_urlencoded::byte_serialize(u.as_bytes()).collect::<String>(),
                        url::form_urlencoded::byte_serialize(p.as_bytes()).collect::<String>()
                    ),
                    _ => String::new(),
                }
            } else {
                String::new()
            };
            Some((i(r, &["id"])?, format!("{protocol}://{auth}{}", host_port(&host, port))))
        })
        .collect();
    let mut tags_of: HashMap<i64, Vec<TagRef>> = HashMap::new();
    for link in &t.monitor_tags {
        let (Some(mid), Some(tid)) = (i(link, &["monitor_id"]), i(link, &["tag_id"])) else { continue };
        let Some(name) = tag_names.get(&tid) else { continue };
        let value: String = s(link, &["value"]).unwrap_or_default().chars().take(db_tags::MAX_VALUE_LEN).collect();
        let list = tags_of.entry(mid).or_default();
        if list.iter().any(|t| t.name().eq_ignore_ascii_case(name)) {
            continue;
        }
        list.push(if value.is_empty() { TagRef::Name(name.clone()) } else { TagRef::Full { name: name.clone(), value } });
    }
    let mut channels_of: HashMap<i64, Vec<ChannelRef>> = HashMap::new();
    for link in &t.monitor_notifications {
        let (Some(mid), Some(nid)) = (i(link, &["monitor_id"]), i(link, &["notification_id"])) else { continue };
        if let Some(name) = channel_names.get(&nid) {
            channels_of.entry(mid).or_default().push(ChannelRef::Name(name.clone()));
        }
    }

    let mut monitor_docs = Vec::new();
    let mut imported_monitors: HashMap<i64, String> = HashMap::new();
    for row in &t.monitors {
        let id = i(row, &["id"]).unwrap_or_default();
        let key = monitor_keys.get(&id).cloned().unwrap_or_default();
        match map_monitor(row, &docker_hosts, &proxies) {
            Ok((mut doc, warnings)) => {
                for w in warnings {
                    out.warnings.push(format!("“{key}”: {w}"));
                }
                if doc.name != key {
                    doc.key = Some(key.clone());
                }
                doc.tags = Some(tags_of.remove(&id).unwrap_or_default());
                doc.notifications = Some(channels_of.remove(&id).unwrap_or_default());
                imported_monitors.insert(id, key);
                monitor_docs.push((id, i(row, &["parent"]), doc));
            }
            Err(reason) => out.skip("monitor", &key, reason),
        }
    }
    // Parents by key (only when the parent itself was imported).
    for (_, parent, doc) in &mut monitor_docs {
        if let Some(pid) = parent {
            match imported_monitors.get(pid) {
                Some(pkey) => doc.parent = Some(pkey.clone()),
                None => out.warnings.push(format!("“{}”: its group wasn’t imported, so it’s placed at the top level.", doc.name)),
            }
        }
    }

    // -- status pages -----------------------------------------------------------
    let mut page_docs = Vec::new();
    let mut page_slugs: HashMap<i64, String> = HashMap::new();
    for row in &t.status_pages {
        let id = i(row, &["id"]).unwrap_or_default();
        let raw_slug = s(row, &["slug"]).unwrap_or_default();
        let slug = raw_slug.to_lowercase();
        let title = s(row, &["title"]).unwrap_or_else(|| raw_slug.clone());
        let mut groups: Vec<&Row> = t.groups.iter().filter(|g| i(g, &["status_page_id"]) == Some(id)).collect();
        groups.sort_by_key(|g| (i(g, &["weight"]).unwrap_or_default(), i(g, &["id"]).unwrap_or_default()));
        let sections = groups
            .iter()
            .map(|g| {
                let gid = i(g, &["id"]).unwrap_or_default();
                let mut links: Vec<&Row> = t.monitor_groups.iter().filter(|l| i(l, &["group_id"]) == Some(gid)).collect();
                links.sort_by_key(|l| (i(l, &["weight"]).unwrap_or_default(), i(l, &["id"]).unwrap_or_default()));
                SectionDoc {
                    name: s(g, &["name"]).unwrap_or_else(|| "Services".into()).chars().take(80).collect(),
                    monitors: links.iter().filter_map(|l| imported_monitors.get(&i(l, &["monitor_id"])?).cloned()).collect(),
                }
            })
            .collect::<Vec<_>>();
        let domain = t
            .status_page_domains
            .iter()
            .filter(|d| i(d, &["status_page_id"]) == Some(id))
            .find_map(|d| s(d, &["domain"]));
        if b(row, &["password"]).is_some() && s(row, &["password"]).is_some() {
            out.warnings.push(format!("Status page “{title}”: password protection isn’t supported; the page is imported as a draft."));
        }
        let protected = s(row, &["password"]).is_some();
        let theme = match s(row, &["theme"]).as_deref() {
            Some("dark") => "dark",
            Some("light") => "light",
            _ => "auto",
        };
        page_slugs.insert(id, slug.clone());
        page_docs.push(StatusPageDoc {
            slug,
            title: title.chars().take(120).collect(),
            description: s(row, &["description"]).unwrap_or_default(),
            custom_domain: domain,
            published: b(row, &["published"]).unwrap_or(true) && !protected,
            theme: theme.into(),
            show_tags: b(row, &["show_tags"]).unwrap_or(false),
            show_uptime: true,
            show_response: true,
            show_powered_by: b(row, &["show_powered_by"]).unwrap_or(true),
            footer_text: s(row, &["footer_text"]),
            custom_css: s(row, &["custom_css"]),
            refresh_seconds: i(row, &["auto_refresh_interval"]).unwrap_or(60).clamp(10, 3600),
            sections: Some(sections),
        });
    }

    // -- maintenance -----------------------------------------------------------
    let window_keys = keyed_names(&t.maintenance);
    let mut window_docs = Vec::new();
    for row in &t.maintenance {
        let id = i(row, &["id"]).unwrap_or_default();
        let key = window_keys.get(&id).cloned().unwrap_or_default();
        match map_maintenance(row) {
            Ok(mut w) => {
                w.title = key;
                w.monitors = t
                    .monitor_maintenance
                    .iter()
                    .filter(|l| i(l, &["maintenance_id"]) == Some(id))
                    .filter_map(|l| imported_monitors.get(&i(l, &["monitor_id"])?).cloned())
                    .collect();
                w.status_pages = t
                    .maintenance_pages
                    .iter()
                    .filter(|l| i(l, &["maintenance_id"]) == Some(id))
                    .filter_map(|l| page_slugs.get(&i(l, &["status_page_id"])?).cloned())
                    .collect();
                window_docs.push(w);
            }
            Err(reason) => out.skip("maintenance", &key, reason),
        }
    }

    out.doc = Document {
        version: config_io::DOC_VERSION,
        settings: None,
        tags: Some(tag_docs),
        notification_channels: Some(channel_docs),
        monitors: Some(monitor_docs.into_iter().map(|(_, _, d)| d).collect()),
        status_pages: Some(page_docs),
        maintenance: Some(window_docs),
    };
    out
}

/// Maps one Kuma monitor row. `Err` = reason it's skipped.
fn map_monitor(
    row: &Row,
    docker_hosts: &HashMap<i64, String>,
    proxies: &HashMap<i64, String>,
) -> Result<(MonitorDoc, Vec<String>), String> {
    let mut warnings = Vec::new();
    let kuma_type = s(row, &["type"]).unwrap_or_else(|| "http".into());
    if let Some(reason) = unsupported_reason(&kuma_type) {
        return Err(reason.into());
    }
    let name = s(row, &["name"]).ok_or("It has no name.")?;
    let url = s(row, &["url"]).unwrap_or_default();
    let hostname = s(row, &["hostname"]).unwrap_or_default();
    let port = i(row, &["port"]);
    let mut opts = Map::new();

    let (our_type, target) = match kuma_type.as_str() {
        "http" => ("http", url),
        "real-browser" => {
            warnings.push("real-browser monitors are checked with a plain HTTP request.".into());
            ("http", url)
        }
        "keyword" => ("keyword", url),
        "json-query" => ("json_query", url),
        "port" => ("tcp", host_port(&hostname, port)),
        "ping" => ("ping", hostname),
        "dns" => ("dns", hostname),
        "docker" => ("docker", s(row, &["docker_container", "dockerContainer"]).unwrap_or_default()),
        "push" => ("push", String::new()),
        "steam" => ("steam", host_port(&hostname, port)),
        "group" => ("group", String::new()),
        "websocket-upgrade" | "websocket" => ("websocket", url),
        "postgres" | "mysql" | "redis" => {
            let (target, user, pass) = connection_string(row, &kuma_type, &mut warnings)?;
            insert_credentials(&mut opts, user, pass);
            (if kuma_type == "mysql" { "mysql" } else { &kuma_type }, target)
        }
        "mqtt" => {
            let (scheme, host) = match hostname.split_once("://") {
                Some((sch @ ("mqtt" | "mqtts"), h)) => (sch.to_string(), h.to_string()),
                Some((other, _)) => return Err(format!("MQTT over “{other}://” isn’t supported.")),
                None => ("mqtt".to_string(), hostname.clone()),
            };
            let topic = s(row, &["mqtt_topic", "mqttTopic"]).unwrap_or_default();
            // The topic goes in as written (`mqtt::topic` reads it back
            // raw); only `%` is escaped, since the path is percent-decoded.
            let path = if topic.is_empty() { String::new() } else { format!("/{}", topic.replace('%', "%25")) };
            if let Some(msg) = s(row, &["mqtt_success_message", "mqttSuccessMessage"]) {
                opts.insert("keyword".into(), json!(msg));
            }
            insert_credentials(&mut opts, s(row, &["mqtt_username", "mqttUsername"]), raw(row, &["mqtt_password", "mqttPassword"]));
            ("mqtt", format!("{scheme}://{}{path}", host_port(&host, port)))
        }
        "kafka-producer" => {
            let brokers: Vec<String> = match json_field(row, &["kafka_producer_brokers", "kafkaProducerBrokers"]) {
                Some(Value::Array(list)) => list.iter().filter_map(|b| b.as_str().map(str::to_string)).collect(),
                _ => Vec::new(),
            };
            let first = brokers.first().ok_or("It lists no Kafka brokers.")?;
            if brokers.len() > 1 {
                warnings.push("only its first broker is checked.".into());
            }
            warnings.push("no message is produced; the monitor checks that the broker answers and the topic exists.".into());
            let topic = s(row, &["kafka_producer_topic", "kafkaProducerTopic"]).unwrap_or_default();
            let scheme = if b(row, &["kafka_producer_ssl", "kafkaProducerSsl"]).unwrap_or(false) { "kafkas" } else { "kafka" };
            let sasl = json_field(row, &["kafka_producer_sasl_options", "kafkaProducerSaslOptions"]);
            if let Some(Value::Object(o)) = sasl
                && o.get("mechanism").and_then(Value::as_str).is_some_and(|m| !m.eq_ignore_ascii_case("none"))
            {
                warnings.push("its SASL login isn’t supported, so brokers that require SASL will fail the check.".into());
            }
            let path = if topic.is_empty() { String::new() } else { format!("/{topic}") };
            ("kafka", format!("{scheme}://{first}{path}"))
        }
        "radius" => {
            insert_credentials(&mut opts, s(row, &["radius_username", "radiusUsername"]), raw(row, &["radius_password", "radiusPassword"]));
            if let Some(secret) = raw(row, &["radius_secret", "radiusSecret"]) {
                opts.insert("secret".into(), json!(secret));
            }
            ("radius", format!("radius://{}", host_port(&hostname, port)))
        }
        "snmp" => {
            let version = s(row, &["snmp_version", "snmpVersion"]).unwrap_or_else(|| "2c".into());
            if version.trim() == "3" {
                return Err("SNMPv3 isn’t supported; only v1 and v2c.".into());
            }
            let oid = s(row, &["snmp_oid", "snmpOid"]).unwrap_or_default();
            // Kuma keeps the community string in the RADIUS password column.
            if let Some(community) = raw(row, &["radius_password", "radiusPassword"]) {
                opts.insert("secret".into(), json!(community));
            }
            if let Some(expected) = s(row, &["expected_value", "expectedValue"]) {
                opts.insert("keyword".into(), json!(expected));
                warnings.push("its expected value is matched as text contained in the answer.".into());
            }
            let path = if oid.is_empty() { String::new() } else { format!("/{}", oid.trim_start_matches('.')) };
            ("snmp", format!("snmp://{}{path}?version={}", host_port(&hostname, port), version.trim()))
        }
        "smtp" => {
            let security = s(row, &["smtp_security", "smtpSecurity"]).unwrap_or_default();
            let port = port.or(match security.as_str() {
                "secure" => Some(465),
                "starttls" => Some(587),
                _ => Some(25),
            });
            if security == "secure" && port != Some(465) {
                warnings.push("it uses implicit TLS on a port other than 465, which is checked without TLS here.".into());
            }
            ("smtp", host_port(&hostname, port))
        }
        "rabbitmq" => {
            let nodes: Vec<String> = match json_field(row, &["rabbitmq_nodes", "rabbitmqNodes"]) {
                Some(Value::Array(list)) => list.iter().filter_map(|n| n.as_str().map(str::to_string)).collect(),
                _ => Vec::new(),
            };
            let node = nodes.first().ok_or("It lists no RabbitMQ nodes.")?;
            if nodes.len() > 1 {
                warnings.push("only its first node is checked.".into());
            }
            warnings.push("it checks the management API's alarms endpoint over HTTP, like Uptime Kuma does.".into());
            insert_credentials(&mut opts, s(row, &["rabbitmq_username", "rabbitmqUsername"]), raw(row, &["rabbitmq_password", "rabbitmqPassword"]));
            opts.insert("auth_method".into(), json!("basic"));
            ("http", format!("{}/api/health/checks/alarms/", node.trim_end_matches('/')))
        }
        other => return Err(format!("The “{other}” monitor type isn’t supported.")),
    };

    // Timing.
    let interval = i(row, &["interval"]).unwrap_or(60).clamp(10, 86_400);
    opts.insert("interval_seconds".into(), json!(interval));
    if let Some(retry) = i(row, &["retry_interval", "retryInterval"]).filter(|r| *r > 0 && *r != interval) {
        opts.insert("retry_interval_seconds".into(), json!(retry.clamp(10, 86_400)));
    }
    let timeout = f(row, &["timeout"]).filter(|t| *t > 0.0).map(|t| t.round() as i64).unwrap_or(interval * 4 / 5);
    opts.insert("timeout_seconds".into(), json!(timeout.clamp(1, 300).min(interval)));
    opts.insert("retries".into(), json!(i(row, &["maxretries", "maxRetries"]).unwrap_or(0).clamp(0, 20)));
    let resend = i(row, &["resend_interval", "resendInterval"]).unwrap_or(0).clamp(0, 10_000);
    if resend > 0 {
        opts.insert("resend_interval".into(), json!(resend));
    }
    if b(row, &["upside_down", "upsideDown"]).unwrap_or(false) {
        opts.insert("upside_down".into(), json!(true));
    }
    if !b(row, &["active"]).unwrap_or(true) {
        opts.insert("active".into(), json!(false));
    }
    if let Some(d) = s(row, &["description"]) {
        opts.insert("description".into(), json!(d.chars().take(2_000).collect::<String>()));
    }

    // HTTP family.
    if matches!(our_type, "http" | "keyword" | "json_query") {
        if let Some(m) = s(row, &["method"]) {
            opts.insert("http_method".into(), json!(m.to_ascii_uppercase()));
        }
        if let Some(Value::Array(codes)) = json_field(row, &["accepted_statuscodes_json", "accepted_statuscodes"]) {
            let codes: Vec<String> = codes.iter().filter_map(|c| c.as_str().map(str::to_string).or_else(|| c.as_i64().map(|n| n.to_string()))).collect();
            if !codes.is_empty() {
                opts.insert("accepted_status_codes".into(), json!(codes.join(",")));
            }
        }
        opts.insert("max_redirects".into(), json!(i(row, &["maxredirects", "maxRedirects"]).unwrap_or(10).clamp(0, 30)));
        if b(row, &["ignore_tls", "ignoreTls"]).unwrap_or(false) {
            opts.insert("ignore_tls".into(), json!(true));
        }
        if let Some(h) = s(row, &["headers"]) {
            match serde_json::from_str::<Value>(&h) {
                Ok(Value::Object(map)) if !map.is_empty() => {
                    opts.insert("http_headers".into(), Value::Object(map));
                }
                Ok(Value::Object(_)) => {}
                _ => warnings.push("its headers aren’t a JSON object and were left out.".into()),
            }
        }
        if let Some(body) = get(row, &["body"]).and_then(Value::as_str).filter(|b| !b.trim().is_empty()) {
            opts.insert("http_body".into(), json!(body));
            let encoding = match s(row, &["http_body_encoding", "httpBodyEncoding"]).as_deref() {
                Some("form") => "form",
                Some("json") | None => {
                    if serde_json::from_str::<Value>(body).is_ok() { "json" } else { "raw" }
                }
                Some(_) => "raw",
            };
            opts.insert("http_body_encoding".into(), json!(encoding));
        }
        import_auth(row, &mut opts, &mut warnings);
        if let Some(pid) = i(row, &["proxy_id", "proxyId"])
            && let Some(proxy) = proxies.get(&pid)
        {
            opts.insert("proxy_url".into(), json!(proxy));
        }
        opts.insert("expiry_notify".into(), json!(b(row, &["expiry_notification", "expiryNotification"]).unwrap_or(false)));
    }
    if our_type == "keyword" {
        opts.insert("keyword".into(), json!(get(row, &["keyword"]).and_then(Value::as_str).unwrap_or_default()));
        if b(row, &["invert_keyword", "invertKeyword"]).unwrap_or(false) {
            opts.insert("keyword_invert".into(), json!(true));
        }
    }
    if our_type == "json_query" {
        let mut path = s(row, &["json_path", "jsonPath"]).unwrap_or_default();
        if !path.starts_with('$') {
            path = format!("$.{path}");
        }
        if serde_json_path::JsonPath::parse(&path).is_err() {
            return Err(format!("Its JSONata query “{path}” has no JSONPath equivalent."));
        }
        opts.insert("json_path".into(), json!(path));
        let op = s(row, &["json_path_operator", "jsonPathOperator"]).unwrap_or_else(|| "==".into());
        let op = match op.as_str() {
            "=" => "==".to_string(),
            other => other.to_string(),
        };
        opts.insert("json_operator".into(), json!(op));
        opts.insert("json_expected".into(), json!(s(row, &["expected_value", "expectedValue"]).unwrap_or_default()));
    }
    if our_type == "dns" {
        if let Some(rt) = s(row, &["dns_resolve_type", "dnsResolveType"]) {
            opts.insert("dns_record_type".into(), json!(rt.to_ascii_uppercase()));
        }
        if let Some(server) = s(row, &["dns_resolve_server", "dnsResolveServer"]) {
            let server = server.split(',').next().unwrap_or_default().trim().to_string();
            let resolver = match port.filter(|p| *p != 53) {
                Some(p) => host_port(&server, Some(p)),
                None => server,
            };
            opts.insert("dns_resolver".into(), json!(resolver));
        }
    }
    if our_type == "docker"
        && let Some(hid) = i(row, &["docker_host", "dockerHost"])
        && let Some(host) = docker_hosts.get(&hid)
    {
        opts.insert("docker_host".into(), json!(host));
    }
    let push_token = (our_type == "push").then(|| s(row, &["push_token", "pushToken"])).flatten();

    Ok((
        MonitorDoc {
            key: None,
            name: name.chars().take(150).collect(),
            monitor_type: our_type.into(),
            target,
            parent: None,
            options: opts,
            tags: None,
            notifications: None,
            push_token,
        },
        warnings,
    ))
}

fn put(out: &mut Row, key: &str, v: Option<String>) {
    if let Some(v) = v {
        out.insert(key.into(), Value::String(v));
    }
}

/// Maps a Kuma notification config (with its `type`) to one of ours:
/// `(kind, config, warning)`.
fn map_notification(c: &Row) -> Result<(&'static str, Row, Option<String>), String> {
    let kind = s(c, &["type"]).unwrap_or_default();
    let mut out = Map::new();
    let mut warning = None;
    let ours: &'static str = match kind.to_ascii_lowercase().as_str() {
        "discord" => {
            put(&mut out, "url", s(c, &["discordWebhookUrl"]));
            put(&mut out, "username", s(c, &["discordUsername"]));
            "discord"
        }
        "slack" => {
            put(&mut out, "url", s(c, &["slackwebhookURL"]));
            put(&mut out, "channel", s(c, &["slackchannel"]));
            put(&mut out, "username", s(c, &["slackusername"]));
            put(&mut out, "icon_emoji", s(c, &["slackiconemo"]));
            "slack"
        }
        "telegram" => {
            put(&mut out, "bot_token", raw(c, &["telegramBotToken"]));
            put(&mut out, "chat_id", s(c, &["telegramChatID"]));
            put(&mut out, "server_url", s(c, &["telegramServerUrl"]));
            if let Some(t) = i(c, &["telegramMessageThreadID"]) {
                out.insert("message_thread_id".into(), json!(t));
            }
            out.insert("silent".into(), json!(b(c, &["telegramSendSilently"]).unwrap_or(false)));
            "telegram"
        }
        "webhook" => {
            put(&mut out, "url", s(c, &["webhookURL"]));
            let content = s(c, &["webhookContentType"]).unwrap_or_else(|| "json".into());
            out.insert("content_type".into(), json!(if content == "form-data" { "form" } else { "json" }));
            if content == "custom" {
                put(&mut out, "body_template", s(c, &["webhookCustomBody"]));
                warning = Some("its custom body uses Kuma’s template variables; check it.".to_string());
            }
            if let Some(Value::Object(h)) = s(c, &["webhookAdditionalHeaders"]).and_then(|h| serde_json::from_str(&h).ok()) {
                out.insert("headers".into(), Value::Object(h));
            }
            "webhook"
        }
        "smtp" => {
            put(&mut out, "host", s(c, &["smtpHost"]));
            if let Some(p) = i(c, &["smtpPort"]) {
                out.insert("port".into(), json!(p));
            }
            let secure = b(c, &["smtpSecure"]).unwrap_or(false);
            out.insert("security".into(), json!(if secure { "tls" } else { "starttls" }));
            out.insert("ignore_tls".into(), json!(b(c, &["smtpIgnoreTLSError"]).unwrap_or(false)));
            put(&mut out, "username", s(c, &["smtpUsername"]));
            put(&mut out, "password", raw(c, &["smtpPassword"]));
            put(&mut out, "from", s(c, &["smtpFrom"]));
            put(&mut out, "to", s(c, &["smtpTo"]));
            put(&mut out, "cc", s(c, &["smtpCC"]));
            put(&mut out, "bcc", s(c, &["smtpBCC"]));
            "email"
        }
        "ntfy" => {
            put(&mut out, "server", s(c, &["ntfyserverurl"]));
            put(&mut out, "topic", s(c, &["ntfytopic"]));
            let priority = i(c, &["ntfyPriority"]).filter(|p| (1..=5).contains(p)).map(|p| p.to_string());
            out.insert("priority".into(), json!(priority.unwrap_or_else(|| "auto".into())));
            put(&mut out, "access_token", raw(c, &["ntfyaccesstoken"]));
            put(&mut out, "username", s(c, &["ntfyusername"]));
            put(&mut out, "password", raw(c, &["ntfypassword"]));
            "ntfy"
        }
        "gotify" => {
            put(&mut out, "server", s(c, &["gotifyserverurl"]));
            put(&mut out, "app_token", raw(c, &["gotifyapplicationToken"]));
            if let Some(p) = i(c, &["gotifyPriority"]) {
                out.insert("priority".into(), json!(p));
            }
            "gotify"
        }
        "pushover" => {
            put(&mut out, "user_key", raw(c, &["pushoveruserkey"]));
            put(&mut out, "app_token", raw(c, &["pushoverapptoken"]));
            put(&mut out, "device", s(c, &["pushoverdevice"]));
            put(&mut out, "sound", s(c, &["pushoversounds"]));
            if let Some(p) = i(c, &["pushoverpriority"]) {
                out.insert("down_priority".into(), json!(p.clamp(0, 2).to_string()));
                out.insert("priority".into(), json!(p.clamp(-2, 1).to_string()));
            }
            "pushover"
        }
        "teams" => {
            put(&mut out, "url", s(c, &["webhookUrl"]));
            warning = Some("Teams now needs a Workflows webhook URL; old connector URLs stop working.".to_string());
            "teams"
        }
        "matrix" => {
            put(&mut out, "homeserver", s(c, &["homeserverUrl"]));
            put(&mut out, "room_id", s(c, &["internalRoomId"]));
            put(&mut out, "access_token", raw(c, &["accessToken"]));
            "matrix"
        }
        "mattermost" => {
            put(&mut out, "url", s(c, &["mattermostWebhookUrl"]));
            put(&mut out, "username", s(c, &["mattermostusername"]));
            put(&mut out, "channel", s(c, &["mattermostchannel"]));
            put(&mut out, "icon_url", s(c, &["mattermosticonurl"]));
            "mattermost"
        }
        "rocket.chat" => {
            put(&mut out, "url", s(c, &["rocketwebhookURL"]));
            put(&mut out, "channel", s(c, &["rocketchannel"]));
            put(&mut out, "username", s(c, &["rocketusername"]));
            put(&mut out, "emoji", s(c, &["rocketiconemo"]));
            "rocketchat"
        }
        "googlechat" => {
            put(&mut out, "url", s(c, &["googleChatWebhookURL"]));
            "googlechat"
        }
        "pagerduty" => {
            put(&mut out, "routing_key", raw(c, &["pagerdutyIntegrationKey"]));
            let severity = s(c, &["pagerdutyPriority"]).filter(|p| ["critical", "error", "warning", "info"].contains(&p.as_str()));
            out.insert("severity".into(), json!(severity.unwrap_or_else(|| "critical".into())));
            let eu = s(c, &["pagerdutyIntegrationUrl"]).is_some_and(|u| u.contains(".eu."));
            out.insert("region".into(), json!(if eu { "eu" } else { "us" }));
            "pagerduty"
        }
        "opsgenie" => {
            put(&mut out, "api_key", raw(c, &["opsgenieApiKey"]));
            out.insert("region".into(), json!(if s(c, &["opsgenieRegion"]).as_deref() == Some("eu") { "eu" } else { "us" }));
            let p = i(c, &["opsgeniePriority"]).unwrap_or(1).clamp(1, 5);
            out.insert("priority".into(), json!(format!("P{p}")));
            "opsgenie"
        }
        "pushbullet" => {
            put(&mut out, "access_token", raw(c, &["pushbulletAccessToken"]));
            "pushbullet"
        }
        "signal" => {
            put(&mut out, "url", s(c, &["signalURL"]));
            put(&mut out, "number", s(c, &["signalNumber"]));
            put(&mut out, "recipients", s(c, &["signalRecipients"]));
            "signal"
        }
        "apprise" => {
            put(&mut out, "urls", s(c, &["appriseURL"]));
            warning = Some("Apprise here talks to an Apprise API server; add its URL to the channel.".to_string());
            "apprise"
        }
        "homeassistant" => {
            put(&mut out, "url", s(c, &["homeAssistantUrl"]));
            put(&mut out, "token", raw(c, &["longLivedAccessToken"]));
            put(&mut out, "service", s(c, &["notificationService"]).map(|svc| svc.trim_start_matches("notify.").to_string()));
            "homeassistant"
        }
        "bark" => {
            let endpoint = s(c, &["barkEndpoint"]).unwrap_or_default();
            let endpoint = endpoint.trim_end_matches('/');
            match endpoint.rsplit_once('/') {
                Some((server, key)) if server.contains("://") && !key.is_empty() => {
                    put(&mut out, "server", Some(server.to_string()));
                    put(&mut out, "device_key", Some(key.to_string()));
                }
                _ => put(&mut out, "server", Some(endpoint.to_string())),
            }
            put(&mut out, "group", s(c, &["barkGroup"]));
            put(&mut out, "sound", s(c, &["barkSound"]));
            "bark"
        }
        "feishu" => {
            put(&mut out, "url", s(c, &["feishuWebHookUrl"]));
            "feishu"
        }
        "dingding" => {
            put(&mut out, "url", s(c, &["webHookUrl"]));
            put(&mut out, "secret", raw(c, &["secretKey"]));
            "dingtalk"
        }
        "twilio" => {
            put(&mut out, "account_sid", s(c, &["twilioAccountSID"]));
            put(&mut out, "auth_token", raw(c, &["twilioAuthToken"]));
            put(&mut out, "from", s(c, &["twilioFromNumber"]));
            put(&mut out, "to", s(c, &["twilioToNumber"]));
            "twilio"
        }
        "" => return Err("It has no provider type.".into()),
        other => return Err(format!("The “{other}” provider isn’t supported; Apprise or a webhook can usually reach it.")),
    };
    Ok((ours, out, warning))
}

/// Parses Kuma's start/end time: `"HH:MM[:SS]"` or `{"hours": h, "minutes": m}`.
fn kuma_time(row: &Row, keys: &[&str]) -> Option<String> {
    match json_field(row, keys).or_else(|| get(row, keys).cloned())? {
        Value::Object(o) => {
            let h = o.get("hours").and_then(Value::as_i64)?;
            let m = o.get("minutes").and_then(Value::as_i64).unwrap_or(0);
            Some(format!("{h:02}:{m:02}"))
        }
        Value::String(t) => crate::maintenance::parse_hhmm(&t).map(|t| t.strftime("%H:%M").to_string()),
        _ => None,
    }
}

/// Kuma stores dates as local wall-clock text in the window's time zone.
fn kuma_date_to_utc(text: &str, tz: &jiff::tz::TimeZone) -> Option<String> {
    let ts = crate::maintenance::parse_local_datetime(text.trim(), tz)?;
    Some(crate::maintenance::fmt_db_ts(ts))
}

fn map_maintenance(row: &Row) -> Result<MaintenanceDoc, String> {
    let strategy_raw = s(row, &["strategy"]).unwrap_or_else(|| "manual".into());
    let tz_name = s(row, &["timezone"]).filter(|t| t != "SAME_AS_SERVER" && jiff::tz::TimeZone::get(t).is_ok()).unwrap_or_else(|| "UTC".into());
    let tz = jiff::tz::TimeZone::get(&tz_name).unwrap_or(jiff::tz::TimeZone::UTC);
    let start = s(row, &["start_date"]).and_then(|d| kuma_date_to_utc(&d, &tz));
    let end = s(row, &["end_date"]).and_then(|d| kuma_date_to_utc(&d, &tz));
    let mut w = MaintenanceDoc {
        title: String::new(),
        description: s(row, &["description"]).unwrap_or_default(),
        strategy: "manual".into(),
        active: b(row, &["active"]).unwrap_or(true),
        start_at: None,
        end_at: None,
        weekdays: None,
        time_from: None,
        time_to: None,
        timezone: tz_name,
        monitors: Vec::new(),
        status_pages: Vec::new(),
    };
    match strategy_raw.as_str() {
        "manual" => {}
        "single" => {
            w.strategy = "once".into();
            if start.is_none() || end.is_none() {
                return Err("It has no start or end date.".into());
            }
            w.start_at = start;
            w.end_at = end;
        }
        "recurring-weekday" | "recurring-interval" => {
            if strategy_raw == "recurring-interval" && i(row, &["interval_day"]).unwrap_or(1) != 1 {
                return Err("“Every N days” schedules aren’t supported; only daily and weekly.".into());
            }
            let (from, to) = (kuma_time(row, &["start_time"]), kuma_time(row, &["end_time"]));
            let (Some(from), Some(to)) = (from, to) else {
                return Err("Its start and end times couldn’t be read.".into());
            };
            w.time_from = Some(from);
            w.time_to = Some(to);
            w.start_at = start;
            w.end_at = end;
            if strategy_raw == "recurring-weekday" {
                w.strategy = "weekly".into();
                let days: Vec<String> = match json_field(row, &["weekdays"]) {
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(Value::as_i64)
                        // Kuma: 0 = Sunday … 6 = Saturday; ours: ISO 1 = Monday … 7 = Sunday.
                        .map(|d| if d == 0 { 7 } else { d })
                        .filter(|d| (1..=7).contains(d))
                        .map(|d| d.to_string())
                        .collect(),
                    _ => Vec::new(),
                };
                if days.is_empty() {
                    return Err("It has no weekdays.".into());
                }
                w.weekdays = Some(days.join(","));
            } else {
                w.strategy = "daily".into();
            }
        }
        "recurring-day-of-month" => return Err("Day-of-month schedules aren’t supported; only daily and weekly.".into()),
        "cron" => return Err("Cron schedules aren’t supported; recreate it as a daily or weekly window.".into()),
        other => return Err(format!("The “{other}” schedule isn’t supported.")),
    }
    Ok(w)
}

// ===========================================================================
// Planning (with automatic skipping of entries that don't validate)
// ===========================================================================

/// Removes objects that fail validation (recording why) until the plan is
/// clean, then returns that plan. Kuma data is messy; one bad monitor must
/// not block the rest.
pub fn plan(conn: &Connection, import: &mut KumaImport) -> rusqlite::Result<Plan> {
    for _ in 0..8 {
        let plan = config_io::plan(conn, &import.doc, Mode::Merge)?;
        if plan.errors.is_empty() {
            return Ok(plan);
        }
        let mut removed = false;
        // Referenced objects first: dropping a bad channel or tag also drops the
        // references to it, which may be the only problem its monitors have.
        let referenced = |k: &str| k == config_io::KIND_CHANNEL || k == config_io::KIND_TAG;
        let first_round = plan.errors.iter().any(|e| referenced(&e.kind));
        for e in plan.errors.iter().filter(|e| !first_round || referenced(&e.kind)) {
            let reason = match &e.field {
                Some(field) => format!("{} ({field})", e.message),
                None => e.message.clone(),
            };
            removed |= remove(import, &e.kind, &e.key, &reason);
        }
        if !removed {
            return Ok(plan);
        }
    }
    config_io::plan(conn, &import.doc, Mode::Merge)
}

/// Drops one object (and references to it). Returns whether anything was removed.
fn remove(import: &mut KumaImport, kind: &str, key: &str, reason: &str) -> bool {
    let doc = &mut import.doc;
    let eq = |a: &str| a.trim().eq_ignore_ascii_case(key.trim());
    let mut removed = false;
    match kind {
        config_io::KIND_MONITOR => {
            if let Some(list) = &mut doc.monitors {
                let before = list.len();
                list.retain(|m| !eq(m.key()));
                removed = list.len() != before;
                for m in list.iter_mut() {
                    if m.parent.as_deref().is_some_and(eq) {
                        m.parent = None;
                    }
                }
            }
            for p in doc.status_pages.iter_mut().flatten() {
                for s in p.sections.iter_mut().flatten() {
                    s.monitors.retain(|k| !eq(k));
                }
            }
            for w in doc.maintenance.iter_mut().flatten() {
                w.monitors.retain(|k| !eq(k));
            }
            if removed {
                import.skipped.push(Skipped { kind: "monitor".into(), name: key.into(), reason: reason.into() });
            }
        }
        config_io::KIND_CHANNEL => {
            if let Some(list) = &mut doc.notification_channels {
                let before = list.len();
                list.retain(|c| !eq(&c.name));
                removed = list.len() != before;
            }
            for m in doc.monitors.iter_mut().flatten() {
                if let Some(n) = &mut m.notifications {
                    n.retain(|c| !eq(c.name()));
                }
            }
            if removed {
                import.skipped.push(Skipped { kind: "notification".into(), name: key.into(), reason: reason.into() });
            }
        }
        config_io::KIND_TAG => {
            if let Some(list) = &mut doc.tags {
                let before = list.len();
                list.retain(|t| !eq(&t.name));
                removed = list.len() != before;
            }
            for m in doc.monitors.iter_mut().flatten() {
                if let Some(t) = &mut m.tags {
                    t.retain(|t| !eq(t.name()));
                }
            }
            if removed {
                import.skipped.push(Skipped { kind: "tag".into(), name: key.into(), reason: reason.into() });
            }
        }
        config_io::KIND_PAGE => {
            if let Some(list) = &mut doc.status_pages {
                let before = list.len();
                list.retain(|p| !eq(&p.slug));
                removed = list.len() != before;
            }
            for w in doc.maintenance.iter_mut().flatten() {
                w.status_pages.retain(|s| !eq(s));
            }
            if removed {
                import.skipped.push(Skipped { kind: "status_page".into(), name: key.into(), reason: reason.into() });
            }
        }
        config_io::KIND_MAINTENANCE => {
            if let Some(list) = &mut doc.maintenance {
                let before = list.len();
                list.retain(|w| !eq(&w.title));
                removed = list.len() != before;
            }
            if removed {
                import.skipped.push(Skipped { kind: "maintenance".into(), name: key.into(), reason: reason.into() });
            }
        }
        _ => {}
    }
    removed
}

#[cfg(test)]
mod tests;
