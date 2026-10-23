//! The API reference: one route table ([`ROUTES`]) drives the `/api-docs`
//! page, `/api/v1/openapi.json` (OpenAPI 3.1) and the Markdown version for AI
//! assistants (`/api-docs.md`, `/llms.txt`, see [`markdown`]). The kit serves
//! all four ([`crate::kit::docs::page`]) from [`DOCS`].

mod markdown;

use crate::kit::docs::{
    BearerScheme, ConfigDocument, Credential, DocsSpec, ErrorCode, RouteDoc, r,
};

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
    r("GET", "/api/v1/status-pages/{id}/subscription", "viewer", "Status pages", "Whether visitors can subscribe to the page's announcements, which email channel sends the mails and whether webhook subscribers are allowed."),
    r("PUT", "/api/v1/status-pages/{id}/subscription", "editor", "Status pages", "Change the subscription settings (enabled, channel_id of an email notification channel or null, webhooks). Omitted fields stay as they are.")
        .req(r#"{"enabled": true, "channel_id": 3, "webhooks": false}"#),
    r("GET", "/api/v1/status-pages/{id}/subscribers", "editor", "Status pages", "List the page's subscribers (email addresses or webhook URLs) with their confirmation state. A webhook with confirmed_at null waits for an editor's approval."),
    r("POST", "/api/v1/status-pages/{id}/subscribers/{sid}/approve", "editor", "Status pages", "Approve a webhook sign-up. Webhooks are never confirmed by a link; once approved, the webhook gets a \"subscribed\" message with its unsubscribe link."),
    r("DELETE", "/api/v1/status-pages/{id}/subscribers/{sid}", "editor", "Status pages", "Remove a subscriber."),
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

/// The reference as Markdown (also served as an MCP resource).
pub fn reference_markdown(base_url: &str) -> String {
    crate::kit::docs::markdown::document(base_url)
}

fn curl_path_param(route: &RouteDoc, segment: &str) -> Option<String> {
    Some(
        match segment {
            "{id}"
                if route.path.starts_with("/api/v1/monitors")
                    || route.path.starts_with("/api/badge") =>
            {
                "7"
            }
            "{id}" | "{iid}" => "1",
            "{token}" => "$VV_PUSH_TOKEN",
            "{slug}" => "acme",
            "{kind}" => "status",
            _ => return None,
        }
        .to_string(),
    )
}

/// Remote agents' tokens and push monitors' URL tokens.
const CREDENTIALS: &[Credential] = &[
    Credential {
        access: "push token",
        words: "the push monitor's token in the URL (no API token)",
        bearer: None,
        header: None,
    },
    Credential {
        access: "agent token",
        words: "remote agent token (`Authorization: Bearer vva_…`)",
        bearer: Some(BearerScheme {
            name: "agentToken",
            description: "Remote agent token (vva_…)",
            variable: "AGENT_TOKEN",
        }),
        header: None,
    },
];

/// Vigilant Vole's error codes besides the kit's generic ones.
const ERROR_CODES: &[ErrorCode] = &[
    ErrorCode {
        code: "conflict",
        status: 409,
        when: "clashes with existing data or the object's state (duplicate tag name or slug, checking a paused monitor)",
        on_page: true,
    },
    ErrorCode {
        code: "too_large",
        status: 413,
        when: "configuration document over 2 MB",
        on_page: false,
    },
    ErrorCode {
        code: "invalid_document",
        status: 422,
        when: "configuration document that can't be parsed",
        on_page: false,
    },
    ErrorCode {
        code: "delivery_failed",
        status: 502,
        when: "a test notification the provider refused",
        on_page: true,
    },
];

/// What Vigilant Vole contributes to the kit's API reference.
pub static DOCS: DocsSpec = DocsSpec {
    routes: ROUTES,
    openapi_title: "Vigilant Vole API",
    openapi_description: "REST API for Vigilant Vole. Authenticate with `Authorization: Bearer vv_…` (create tokens under Settings → API tokens). Read-only tokens act as viewers. Errors use `{\"error\": {\"code\", \"message\", \"fields\"}}`; lists use `?limit=&cursor=` and return `next_cursor`.",
    bearer_description: "API token (vv_…)",
    credentials: CREDENTIALS,
    error_example: ERROR_EXAMPLE,
    error_codes: ERROR_CODES,
    config_document: Some(ConfigDocument {
        path: "/api/v1/config",
        file: "vigilant.yml",
        body: "a configuration document in YAML or JSON (see the example and \"Config as code\" below)",
    }),
    curl_path_param,
    markdown_intro: markdown::intro,
    markdown_middle: markdown::middle,
    markdown_outro: markdown::outro,
    page_subtitle: "docs.sub",
    openapi_path: "/api/v1/openapi.json",
    api_base: "/api/v1",
    example_path: "/api/v1/monitors",
    metrics_path: Some("/metrics"),
    fallback_host: Some("localhost:3001"),
    param_types: &[],
    // `POST /mcp` says "api token", which the published reference shows as
    // "public (no token)"; strict checking would refuse it.
    strict_access: false,
};
