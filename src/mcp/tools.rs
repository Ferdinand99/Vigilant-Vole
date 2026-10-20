//! The MCP tools and resources. Each tool is a thin layer over a REST
//! endpoint (`/api/v1/**`, called inside the process with the caller's own
//! token), so validation, roles and masking are the REST API's.

use std::sync::{LazyLock, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, header};
use serde_json::{Map, Value, json};
use tower::ServiceExt as _;

use super::{Caller, RpcResult};
use crate::api::AppState;

/// Longest tool answer, in characters.
const MAX_TEXT: usize = 100_000;
/// Replaces secret values.
const MASK: &str = "••••••";

/// Headers passed on to the REST API (language, and the host for absolute URLs).
const FORWARDED: &[&str] = &["host", "x-forwarded-host", "x-forwarded-proto", "accept-language"];

pub(crate) struct Ctx {
    state: AppState,
    caller: Caller,
    forwarded: HeaderMap,
}

impl Ctx {
    pub fn new(state: AppState, caller: Caller, incoming: &HeaderMap) -> Self {
        let mut forwarded = HeaderMap::new();
        for name in FORWARDED {
            if let Some(value) = incoming.get(*name) {
                forwarded.insert(HeaderName::from_static(name), value.clone());
            }
        }
        Ctx { state, caller, forwarded }
    }

    pub fn can_write(&self) -> bool {
        self.caller.can_write
    }

    /// The REST API's router for this instance (built once per instance).
    fn router(&self) -> Router {
        static ROUTERS: LazyLock<Mutex<std::collections::HashMap<usize, Router>>> =
            LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
        let key = std::sync::Arc::as_ptr(&self.state.config) as usize;
        let mut map = ROUTERS.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(key).or_insert_with(|| crate::api::build_router(self.state.clone())).clone()
    }

    /// Calls the REST API as the caller. `Ok(Null)` for empty answers.
    async fn api(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, ToolError> {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, self.caller.authorization.clone())
            .header(header::ACCEPT, "application/json");
        for (name, value) in &self.forwarded {
            builder = builder.header(name, value);
        }
        let request = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap_or_default())),
            None => builder.body(Body::empty()),
        }
        .map_err(|e| ToolError::Internal(e.to_string()))?;
        let response = self
            .router()
            .oneshot(request)
            .await
            .map_err(|e| ToolError::Internal(e.to_string()))?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .map_err(|e| ToolError::Internal(e.to_string()))?;
        let value: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
        if status.is_success() {
            return Ok(value);
        }
        let error = value.get("error").cloned().unwrap_or(Value::Null);
        let mut message = error.get("message").and_then(Value::as_str).unwrap_or("the request failed").to_string();
        if let Some(fields) = error.get("fields").and_then(Value::as_object) {
            for (field, why) in fields {
                if let Some(why) = why.as_str() {
                    message.push_str(&format!(" [{field}: {why}]"));
                }
            }
        }
        Err(match status.as_u16() {
            401 | 403 => ToolError::Denied(message),
            code => ToolError::Api(code, message),
        })
    }

    async fn get(&self, path: &str) -> Result<Value, ToolError> {
        self.api(Method::GET, path, None).await
    }
}

#[derive(Debug)]
enum ToolError {
    /// The arguments are wrong.
    Invalid(String),
    /// The token or its owner may not do this.
    Denied(String),
    /// The REST API said no.
    Api(u16, String),
    Internal(String),
}

