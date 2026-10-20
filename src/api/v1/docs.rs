//! The API reference: one route table ([`ROUTES`]) drives the `/api-docs`
//! page, `/api/v1/openapi.json` (OpenAPI 3.1) and the Markdown version for AI
//! assistants (`/api-docs.md`, `/llms.txt`, see [`markdown`]).

mod markdown;

use crate::i18n::Tr as _;
use askama::Template;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_extra::extract::cookie::SignedCookieJar;
use serde_json::{Map, Value, json};

use super::{cached_ok, json_ok};
use crate::api::{ApiResult, AppState, HtmlTemplate, PageCtx};
use crate::auth::CurrentUser;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api-docs", get(page))
        .route("/api-docs.md", get(markdown_doc))
        .route("/llms.txt", get(llms_txt))
        .route("/api/v1/openapi.json", get(openapi))
}

/// One documented endpoint.
#[derive(Debug, Clone, Copy)]
pub struct RouteDoc {
    pub method: &'static str,
    pub path: &'static str,
    /// Minimum access: `viewer`, `editor`, `admin`, or a machine credential
    /// (`public`, `push token`, `agent token`, `metrics token`).
    pub access: &'static str,
    pub group: &'static str,
    pub summary: &'static str,
    /// Query parameters, `name: description` per entry.
    pub query: &'static [&'static str],
    pub request: &'static str,
    pub response: &'static str,
}

const fn r(method: &'static str, path: &'static str, access: &'static str, group: &'static str, summary: &'static str) -> RouteDoc {
    RouteDoc { method, path, access, group, summary, query: &[], request: "", response: "" }
}

impl RouteDoc {
    const fn q(mut self, query: &'static [&'static str]) -> Self {
        self.query = query;
        self
    }
    const fn req(mut self, request: &'static str) -> Self {
        self.request = request;
        self
    }
    const fn res(mut self, response: &'static str) -> Self {
        self.response = response;
        self
    }

    /// Lowercase method, for CSS-free badges and OpenAPI keys.
    pub fn method_lower(&self) -> String {
        self.method.to_ascii_lowercase()
    }

    /// Anchor id on the docs page.
    pub fn anchor(&self) -> String {
        let mut s = format!("{}-{}", self.method_lower(), self.path);
        s.retain(|c| c.is_ascii_alphanumeric() || c == '-' || c == '/');
        s.replace('/', "-").trim_matches('-').to_string()
    }

    pub fn is_public(&self) -> bool {
        !matches!(self.access, "viewer" | "editor" | "admin")
    }

    /// `/api/v1/config` takes and returns YAML (JSON works too).
    pub fn is_yaml(&self) -> bool {
        self.path == "/api/v1/config"
    }

    /// The status code of a successful call.
    /// POST creates (201) only on a collection, i.e. a path that also lists;
    /// actions like `/pause`, `/test` or `/bulk` answer 200.
    pub fn success_status(&self) -> u16 {
        match self.method {
            "DELETE" => 204,
            "POST" if self.path.starts_with("/api/v1/") && !self.is_yaml() && ROUTES.iter().any(|r| r.method == "GET" && r.path == self.path) => 201,
            _ => 200,
        }
    }

    /// Status class for the access badge.
    pub fn access_class(&self) -> &'static str {
        match self.access {
            "admin" => "status-down",
            "editor" => "status-pending",
            "viewer" => "status-up",
            _ => "status-info",
        }
    }
}

const LIMIT_Q: &str = "limit: page size (1–500, default 50)";
const CURSOR_Q: &str = "cursor: next_cursor from the previous page";

const MONITOR_EXAMPLE: &str = r#"{
  "id": 7,
  "name": "Website",
  "description": "",
  "monitor_type": "http",
  "target": "https://example.com",
  "parent_id": null,
  "interval_seconds": 60,
  "retry_interval_seconds": null,
  "timeout_seconds": 10,
  "retries": 1,
  "active": true,
  "upside_down": false,
  "http_method": "GET",
  "accepted_status_codes": "200-399",
  "max_redirects": 10,
  "ignore_tls": false,
  "expiry_notify": true,
  "expiry_days": 14,
  "status": "up",
  "tags": [{"id": 2, "name": "production", "color": "green", "value": ""}],
  "notifications": [{"channel_id": 1, "escalation": false}],
  "created_at": "2026-10-19 08:00:00",
  "updated_at": "2026-10-19 08:00:00"
}"#;

const MONITOR_CREATE: &str = r#"{
  "name": "Website",
  "monitor_type": "http",
  "target": "https://example.com",
  "interval_seconds": 60,
  "retries": 1,
  "tags": ["production", {"name": "env", "value": "prod"}],
  "notifications": [1, {"channel_id": 3, "escalation": true}],
  "escalation_minutes": 15
}"#;

const ERROR_EXAMPLE: &str = r#"{
  "error": {
    "code": "validation_failed",
    "message": "Enter a full URL, including https://, like https://example.com.",
    "fields": {"target": "Enter a full URL, including https://, like https://example.com."}
  }
}"#;

const PLAN_EXAMPLE: &str = r#"{
  "applied": false,
  "mode": "merge",
  "summary": {"create": 2, "update": 1, "delete": 0, "unchanged": 14, "skip": 0},
  "items": [
    {"kind": "monitor", "key": "Website", "action": "update", "changes": ["interval_seconds"]},
    {"kind": "tag", "key": "staging", "action": "create"}
  ],
  "errors": [],
  "warnings": []
}"#;

const CONFIG_EXAMPLE: &str = r#"version: 1
tags:
  - name: production
    color: green
notification_channels:
  - name: Ops Discord
    kind: discord
    config:
      url: "••••••"
monitors:
  - name: Website
    monitor_type: http
    target: https://example.com
    interval_seconds: 30
    tags: [production]
    notifications: [Ops Discord]
status_pages:
  - slug: acme
    title: Acme status
    sections:
      - name: Services
        monitors: [Website]"#;