impl ToolError {
    fn text(&self) -> String {
        match self {
            ToolError::Invalid(m) => format!("Invalid arguments: {m}"),
            ToolError::Denied(m) => format!("Not allowed: {m}. This token (or the user it belongs to) can't do that."),
            ToolError::Api(code, m) => format!("Vigilant Vole answered {code}: {m}"),
            ToolError::Internal(m) => format!("Internal error: {m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------

struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    write: bool,
    /// Overwrites or changes many things at once.
    heavy: bool,
    schema: Value,
}

fn object(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

fn id_prop(what: &str) -> Value {
    json!({"type": "integer", "minimum": 1, "description": what})
}

static TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| {
    let monitor_id = || object(json!({"id": id_prop("The monitor's id (from list_monitors).")}), &["id"]);
    vec![
        Tool {
            name: "instance_summary",
            title: "Instance summary",
            description: "Start here. Version, how many monitors are up, down, pending, paused or in maintenance, which monitors are down right now, how many status pages and remote agents exist.",
            write: false,
            heavy: false,
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "list_monitors",
            title: "List monitors",
            description: "List monitors with their current status. Filter by status, type, tag or a search text (matches name, target and description). Paginated: pass `cursor` from `next_cursor`.",
            write: false,
            heavy: false,
            schema: object(
                json!({
                    "status": {"type": "string", "enum": ["up", "down", "pending", "maintenance", "paused", "unknown"], "description": "Only monitors in this state."},
                    "type": {"type": "string", "description": "Monitor type, e.g. http, tcp, ping, dns, tls, push."},
                    "tag": {"type": "string", "description": "Tag name."},
                    "q": {"type": "string", "description": "Search text."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Page size (default 50)."},
                    "cursor": {"type": "string", "description": "`next_cursor` of the previous page."}
                }),
                &[],
            ),
        },
        Tool {
            name: "get_monitor",
            title: "Get a monitor",
            description: "One monitor with all its settings, tags and notification links. Passwords and tokens are masked.",
            write: false,
            heavy: false,
            schema: monitor_id(),
        },
        Tool {
            name: "monitor_history",
            title: "Monitor history",
            description: "Uptime over the last N days (with one entry per day) and the most recent checks (status, response time, message) within that period.",
            write: false,
            heavy: false,
            schema: object(
                json!({
                    "id": id_prop("The monitor's id."),
                    "days": {"type": "integer", "minimum": 1, "maximum": 365, "description": "Period in days (default 7)."},
                    "checks": {"type": "integer", "minimum": 0, "maximum": 100, "description": "How many recent checks to include (default 20, 0 for none)."}
                }),
                &["id"],
            ),
        },
        Tool {
            name: "list_events",
            title: "Monitor events",
            description: "A monitor's outages, maintenance periods, pauses and other events, newest first, with durations.",
            write: false,
            heavy: false,
            schema: object(
                json!({"id": id_prop("The monitor's id."), "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "How many events (default 20)."}}),
                &["id"],
            ),
        },
        Tool {
            name: "why_is_it_down",
            title: "Why is it down?",
            description: "Everything useful for diagnosing one monitor in a single call: its current status and settings, the ongoing outage (if any) with how long it has lasted, the last events, the last checks with their error messages, and certificate / domain expiry.",
            write: false,
            heavy: false,
            schema: monitor_id(),
        },
        Tool {
            name: "list_status_pages",
            title: "List status pages",
            description: "The public status pages (id, slug, title, whether published and protected).",
            write: false,
            heavy: false,
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "list_incidents",
            title: "List incidents",
            description: "Status page incidents (the announcements people post), newest first. Give `status_page_id` for one page, or leave it out for all pages. `open_only` hides resolved ones.",
            write: false,
            heavy: false,
            schema: object(
                json!({
                    "status_page_id": id_prop("Only this status page."),
                    "open_only": {"type": "boolean", "description": "Only incidents that aren't resolved."}
                }),
                &[],
            ),
        },
        Tool {
            name: "list_maintenance",
            title: "List maintenance windows",
            description: "Maintenance windows with their schedule, current state (active, scheduled, paused, ended) and the monitors they cover.",
            write: false,
            heavy: false,
            schema: object(json!({}), &[]),
        },
        Tool {
            name: "create_monitor",
            title: "Create a monitor",
            description: "Create a monitor. Needs at least `name`, `monitor_type` (http, keyword, tcp, ping, dns, tls, push, …) and usually `target`. Same fields and validation as the REST API: interval_seconds, timeout_seconds, retries, tags, notifications, … Unknown tag names are created.",
            write: true,
            heavy: false,
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "monitor_type": {"type": "string"},
                    "target": {"type": "string"},
                    "interval_seconds": {"type": "integer", "minimum": 5},
                    "retries": {"type": "integer", "minimum": 0}
                },
                "required": ["name", "monitor_type"],
                "additionalProperties": true
            }),
        },
        Tool {
            name: "update_monitor",
            title: "Change a monitor",
            description: "Change some fields of a monitor (everything not mentioned stays). Send \"••••••\" for a secret to keep its stored value.",
            write: true,
            heavy: true,
            schema: json!({
                "type": "object",
                "properties": {"id": id_prop("The monitor's id.")},
                "required": ["id"],
                "additionalProperties": true
            }),
        },
        Tool {
            name: "pause_monitor",
            title: "Pause a monitor",
            description: "Stop checks and alerts for a monitor.",
            write: true,
            heavy: false,
            schema: monitor_id(),
        },
        Tool {
            name: "resume_monitor",
            title: "Resume a monitor",
            description: "Start a paused monitor again.",
            write: true,
            heavy: false,
            schema: monitor_id(),
        },
        Tool {
            name: "check_now",
            title: "Check now",
            description: "Run a check right now and return its result. Not possible for paused, push and group monitors.",
            write: true,
            heavy: false,
            schema: monitor_id(),
        },
        Tool {
            name: "bulk_edit",
            title: "Change many monitors",
            description: "Change settings on many monitors at once, chosen by `ids` or by a `filter` (same fields as list_monitors: status, type, tag, q). `set` holds the settings (interval_seconds, retries, run_on_agents, channels {add, remove}, tags {add, remove}, …) with the semantics of POST /api/v1/monitors/bulk. One transaction; settings that don't fit a monitor's type are skipped and listed.",
            write: true,
            heavy: true,
            schema: json!({
                "type": "object",
                "properties": {
                    "ids": {"type": "array", "items": {"type": "integer"}},
                    "filter": {"type": "object", "additionalProperties": true},
                    "set": {"type": "object", "additionalProperties": true}
                },
                "required": ["set"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "create_incident",
            title: "Post an incident",
            description: "Announce an incident on a status page. Severity: info, warning, danger or success.",
            write: true,
            heavy: false,
            schema: object(
                json!({
                    "status_page_id": id_prop("The status page's id (from list_status_pages)."),
                    "title": {"type": "string"},
                    "body": {"type": "string", "description": "Markdown: **bold**, *italic*, [links](https://…)."},
                    "severity": {"type": "string", "enum": ["info", "warning", "danger", "success"]},
                    "pinned": {"type": "boolean", "description": "Show at the top while open (default true)."}
                }),
                &["status_page_id", "title"],
            ),
        },
        Tool {
            name: "update_incident",
            title: "Update an incident",
            description: "Change an incident and/or post a timeline update. `update` = {status: investigating | identified | monitoring | resolved, body}; a resolved update resolves the incident, any other status reopens it. `resolved` resolves or reopens without a timeline entry.",
            write: true,
            heavy: false,
            schema: object(
                json!({
                    "status_page_id": id_prop("The status page's id."),
                    "incident_id": id_prop("The incident's id (from list_incidents)."),
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "severity": {"type": "string", "enum": ["info", "warning", "danger", "success"]},
                    "pinned": {"type": "boolean"},
                    "resolved": {"type": "boolean"},
                    "update": {
                        "type": "object",
                        "properties": {
                            "status": {"type": "string", "enum": ["investigating", "identified", "monitoring", "resolved"]},
                            "body": {"type": "string"}
                        },
                        "required": ["status", "body"],
                        "additionalProperties": false
                    }
                }),
                &["status_page_id", "incident_id"],
            ),
        },
        Tool {
            name: "start_maintenance",
            title: "Start maintenance",
            description: "Put monitors into maintenance right now: creates a manual maintenance window that stays on until end_maintenance. Monitors in it record maintenance and send no alerts. Optionally announce it on status pages.",
            write: true,
            heavy: false,
            schema: object(
                json!({
                    "title": {"type": "string"},
                    "description": {"type": "string", "description": "Shown on status pages (Markdown)."},
                    "monitor_ids": {"type": "array", "items": {"type": "integer"}, "minItems": 1, "description": "A group covers everything in it."},
                    "status_page_ids": {"type": "array", "items": {"type": "integer"}}
                }),
                &["title", "monitor_ids"],
            ),
        },
        Tool {
            name: "end_maintenance",
            title: "End maintenance",
            description: "Turn a maintenance window off (a manual window ends; a scheduled one is paused until you enable it again).",
            write: true,
            heavy: false,
            schema: object(json!({"id": id_prop("The maintenance window's id (from list_maintenance).")}), &["id"]),
        },
    ]
});

/// The tool list for `tools/list` (read tools only for read tokens).
pub fn list(can_write: bool) -> Vec<Value> {
    TOOLS
        .iter()
        .filter(|t| can_write || !t.write)
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": t.schema,
                "annotations": {
                    "title": t.title,
                    "readOnlyHint": !t.write,
                    "destructiveHint": t.write && t.heavy,
                    "idempotentHint": !t.write,
                    "openWorldHint": false
                }
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

type Args = Map<String, Value>;

fn id_arg(args: &Args, key: &str) -> Result<i64, ToolError> {
    match args.get(key) {
        Some(v) => v
            .as_i64()
            .filter(|n| *n >= 1)
            .ok_or_else(|| ToolError::Invalid(format!("`{key}` must be a positive integer"))),
        None => Err(ToolError::Invalid(format!("`{key}` is required"))),
    }
}

fn opt_str<'a>(args: &'a Args, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(ToolError::Invalid(format!("`{key}` must be a string"))),
    }
}

fn opt_int(args: &Args, key: &str, min: i64, max: i64) -> Result<Option<i64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_i64() {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(ToolError::Invalid(format!("`{key}` must be an integer from {min} to {max}"))),
        },
    }
}

fn opt_bool(args: &Args, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ToolError::Invalid(format!("`{key}` must be true or false"))),
    }
}

fn query(pairs: &[(&str, Option<String>)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        if let Some(v) = v {
            ser.append_pair(k, v);
        }
    }
    let q = ser.finish();
    if q.is_empty() { String::new() } else { format!("?{q}") }
}

/// Arguments minus some keys, as a JSON body.
fn body_without(args: &Args, drop: &[&str]) -> Value {
    Value::Object(args.iter().filter(|(k, _)| !drop.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect())
}

// ---------------------------------------------------------------------------
// Masking
// ---------------------------------------------------------------------------

fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["password", "passwd", "secret", "token", "api_key", "apikey", "private_key", "community", "authorization", "cookie", "webhook"]
        .iter()
        .any(|needle| k.contains(needle))
        || k == "pass"
        || k.ends_with("_pass")
        || k.ends_with("_key") && k != "monitor_key"
}

/// `https://user:pw@host` → `https://user:••••••@host`.
fn mask_userinfo(text: &str) -> Option<String> {
    let (scheme, rest) = text.split_once("://")?;
    let (authority, tail) = rest.split_once('/').map_or((rest, ""), |(a, t)| (a, t));
    let (userinfo, host) = authority.rsplit_once('@')?;
    let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
    let slash = if rest.contains('/') { "/" } else { "" };
    Some(format!("{scheme}://{user}:{MASK}@{host}{slash}{tail}"))
}

/// Masks secrets anywhere in an answer, whatever the REST API already did.
pub fn mask(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if key == "http_headers" {
                    if let Value::Object(h) = v {
                        for hv in h.values_mut() {
                            *hv = json!(MASK);
                        }
                    } else if v.as_str().is_some_and(|s| !s.is_empty()) {
                        *v = json!(MASK);
                    }
                } else if is_secret_key(key) {
                    if v.as_str().is_some_and(|s| !s.is_empty() && s != MASK) {
                        *v = json!(MASK);
                    } else if v.is_object() || v.is_array() {
                        mask(v);
                    }
                } else {
                    mask(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask),
        Value::String(s) => {
            if let Some(masked) = mask_userinfo(s) {
                *s = masked;
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

fn result(value: &Value, is_error: bool) -> Value {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    if text.chars().count() > MAX_TEXT {
        let cut: String = text.chars().take(MAX_TEXT).collect();
        text = format!("{cut}\n… (truncated: ask for less, with `limit` or a filter)");
    }
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn error_result(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

pub async fn call(ctx: &Ctx, name: &str, args: Args) -> RpcResult {
    let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
        return Err((-32602, format!("Unknown tool: {name}")));
    };
    if tool.write && !ctx.caller.can_write {
        return Ok(error_result(
            "This token is read-only (or its user can't edit), so it can't change anything. Create a write token under Settings → API tokens if you want that.",
        ));
    }
    match run(ctx, name, &args).await {
        Ok(mut value) => {
            mask(&mut value);
            Ok(result(&value, false))
        }
        Err(e) => Ok(error_result(&e.text())),
    }
}

async fn run(ctx: &Ctx, name: &str, args: &Args) -> Result<Value, ToolError> {
    match name {
        "instance_summary" => instance_summary(ctx).await,
        "list_monitors" => {
            let q = query(&[
                ("status", opt_str(args, "status")?.map(str::to_string)),
                ("type", opt_str(args, "type")?.map(str::to_string)),
                ("tag", opt_str(args, "tag")?.map(str::to_string)),
                ("q", opt_str(args, "q")?.map(str::to_string)),
                ("limit", Some(opt_int(args, "limit", 1, 200)?.unwrap_or(50).to_string())),
                ("cursor", opt_str(args, "cursor")?.map(str::to_string)),
            ]);
            ctx.get(&format!("/api/v1/monitors{q}")).await
        }
        "get_monitor" => ctx.get(&format!("/api/v1/monitors/{}", id_arg(args, "id")?)).await,
        "monitor_history" => monitor_history(ctx, args).await,
        "list_events" => {
            let id = id_arg(args, "id")?;
            let limit = opt_int(args, "limit", 1, 200)?.unwrap_or(20);
            ctx.get(&format!("/api/v1/monitors/{id}/events?limit={limit}")).await
        }
        "why_is_it_down" => why_is_it_down(ctx, id_arg(args, "id")?).await,
        "list_status_pages" => ctx.get("/api/v1/status-pages").await,
        "list_incidents" => list_incidents(ctx, args).await,
        "list_maintenance" => ctx.get("/api/v1/maintenance").await,
        "create_monitor" => {
            for key in ["name", "monitor_type"] {
                if opt_str(args, key)?.is_none_or(|s| s.trim().is_empty()) {
                    return Err(ToolError::Invalid(format!("`{key}` is required")));
                }
            }
            ctx.api(Method::POST, "/api/v1/monitors", Some(Value::Object(args.clone()))).await
        }
        "update_monitor" => {
            let id = id_arg(args, "id")?;
            let body = body_without(args, &["id"]);
            if body.as_object().is_some_and(Map::is_empty) {
                return Err(ToolError::Invalid("give at least one field to change".into()));
            }
            ctx.api(Method::PATCH, &format!("/api/v1/monitors/{id}"), Some(body)).await
        }
        "pause_monitor" => ctx.api(Method::POST, &format!("/api/v1/monitors/{}/pause", id_arg(args, "id")?), None).await,
        "resume_monitor" => ctx.api(Method::POST, &format!("/api/v1/monitors/{}/resume", id_arg(args, "id")?), None).await,
        "check_now" => ctx.api(Method::POST, &format!("/api/v1/monitors/{}/check", id_arg(args, "id")?), None).await,
        "bulk_edit" => {
            if !args.get("set").is_some_and(Value::is_object) {
                return Err(ToolError::Invalid("`set` must be an object".into()));
            }
            if !args.contains_key("ids") && !args.contains_key("filter") {
                return Err(ToolError::Invalid("give `ids` or a `filter`".into()));
            }
            ctx.api(Method::POST, "/api/v1/monitors/bulk", Some(Value::Object(args.clone()))).await
        }
        "create_incident" => {
            let page = id_arg(args, "status_page_id")?;
            if opt_str(args, "title")?.is_none_or(|s| s.trim().is_empty()) {
                return Err(ToolError::Invalid("`title` is required".into()));
            }
            opt_bool(args, "pinned")?;
            ctx.api(Method::POST, &format!("/api/v1/status-pages/{page}/incidents"), Some(body_without(args, &["status_page_id"])))
                .await
        }
        "update_incident" => update_incident(ctx, args).await,
        "start_maintenance" => {
            if opt_str(args, "title")?.is_none_or(|s| s.trim().is_empty()) {
                return Err(ToolError::Invalid("`title` is required".into()));
            }
            let monitors = args.get("monitor_ids").and_then(Value::as_array).filter(|a| !a.is_empty());
            if monitors.is_none() {
                return Err(ToolError::Invalid("`monitor_ids` must list at least one monitor".into()));
            }
            let mut body = body_without(args, &[]);
            body["strategy"] = json!("manual");
            body["active"] = json!(true);
            ctx.api(Method::POST, "/api/v1/maintenance", Some(body)).await
        }
        "end_maintenance" => {
            ctx.api(Method::PATCH, &format!("/api/v1/maintenance/{}", id_arg(args, "id")?), Some(json!({"active": false}))).await
        }
        other => Err(ToolError::Internal(format!("tool {other} has no implementation"))),
    }
}

async fn instance_summary(ctx: &Ctx) -> Result<Value, ToolError> {
    let health = ctx.get("/api/v1/health").await?;
    let down = ctx.get("/api/v1/monitors?status=down&limit=25").await?;
    let pending = ctx.get("/api/v1/monitors?status=pending&limit=25").await?;
    let brief = |list: &Value| -> Vec<Value> {
        list["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|m| json!({"id": m["id"], "name": m["name"], "monitor_type": m["monitor_type"], "target": m["target"], "status": m["status"]}))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = json!({
        "version": health["version"],
        "uptime_seconds": health["uptime_seconds"],
        "monitors": health["monitors"],
        "status_pages": health["status_pages"],
        "agents": health["agents"],
        "down_now": brief(&down),
        "pending_now": brief(&pending),
    });
    // Agent details are admin-only; leave them out for everyone else.
    if ctx.caller.user.is_admin()
        && let Ok(agents) = ctx.get("/api/v1/agents").await
    {
        out["agent_list"] = agents["data"].clone();
    }
    Ok(out)
}

async fn monitor_history(ctx: &Ctx, args: &Args) -> Result<Value, ToolError> {
    let id = id_arg(args, "id")?;
    let days = opt_int(args, "days", 1, 365)?.unwrap_or(7);
    let checks = opt_int(args, "checks", 0, 100)?.unwrap_or(20);
    let uptime = ctx.get(&format!("/api/v1/monitors/{id}/uptime?days={days}")).await?;
    let mut out = json!({"monitor_id": id, "days": days, "uptime": uptime});
    if checks > 0 {
        let since = crate::maintenance::fmt_db_ts(jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * days));
        let q = query(&[("since", Some(since)), ("limit", Some(checks.to_string()))]);
        // Push, group and agent-only monitors have no local checks.
        match ctx.get(&format!("/api/v1/monitors/{id}/heartbeats{q}")).await {
            Ok(h) => out["recent_checks"] = h["data"].clone(),
            Err(ToolError::Api(code, _)) if code == 404 || code == 409 => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

async fn why_is_it_down(ctx: &Ctx, id: i64) -> Result<Value, ToolError> {
    let monitor = ctx.get(&format!("/api/v1/monitors/{id}")).await?;
    let events = ctx.get(&format!("/api/v1/monitors/{id}/events?limit=5")).await?;
    let checks = ctx
        .get(&format!("/api/v1/monitors/{id}/heartbeats?limit=5"))
        .await
        .map(|h| h["data"].clone())
        .unwrap_or(Value::Null);
    let event_list = events["data"].as_array().cloned().unwrap_or_default();
    let ongoing = event_list.iter().find(|e| e["ended_at"].is_null() && e["kind"] == "down").cloned();
    let expiry = ctx
        .state
        .with_conn(move |c| {
            let mut stmt = c.prepare("SELECT kind, subject, issuer, expires_at FROM monitor_expiry WHERE monitor_id = ?1 ORDER BY kind")?;
            let rows = stmt.query_map([id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .map_err(|e| ToolError::Internal(e.to_string()))?;
    let now = jiff::Timestamp::now();
    let expiries: Vec<Value> = expiry
        .into_iter()
        .map(|(kind, subject, issuer, expires_at)| {
            let days_left = crate::maintenance::parse_db_ts(&expires_at).map(|t| now.duration_until(t).as_secs().div_euclid(86_400));
            json!({"kind": if kind == "cert" { "certificate" } else { "domain" }, "subject": subject, "issuer": issuer, "expires_at_utc": expires_at, "days_left": days_left})
        })
        .collect();
    let pick = |k: &str| monitor[k].clone();
    Ok(json!({
        "monitor": {
            "id": pick("id"), "name": pick("name"), "monitor_type": pick("monitor_type"), "target": pick("target"),
            "status": pick("status"), "active": pick("active"), "interval_seconds": pick("interval_seconds"),
            "timeout_seconds": pick("timeout_seconds"), "retries": pick("retries"), "upside_down": pick("upside_down"),
            "tags": pick("tags")
        },
        "ongoing_outage": ongoing,
        "last_events": event_list,
        "last_checks": checks,
        "expiry": expiries,
        "hint": "last_checks[].message holds the error text of failed checks; ongoing_outage.started_at is when it began (UTC)."
    }))
}

async fn list_incidents(ctx: &Ctx, args: &Args) -> Result<Value, ToolError> {
    let open_only = opt_bool(args, "open_only")?.unwrap_or(false);
    let pages: Vec<(i64, String)> = if args.contains_key("status_page_id") {
        let id = id_arg(args, "status_page_id")?;
        vec![(id, String::new())]
    } else {
        let list = ctx.get("/api/v1/status-pages").await?;
        list["data"]
            .as_array()
            .map(|a| a.iter().filter_map(|p| Some((p["id"].as_i64()?, p["slug"].as_str().unwrap_or("").to_string()))).take(25).collect())
            .unwrap_or_default()
    };
    let mut incidents = Vec::new();
    for (page, slug) in pages {
        let list = ctx.get(&format!("/api/v1/status-pages/{page}/incidents")).await?;
        for mut inc in list["data"].as_array().cloned().unwrap_or_default() {
            if open_only && !inc["resolved_at"].is_null() {
                continue;
            }
            inc["status_page_slug"] = json!(slug);
            incidents.push(inc);
        }
    }
    Ok(json!({"incidents": incidents}))
}

async fn update_incident(ctx: &Ctx, args: &Args) -> Result<Value, ToolError> {
    let page = id_arg(args, "status_page_id")?;
    let incident = id_arg(args, "incident_id")?;
    let patch = body_without(args, &["status_page_id", "incident_id", "update"]);
    let update = match args.get("update") {
        None | Some(Value::Null) => None,
        Some(u @ Value::Object(_)) => Some(u.clone()),
        Some(_) => return Err(ToolError::Invalid("`update` must be an object with status and body".into())),
    };
    if patch.as_object().is_some_and(Map::is_empty) && update.is_none() {
        return Err(ToolError::Invalid("give fields to change, an `update`, or both".into()));
    }
    let base = format!("/api/v1/status-pages/{page}/incidents/{incident}");
    let mut out = Map::new();
    if update.is_some() && patch.as_object().is_some_and(|p| !p.is_empty()) {
        // Fields first, then the timeline entry (a resolving entry decides the final state).
        out.insert("incident".into(), ctx.api(Method::PATCH, &base, Some(patch.clone())).await?);
    } else if !patch.as_object().is_some_and(Map::is_empty) {
        out.insert("incident".into(), ctx.api(Method::PATCH, &base, Some(patch.clone())).await?);
    }
    if let Some(u) = update {
        out.insert("update".into(), ctx.api(Method::POST, &format!("{base}/updates"), Some(u)).await?);
        out.insert("incident".into(), ctx.get(&base).await?);
    }
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

const API_REFERENCE: &str = "vigilant-vole://api-reference";
const MONITOR_PREFIX: &str = "vigilant-vole://monitor/";

pub async fn list_resources(ctx: &Ctx) -> RpcResult {
    let mut resources = vec![json!({
        "uri": API_REFERENCE,
        "name": "api-reference",
        "title": "Vigilant Vole API reference",
        "description": "The REST API reference as Markdown: every endpoint, field and example.",
        "mimeType": "text/markdown"
    })];
    if let Ok(list) = ctx.get("/api/v1/monitors?limit=100").await {
        for m in list["data"].as_array().cloned().unwrap_or_default() {
            if let Some(id) = m["id"].as_i64() {
                resources.push(json!({
                    "uri": format!("{MONITOR_PREFIX}{id}"),
                    "name": format!("monitor-{id}"),
                    "title": m["name"],
                    "description": format!("Monitor {id}: settings and current status."),
                    "mimeType": "application/json"
                }));
            }
        }
    }
    Ok(json!({"resources": resources}))
}

pub fn resource_templates() -> Value {
    json!({"resourceTemplates": [{
        "uriTemplate": format!("{MONITOR_PREFIX}{{id}}"),
        "name": "monitor",
        "title": "Monitor",
        "description": "One monitor's settings and current status (secrets masked).",
        "mimeType": "application/json"
    }]})
}

pub async fn read_resource(ctx: &Ctx, uri: &str) -> RpcResult {
    if uri == API_REFERENCE {
        let settings = ctx.state.settings().await.map_err(|_| (-32603, "Could not read the settings.".to_string()))?;
        let mut headers = HeaderMap::new();
        for (k, v) in &ctx.forwarded {
            headers.insert(k.clone(), v.clone());
        }
        let base = crate::api::agents::public_base_url(&settings, &headers);
        let mut text = crate::api::v1::reference_markdown(&base);
        // The reference describes the REST API; point out the MCP endpoint too.
        text.push_str(&format!("\n\n## MCP\n\nAI assistants can also use `POST {base}/mcp` (Model Context Protocol over Streamable HTTP, same API tokens). See docs/mcp.md.\n"));
        return Ok(json!({"contents": [{"uri": uri, "mimeType": "text/markdown", "text": text}]}));
    }
    if let Some(id) = uri.strip_prefix(MONITOR_PREFIX).and_then(|s| s.parse::<i64>().ok()).filter(|n| *n >= 1) {
        return match ctx.get(&format!("/api/v1/monitors/{id}")).await {
            Ok(mut monitor) => {
                mask(&mut monitor);
                let text = serde_json::to_string_pretty(&monitor).unwrap_or_default();
                Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": text}]}))
            }
            Err(ToolError::Api(404, _)) => Err((-32002, format!("Resource not found: {uri}"))),
            Err(e) => Err((-32603, e.text())),
        };
    }
    Err((-32002, format!("Resource not found: {uri}")))
}

#[allow(dead_code)]
fn _assert_header_value(_: HeaderValue) {}