/// Every documented endpoint, in display order.
pub static ROUTES: &[RouteDoc] = &[
    // -- Monitors --
    r("GET", "/api/v1/monitors", "viewer", "Monitors", "List monitors with their current status.")
        .q(&["status: up | down | pending | maintenance | paused | unknown", "type: monitor type, e.g. http", "tag: tag name", "q: search name, target and description", LIMIT_Q, CURSOR_Q])
        .res(r#"{"data": [ …monitor… ], "next_cursor": "42"}"#),
    r("POST", "/api/v1/monitors", "editor", "Monitors", "Create a monitor. Same validation as the monitor form; unknown tag names are created.")
        .req(MONITOR_CREATE)
        .res(MONITOR_EXAMPLE),
    r("GET", "/api/v1/monitors/{id}", "viewer", "Monitors", "Get one monitor. Passwords are masked; push tokens are shown to editors only.").res(MONITOR_EXAMPLE),
    r("PUT", "/api/v1/monitors/{id}", "editor", "Monitors", "Replace a monitor: fields you leave out go back to their defaults (tags and notifications are kept unless sent).")
        .req(MONITOR_CREATE)
        .res(MONITOR_EXAMPLE),
    r("PATCH", "/api/v1/monitors/{id}", "editor", "Monitors", "Change some fields; everything else stays. Send \"••••••\" for a secret to keep it.")
        .req(r#"{"interval_seconds": 30, "retries": 2}"#)
        .res(MONITOR_EXAMPLE),
    r("DELETE", "/api/v1/monitors/{id}", "editor", "Monitors", "Delete a monitor and its history. Returns 204."),
    r("POST", "/api/v1/monitors/{id}/pause", "editor", "Monitors", "Pause checks and alerts.").res(MONITOR_EXAMPLE),
    r("POST", "/api/v1/monitors/{id}/resume", "editor", "Monitors", "Resume a paused monitor.").res(MONITOR_EXAMPLE),
    r("POST", "/api/v1/monitors/{id}/check", "editor", "Monitors", "Run a check right now and return its result (409 for paused, push and group monitors).")
        .res(r#"{"status": "up", "heartbeat": {"id": 991, "status": "up", "response_time_ms": 84, "message": "200 OK", "checked_at": "2026-10-19 14:03:00"}}"#),
    r("GET", "/api/v1/monitors/{id}/heartbeats", "viewer", "Monitors", "Check history, newest first (local checks only).")
        .q(&["since: UTC time, e.g. 2026-10-19 00:00:00", LIMIT_Q, CURSOR_Q])
        .res(r#"{"data": [{"id": 991, "monitor_id": 7, "agent_id": null, "status": "up", "response_time_ms": 84, "message": "200 OK", "checked_at": "2026-10-19 14:03:00"}], "next_cursor": "940"}"#),
    r("GET", "/api/v1/monitors/{id}/uptime", "viewer", "Monitors", "Uptime over a window plus one entry per day (survives heartbeat retention).")
        .q(&["days: 1–365 (default 30)"])
        .res(r#"{"monitor_id": 7, "days": 30, "uptime_pct": 99.98, "uptime_24h_pct": 100.0, "daily": [{"day": "2026-10-19", "up": 1440, "down": 0, "pending": 0, "maintenance": 0, "avg_ms": 82.4, "uptime_pct": 100.0}]}"#),
    r("GET", "/api/v1/monitors/{id}/events", "viewer", "Monitors", "Outages, maintenance, pauses and other events, newest first.")
        .q(&[LIMIT_Q])
        .res(r#"{"data": [{"id": 12, "monitor_id": 7, "kind": "down", "message": "timeout", "started_at": "2026-10-18 03:10:00", "ended_at": "2026-10-18 03:14:00", "duration_seconds": 240}], "next_cursor": null}"#),
    r("PUT", "/api/v1/monitors/{id}/tags", "editor", "Monitors", "Replace a monitor's tags (names are created if new).")
        .req(r#"["production", {"name": "env", "value": "prod"}, {"id": 4}]"#)
        .res(MONITOR_EXAMPLE),
    r("POST", "/api/v1/monitors/bulk", "editor", "Monitors", "Change settings on many monitors at once (ids, or every monitor matching a filter like the list's). Each setting is optional; it uses the monitor form's validation and one transaction. Settings that don't apply to a monitor's type are skipped and listed: agents_unsupported, timing_unsupported, timeout_exceeds_interval, escalation_needs_delay.")
        .req(r#"{"filter": {"type": "http", "tag": "production"}, "set": {"run_on_agents": true, "confirm_with_agents": true, "interval_seconds": 120, "channels": {"add": [{"name": "Ops Discord", "escalation": false}], "remove": [3]}, "tags": {"add": [{"name": "eu", "color": "teal", "value": "west"}], "remove": ["legacy"]}}}"#)
        .res(r#"{"updated": 38, "skipped": [{"id": 12, "reason": "agents_unsupported"}]}"#),
    // -- Tags --
    r("GET", "/api/v1/tags", "viewer", "Tags", "List tags with how many monitors use them.")
        .res(r#"{"data": [{"id": 2, "name": "production", "color": "green", "monitors": 12}], "next_cursor": null}"#),
    r("POST", "/api/v1/tags", "editor", "Tags", "Create a tag. Colours are palette names (red, orange, yellow, green, mint, teal, cyan, blue, indigo, purple, pink, brown, gray); hex values map to the nearest.")
        .req(r#"{"name": "production", "color": "green"}"#)
        .res(r#"{"id": 2, "name": "production", "color": "green"}"#),
    r("GET", "/api/v1/tags/{id}", "viewer", "Tags", "Get one tag."),
    r("PATCH", "/api/v1/tags/{id}", "editor", "Tags", "Rename or recolour a tag.").req(r#"{"color": "teal"}"#),
    r("DELETE", "/api/v1/tags/{id}", "editor", "Tags", "Delete a tag (it is removed from every monitor)."),
    // -- Notifications --
    r("GET", "/api/v1/notification-providers", "viewer", "Notifications", "Every channel kind with its config fields (which are required, which are secret).")
        .res(r#"{"data": [{"kind": "discord", "label": "Discord", "fields": [{"key": "url", "type": "url", "required": true, "secret": true}]}]}"#),
    r("GET", "/api/v1/notification-channels", "viewer", "Notifications", "List channels. Secrets are always masked as \"••••••\".")
        .res(r#"{"data": [{"id": 1, "name": "Ops Discord", "kind": "discord", "active": true, "is_default": true, "monitors": 12, "config": {"url": "••••••", "username": "Vole"}}], "next_cursor": null}"#),
    r("POST", "/api/v1/notification-channels", "editor", "Notifications", "Create a channel. apply_existing links it to every monitor.")
        .req(r#"{"name": "Ops Discord", "kind": "discord", "config": {"url": "https://discord.com/api/webhooks/…"}, "is_default": true, "apply_existing": false}"#),
    r("GET", "/api/v1/notification-channels/{id}", "viewer", "Notifications", "Get one channel (secrets masked)."),
    r("PATCH", "/api/v1/notification-channels/{id}", "editor", "Notifications", "Update a channel. Secrets you leave out or send masked keep their stored value.")
        .req(r#"{"active": false}"#),
    r("DELETE", "/api/v1/notification-channels/{id}", "editor", "Notifications", "Delete a channel."),
    r("POST", "/api/v1/notification-channels/{id}/test", "editor", "Notifications", "Send a test notification (502 with the provider's error when delivery fails).")
        .res(r#"{"ok": true}"#),
    // -- Remote browsers --
    r("GET", "/api/v1/remote-browsers", "viewer", "Remote browsers", "List the remote browsers (CDP endpoints) real-browser monitors use. Tokens and passwords in URLs are masked; the first one is the default.")
        .res(r#"{"data": [{"id": 1, "name": "Chrome", "url": "ws://chrome:9222/devtools/browser/…", "monitors": 3, "default": true}], "next_cursor": null}"#),
    r("POST", "/api/v1/remote-browsers", "admin", "Remote browsers", "Add a remote browser: ws(s)://…/devtools/browser/<id>, http(s)://host:port (discovered through /json/version) or wss://host?token=….")
        .req(r#"{"name": "Chrome", "url": "http://chrome:9222"}"#),
    r("GET", "/api/v1/remote-browsers/{id}", "viewer", "Remote browsers", "Get one remote browser (secrets masked)."),
    r("PUT", "/api/v1/remote-browsers/{id}", "admin", "Remote browsers", "Change a remote browser. A masked URL keeps its token while the host stays the same."),
    r("PATCH", "/api/v1/remote-browsers/{id}", "admin", "Remote browsers", "Rename a remote browser or change its URL.").req(r#"{"name": "Chrome (LAN)"}"#),
    r("DELETE", "/api/v1/remote-browsers/{id}", "admin", "Remote browsers", "Remove a remote browser; monitors that used it fall back to the default one."),
    r("POST", "/api/v1/remote-browsers/{id}/test", "admin", "Remote browsers", "Connect and ask the browser for its version.")
        .res(r#"{"ok": true, "product": "HeadlessChrome/156.0.8078.36", "protocol_version": "1.3"}"#),
    // -- Status pages --
    r("GET", "/api/v1/status-pages", "viewer", "Status pages", "List status pages."),
    r("POST", "/api/v1/status-pages", "editor", "Status pages", "Create a status page (slug defaults to the title).")
        .req(r#"{"title": "Acme status", "slug": "acme", "sections": [{"name": "API", "monitor_ids": [7, 8]}]}"#),
    r("GET", "/api/v1/status-pages/{id}", "viewer", "Status pages", "Get a page with its sections and monitors.")
        .res(r#"{"id": 1, "slug": "acme", "title": "Acme status", "published": true, "theme": "auto", "sections": [{"id": 3, "name": "API", "monitors": [{"id": 7, "name": "Website"}]}], "public_url": "/status/acme"}"#),
    r("PATCH", "/api/v1/status-pages/{id}", "editor", "Status pages", "Change page settings (and optionally its sections). Also: password (write-only, \"\" removes), analytics, logo/favicon as data: URLs (\"\" removes).").req(r#"{"theme": "dark", "refresh_seconds": 30, "password": "correct horse", "analytics": {"provider": "plausible", "site_id": "status.example.com", "script_url": "https://plausible.io/js/script.js"}}"#),
    r("DELETE", "/api/v1/status-pages/{id}", "editor", "Status pages", "Delete a page."),
    r("PUT", "/api/v1/status-pages/{id}/sections", "editor", "Status pages", "Replace the page layout: sections in order, monitors in order.")
        .req(r#"[{"name": "API", "monitor_ids": [7, 8]}, {"name": "Website", "monitor_ids": [9]}]"#),
    r("GET", "/api/v1/status-pages/{id}/incidents", "viewer", "Status pages", "List a page's incidents."),
    r("POST", "/api/v1/status-pages/{id}/incidents", "editor", "Status pages", "Post an incident (severity: info, warning, danger, success).")
        .req(r#"{"title": "Elevated error rates", "body": "We're looking into it.", "severity": "warning", "pinned": true}"#),
    r("GET", "/api/v1/status-pages/{id}/incidents/{iid}", "viewer", "Status pages", "Get one incident."),
    r("PATCH", "/api/v1/status-pages/{id}/incidents/{iid}", "editor", "Status pages", "Update an incident; resolved: true resolves it, false reopens it.")
        .req(r#"{"body": "Fixed.", "severity": "success", "resolved": true}"#),
    r("DELETE", "/api/v1/status-pages/{id}/incidents/{iid}", "editor", "Status pages", "Delete an incident."),
    r("GET", "/api/v1/status-pages/{id}/incidents/{iid}/updates", "viewer", "Status pages", "List an incident's updates, newest first."),
    r("POST", "/api/v1/status-pages/{id}/incidents/{iid}/updates", "editor", "Status pages", "Post an update to an incident (status: investigating, identified, monitoring, resolved). A resolved update resolves the incident; any other status reopens it.")
        .req(r#"{"status": "identified", "body": "A bad deploy. Rolling back."}"#),
    r("DELETE", "/api/v1/status-pages/{id}/incidents/{iid}/updates/{uid}", "editor", "Status pages", "Delete one update."),
    // -- Maintenance --
    r("GET", "/api/v1/maintenance", "viewer", "Maintenance", "List maintenance windows with their current state."),
    r("POST", "/api/v1/maintenance", "editor", "Maintenance", "Create a window. Times: start_at/end_at in UTC, time_from/time_to local to timezone, weekdays ISO (1 = Monday). Strategies: manual, once, daily, weekly, interval (interval_days, counted from start_at), monthly (month_days like 1,15,-1) and cron (cron + duration_minutes).")
        .req(r#"{"title": "Database upgrade", "strategy": "weekly", "weekdays": "2,4", "time_from": "22:00", "time_to": "23:30", "timezone": "Europe/Oslo", "monitor_ids": [7]}"#),
    r("GET", "/api/v1/maintenance/{id}", "viewer", "Maintenance", "Get one window."),
    r("PATCH", "/api/v1/maintenance/{id}", "editor", "Maintenance", "Change a window; null clears a time field.").req(r#"{"active": false}"#),
    r("DELETE", "/api/v1/maintenance/{id}", "editor", "Maintenance", "Delete a window."),
    // -- System --
    r("GET", "/api/v1/me", "viewer", "System", "Who you are and how you signed in.")
        .res(r#"{"id": 1, "username": "ada", "role": "admin", "auth": "token", "token_scope": "read", "can_edit": false, "is_admin": false}"#),
    r("GET", "/api/v1/health", "viewer", "System", "Version, uptime and monitor counts.")
        .res(r#"{"status": "ok", "version": "0.1.0", "uptime_seconds": 86400, "monitors": {"total": 14, "by_status": {"up": 13, "down": 1}}, "status_pages": 1, "agents": 2}"#),
    r("GET", "/api/v1/agents", "admin", "System", "Remote probe agents and whether they're online."),
    r("GET", "/api/v1/settings", "admin", "System", "Instance settings.")
        .res(r#"{"retention_days": 90, "base_url": "https://status.example.com", "timezone": "Europe/Oslo", "language": "en", "site_name": "Vigilant Vole", "badges_public": false, "metrics_enabled": true}"#),
    r("PATCH", "/api/v1/settings", "admin", "System", "Change some settings.").req(r#"{"retention_days": 180, "base_url": "https://status.example.com"}"#),
    r("GET", "/api/v1/openapi.json", "viewer", "System", "This reference as an OpenAPI 3.1 document."),
    // -- Configuration --
    r("GET", "/api/v1/config", "editor", "Configuration", "Export the whole configuration as YAML or JSON. Secrets are masked unless an admin asks for them.")
        .q(&["format: yaml (default) | json", "secrets: 1 to include secrets (admins only)", "download: 1 to download as a file"])
        .res(CONFIG_EXAMPLE),
    r("POST", "/api/v1/config", "admin", "Configuration", "Import a configuration document (YAML or JSON body). Merge upserts by key; replace also deletes what the document doesn't list in the sections it contains. Masked secrets keep stored values.")
        .q(&["mode: merge (default) | replace", "dry_run: 1 to only return the plan", "confirm: 1, required to apply a replace"])
        .req(CONFIG_EXAMPLE)
        .res(PLAN_EXAMPLE),
    // -- Public & machine endpoints --
    r("GET", "/api/push/{token}", "push token", "Push and machine endpoints", "Report a push monitor's status (POST works too). Rate limited to 1 per second.")
        .q(&["status: up (default) | down", "msg: message to record", "ping: response time in ms"])
        .res(r#"{"ok": true}"#),
    r("GET", "/api/badge/{id}/{kind}", "public", "Push and machine endpoints", "SVG badge: kind is status, uptime, ping or cert-exp. Public when the monitor is on a published status page or badges are public.")
        .q(&["duration: 24h | 7d | 30d", "style: flat | flat-square", "label: custom label"]),
    r("GET", "/api/status-page/{slug}", "public", "Push and machine endpoints", "A published status page as JSON (CORS enabled), for embedding.")
        .res(r#"{"title": "Acme status", "overall": {"indicator": "operational", "title": "All systems operational"}, "sections": [ … ], "incidents": [ … ]}"#),
    r("GET", "/api/agent/v1/monitors", "agent token", "Push and machine endpoints", "For remote agents: the monitors to run (Bearer vva_… token, ETag)."),
    r("POST", "/api/agent/v1/results", "agent token", "Push and machine endpoints", "For remote agents: a batch of up to 500 results.")
        .req(r#"{"results": [{"monitor_id": 7, "status": "up", "response_time_ms": 120, "message": "200 OK", "checked_at": "2026-10-19 14:03:00"}]}"#),
    r("GET", "/metrics", "metrics token", "Push and machine endpoints", "Prometheus metrics (when enabled). Bearer token or basic-auth password; Kuma-compatible series included."),
    r("POST", "/mcp", "api token", "Push and machine endpoints", "Model Context Protocol server (Streamable HTTP, JSON-RPC 2.0) for AI assistants: tools to read and, with a write token, steer your monitors, incidents and maintenance, plus this reference as a resource. Bearer API token; GET answers 405. See docs/mcp.md.")
        .req(r#"{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "why_is_it_down", "arguments": {"id": 7}}}"#),
    r("GET", "/healthz", "public", "Push and machine endpoints", "Liveness probe: always \"ok\"."),
];

/// Groups in display order.
fn groups() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for r in ROUTES {
        if !out.contains(&r.group) {
            out.push(r.group);
        }
    }
    out
}

/// A group of routes, for the template.
pub struct GroupView {
    pub name: &'static str,
    pub anchor: String,
    pub title_id: String,
    pub routes: Vec<RouteDoc>,
}

#[derive(Template)]
#[template(path = "api_docs.html")]
struct DocsTemplate {
    ctx: PageCtx,
    groups: Vec<GroupView>,
    base_url: String,
    error_example: &'static str,
}

/// This instance's address: the configured base URL, else the request's host.
fn instance_url(base_url: Option<String>, headers: &HeaderMap) -> String {
    base_url.unwrap_or_else(|| {
        let host = headers.get(axum::http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost:3001");
        format!("http://{host}")
    })
}

async fn page(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar, headers: HeaderMap) -> ApiResult<Response> {
    let settings = state.settings().await?;
    let base_url = instance_url(settings.base_url.clone(), &headers);
    let (ctx, jar) = PageCtx::build(&state, Some(&user), &jar, crate::i18n::t("docs.title"), "", "").await;
    let groups = groups()
        .into_iter()
        .map(|g| GroupView {
            name: g,
            anchor: g.to_ascii_lowercase().replace(' ', "-"),
            title_id: format!("{}-title", g.to_ascii_lowercase().replace(' ', "-")),
            routes: ROUTES.iter().filter(|r| r.group == g).copied().collect(),
        })
        .collect();
    Ok((jar, HtmlTemplate(DocsTemplate { ctx, groups, base_url, error_example: ERROR_EXAMPLE })).into_response())
}

/// `{id}` style path parameters.
fn path_params(path: &str) -> Vec<&str> {
    path.split('/').filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}'))).collect()
}

/// JSON Schema type of a path parameter.
fn path_param_type(name: &str) -> &'static str {
    if matches!(name, "kind" | "token" | "slug") { "string" } else { "integer" }
}

fn example(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

/// The OpenAPI 3.1 document, assembled from [`ROUTES`].
pub fn openapi_document(server_url: &str) -> Value {
    let mut paths = Map::new();
    for r in ROUTES {
        let mut op = Map::new();
        op.insert("summary".into(), json!(r.summary));
        op.insert("tags".into(), json!([r.group]));
        op.insert("operationId".into(), json!(r.anchor()));
        let mut params: Vec<Value> = path_params(r.path)
            .into_iter()
            .map(|p| json!({"name": p, "in": "path", "required": true, "schema": {"type": path_param_type(p)}}))
            .collect();
        for q in r.query {
            let (name, desc) = q.split_once(':').unwrap_or((q, ""));
            params.push(json!({"name": name.trim(), "in": "query", "required": false, "description": desc.trim(), "schema": {"type": "string"}}));
        }
        if !params.is_empty() {
            op.insert("parameters".into(), Value::Array(params));
        }
        if !r.request.is_empty() {
            let content = if r.is_yaml() {
                json!({
                    "application/yaml": {"schema": {"type": "string"}, "example": r.request},
                    "application/json": {"schema": {"type": "object"}},
                })
            } else {
                json!({"application/json": {"schema": {}, "example": example(r.request)}})
            };
            op.insert("requestBody".into(), json!({"required": true, "content": content}));
        }
        let ok_code = r.success_status().to_string();
        let mut ok = json!({"description": "Success"});
        if !r.response.is_empty() {
            let ct = if r.is_yaml() && r.method == "GET" { "application/yaml" } else { "application/json" };
            ok["content"] = json!({ ct: {"example": example(r.response)} });
        }
        let mut responses = Map::new();
        responses.insert(ok_code, ok);
        if !r.is_public() {
            responses.insert("401".into(), json!({"$ref": "#/components/responses/Error"}));
            responses.insert("403".into(), json!({"$ref": "#/components/responses/Error"}));
        }
        if r.path.contains('{') {
            responses.insert("404".into(), json!({"$ref": "#/components/responses/Error"}));
        }
        if matches!(r.method, "POST" | "PUT" | "PATCH") {
            responses.insert("422".into(), json!({"$ref": "#/components/responses/Error"}));
        }
        op.insert("responses".into(), Value::Object(responses));
        let security = match r.access {
            "public" | "push token" => json!([]),
            "agent token" => json!([{"agentToken": []}]),
            _ => json!([{"bearerAuth": []}, {"sessionCookie": []}]),
        };
        op.insert("security".into(), security);
        op.insert("x-access".into(), json!(r.access));
        let entry = paths.entry(r.path.to_string()).or_insert_with(|| json!({}));
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(r.method_lower(), Value::Object(op));
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Vigilant Vole API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "REST API for Vigilant Vole. Authenticate with `Authorization: Bearer vv_…` (create tokens under Settings → API tokens). Read-only tokens act as viewers. Errors use `{\"error\": {\"code\", \"message\", \"fields\"}}`; lists use `?limit=&cursor=` and return `next_cursor`.",
        },
        "servers": [{"url": server_url}],
        "tags": groups().iter().map(|g| json!({"name": g})).collect::<Vec<_>>(),
        "paths": paths,
        "components": {
            "securitySchemes": {
                "bearerAuth": {"type": "http", "scheme": "bearer", "description": "API token (vv_…)"},
                "agentToken": {"type": "http", "scheme": "bearer", "description": "Remote agent token (vva_…)"},
                "sessionCookie": {"type": "apiKey", "in": "cookie", "name": crate::auth::SESSION_COOKIE},
            },
            "responses": {
                "Error": {
                    "description": "Error",
                    "content": {"application/json": {"example": example(ERROR_EXAMPLE)}},
                },
            },
        },
    })
}

async fn openapi(State(state): State<AppState>, _user: CurrentUser, headers: HeaderMap) -> ApiResult<Response> {
    let server = state.settings().await?.base_url.unwrap_or_else(|| "/".into());
    Ok(json_ok(&headers, &openapi_document(&server)))
}

/// The reference as Markdown (also served as an MCP resource).
pub fn reference_markdown(base_url: &str) -> String {
    markdown::document(base_url)
}

/// The reference as Markdown, for AI assistants.
async fn markdown_doc(State(state): State<AppState>, _user: CurrentUser, headers: HeaderMap) -> ApiResult<Response> {
    let base_url = instance_url(state.settings().await?.base_url, &headers);
    Ok(cached_ok(&headers, markdown::document(&base_url).into_bytes(), "text/markdown; charset=utf-8"))
}

/// The same document at the conventional `llms.txt` address.
async fn llms_txt(State(state): State<AppState>, _user: CurrentUser, headers: HeaderMap) -> ApiResult<Response> {
    let base_url = instance_url(state.settings().await?.base_url, &headers);
    Ok(cached_ok(&headers, markdown::document(&base_url).into_bytes(), "text/plain; charset=utf-8"))
}
