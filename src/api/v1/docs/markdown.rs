//! The API reference as one Markdown document, for AI assistants
//! (`/api-docs.md`, `/llms.txt`, the "Copy for AI" button on `/api-docs`).
//!
//! Built from [`ROUTES`] like the page and the OpenAPI document, so the three
//! can't drift apart. Pure and deterministic: the only input is the instance's
//! base URL. English on purpose (field names, values and codes are English);
//! it never contains a real token or secret.

use std::fmt::Write as _;

use axum::http::StatusCode;
use serde_json::Value;

use super::{ERROR_EXAMPLE, ROUTES, RouteDoc, groups, path_param_type, path_params};
use crate::db::models::MonitorType;

/// Error codes of the v1 envelope: code, HTTP status, when.
const ERROR_CODES: &[(&str, u16, &str)] = &[
    ("bad_request", 400, "malformed query, cursor or body"),
    ("unauthorized", 401, "missing, invalid, expired or revoked token"),
    ("forbidden", 403, "your role or the token's scope doesn't allow it"),
    ("not_found", 404, "no such object or route"),
    ("conflict", 409, "clashes with existing data or the object's state (duplicate tag name or slug, checking a paused monitor)"),
    ("too_large", 413, "configuration document over 2 MB"),
    ("unsupported_media_type", 415, "body without `Content-Type: application/json`"),
    ("validation_failed", 422, "invalid fields; `fields` maps each field to its message"),
    ("invalid_document", 422, "configuration document that can't be parsed"),
    ("delivery_failed", 502, "a test notification the provider refused"),
    ("internal", 500, "server error; try again"),
];

/// The whole reference for the instance at `base_url`.
pub fn document(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let mut out = String::with_capacity(64 * 1024);
    header(&mut out, base);
    authentication(&mut out);
    conventions(&mut out);
    errors(&mut out);
    monitor_types(&mut out);
    endpoints(&mut out);
    config_as_code(&mut out);
    common_tasks(&mut out);
    out
}

fn header(out: &mut String, base: &str) {
    let version = env!("CARGO_PKG_VERSION");
    let _ = write!(
        out,
        "# Vigilant Vole API\n\n\
         > REST API of a Vigilant Vole uptime monitor (version {version}), generated from the server's own route table. \
         Everything the web app can do (monitors, tags, notification channels, status pages, incidents, maintenance, \
         settings, configuration as code) can be done with these JSON endpoints.\n\n\
         - Instance: {base}\n\
         - API base URL: {base}/api/v1\n\
         - Browsable reference: {base}/api-docs (sign in) · OpenAPI 3.1: {base}/api/v1/openapi.json\n\
         - The web interface is translated; this document, field names, values and error codes are English.\n\n\
         Examples use two shell variables; never paste a real token into a chat or commit it:\n\n\
         ```bash\n\
         export VV_URL=\"{base}\"\n\
         export VV_TOKEN=\"<API token from Settings → API tokens>\"\n\
         curl -H \"Authorization: Bearer $VV_TOKEN\" \"$VV_URL/api/v1/me\"\n\
         ```\n\n"
    );
}

fn authentication(out: &mut String) {
    out.push_str(
        "## Authentication\n\n\
         - Create a token in the web app under **Settings → API tokens** (`/settings/tokens`). Tokens start with `vv_`, \
         are shown once and can expire (30, 90, 365 days or never).\n\
         - Send it on every request: `Authorization: Bearer $VV_TOKEN`.\n\
         - Scope `read` acts as a **viewer** (every GET); scope `write` acts as the token's owner with their role.\n\
         - Roles: **viewer** reads everything; **editor** also changes monitors, tags, notification channels, status pages, \
         incidents and maintenance and exports configuration; **admin** also changes settings, users, remote browsers, \
         agents and imports configuration. Each endpoint below names the least role it needs.\n\
         - API tokens work only under `/api/` and for `/metrics`; they can't open the web interface.\n\
         - `/metrics` (Prometheus) takes the token as a Bearer token or as the HTTP Basic password (any user name).\n\
         - Machine credentials are separate: push monitors carry their own token in the URL, remote agents send an \
         agent token (`vva_…`).\n\n",
    );
}

fn conventions(out: &mut String) {
    out.push_str(
        "## Conventions\n\n\
         - JSON in and out; send bodies with `Content-Type: application/json`. `/api/v1/config` also speaks YAML.\n\
         - Lists return `{\"data\": [...], \"next_cursor\": \"42\" | null}`. Pass `?limit=` (1–500, default 50) and \
         `?cursor=<next_cursor>` for the next page; stop when `next_cursor` is `null`.\n\
         - GETs send a weak `ETag` and answer `If-None-Match` with `304 Not Modified`.\n\
         - Times are UTC strings `YYYY-MM-DD HH:MM:SS`.\n\
         - Secrets (passwords, webhook URLs, keys) are returned masked as `\"••••••\"`. Send `\"••••••\"` back (or leave \
         the field out of a PATCH) to keep the stored value.\n\
         - `POST` that creates returns `201 Created` with a `Location` header; `DELETE` returns `204 No Content`.\n\
         - `PUT` replaces an object (fields left out go back to defaults); `PATCH` changes only the fields you send.\n\
         - Cookie-authenticated writes must come from the app's own pages; token requests are exempt.\n\n",
    );
}

fn errors(out: &mut String) {
    let _ = write!(out, "## Errors\n\nEvery error uses one envelope (`fields` only on validation errors):\n\n```json\n{ERROR_EXAMPLE}\n```\n\n");
    out.push_str("| Code | HTTP | When |\n|---|---|---|\n");
    for (code, status, when) in ERROR_CODES {
        let _ = writeln!(out, "| `{code}` | {status} | {when} |");
    }
    out.push('\n');
}

fn monitor_types(out: &mut String) {
    let types: Vec<String> = MonitorType::all().iter().map(|t| format!("`{}`", t.as_str())).collect();
    let _ = write!(out, "## Monitor types\n\n`monitor_type` is one of: {}.\n\n", types.join(", "));
}

fn endpoints(out: &mut String) {
    for group in groups() {
        let _ = write!(out, "## Endpoints: {group}\n\n");
        for r in ROUTES.iter().filter(|r| r.group == group) {
            endpoint(out, r);
        }
    }
}

fn endpoint(out: &mut String, r: &RouteDoc) {
    let _ = write!(out, "### {} {}\n\n{}\n\n- Access: {}\n", r.method, r.path, r.summary, access(r));
    let status = r.success_status();
    let reason = StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or_default();
    let _ = writeln!(out, "- Success: `{status} {reason}`");
    let params = path_params(r.path);
    if !params.is_empty() {
        let list: Vec<String> = params.iter().map(|p| format!("`{p}` ({})", path_param_type(p))).collect();
        let _ = writeln!(out, "- Path parameters: {}", list.join(", "));
    }
    if !r.query.is_empty() {
        out.push_str("- Query parameters:\n");
        for q in r.query {
            let (name, desc) = q.split_once(':').unwrap_or((q, ""));
            let _ = writeln!(out, "  - `{}`: {}", name.trim(), desc.trim());
        }
    }
    if let Some(body) = body_fields(r) {
        let _ = writeln!(out, "- Body: {body}");
    }
    let _ = write!(out, "\nExample request:\n\n```bash\n{}\n```\n\n", curl(r));
    if !r.response.is_empty() {
        let lang = if r.is_yaml() && r.method == "GET" { "yaml" } else { "json" };
        let _ = write!(out, "Example response:\n\n```{lang}\n{}\n```\n\n", r.response);
    }
}

/// Who may call a route, in words.
fn access(r: &RouteDoc) -> &'static str {
    match r.access {
        "viewer" => "viewer (any API token, read scope is enough)",
        "editor" => "editor (write token of an editor or admin)",
        "admin" => "admin (write token of an admin)",
        "push token" => "the push monitor's token in the URL (no API token)",
        "agent token" => "remote agent token (`Authorization: Bearer vva_…`)",
        "metrics token" => "API token as Bearer token or HTTP Basic password",
        _ => "public (no token)",
    }
}

/// The request body's top-level fields and their JSON types, from the example.
fn body_fields(r: &RouteDoc) -> Option<String> {
    if r.request.is_empty() {
        return None;
    }
    if r.is_yaml() {
        return Some("a configuration document in YAML or JSON (see the example and \"Config as code\" below)".into());
    }
    match serde_json::from_str::<Value>(r.request) {
        Ok(Value::Object(map)) => {
            let fields: Vec<String> = map.iter().map(|(k, v)| format!("`{k}` ({})", json_type(v))).collect();
            Some(format!("JSON object; fields used in the example: {}", fields.join(", ")))
        }
        Ok(Value::Array(_)) => Some("JSON array (see the example)".into()),
        _ => Some("see the example".into()),
    }
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `curl` for a route, with `$VV_URL`/`$VV_TOKEN` placeholders.
fn curl(r: &RouteDoc) -> String {
    let path: Vec<String> = r
        .path
        .split('/')
        .map(|seg| match seg {
            "{id}" if r.path.starts_with("/api/v1/monitors") || r.path.starts_with("/api/badge") => "7".to_string(),
            "{id}" | "{iid}" => "1".to_string(),
            "{token}" => "$VV_PUSH_TOKEN".to_string(),
            "{slug}" => "acme".to_string(),
            "{kind}" => "status".to_string(),
            other => other.to_string(),
        })
        .collect();
    let mut url = format!("$VV_URL{}", path.join("/"));
    let mut cmd = String::from("curl");
    if r.method != "GET" {
        let _ = write!(cmd, " -X {}", r.method);
    }
    match r.access {
        "viewer" | "editor" | "admin" | "metrics token" => cmd.push_str(" -H \"Authorization: Bearer $VV_TOKEN\""),
        "agent token" => cmd.push_str(" -H \"Authorization: Bearer $VV_AGENT_TOKEN\""),
        _ => {}
    }
    if r.is_yaml() && r.method == "POST" {
        url.push_str("?dry_run=1");
        let _ = write!(cmd, " -H \"Content-Type: application/yaml\" --data-binary @vigilant.yml \"{url}\"");
    } else if r.is_yaml() {
        let _ = write!(cmd, " \"{url}\" -o vigilant.yml");
    } else if r.request.is_empty() {
        let _ = write!(cmd, " \"{url}\"");
    } else {
        let _ = write!(cmd, " -H \"Content-Type: application/json\" \"{url}\" -d '{}'", shell_quote(r.request));
    }
    cmd
}

/// Text for inside single quotes in a POSIX shell.
fn shell_quote(text: &str) -> String {
    text.replace('\'', "'\\''")
}

/// The example request body of a documented route.
fn example_body(method: &str, path: &str) -> &'static str {
    ROUTES.iter().find(|r| r.method == method && r.path == path).map_or("{}", |r| r.request)
}

fn config_as_code(out: &mut String) {
    out.push_str(
        "## Config as code\n\n\
         The whole setup (settings, tags, notification channels, monitors, status pages, maintenance) is one YAML or \
         JSON document. Export needs an editor (secrets only for admins with `secrets=1`); import needs an admin. An \
         import is one transaction: on any error you get `422` with every problem and nothing is written. Objects are \
         matched by name (status pages by slug); masked secrets keep their stored values.\n\n\
         ```bash\n\
         # Export (YAML; add format=json for JSON)\n\
         curl -H \"Authorization: Bearer $VV_TOKEN\" \"$VV_URL/api/v1/config\" -o vigilant.yml\n\n\
         # Preview what an import would change (nothing is written)\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/yaml\" \\\n  \
         --data-binary @vigilant.yml \"$VV_URL/api/v1/config?dry_run=1\"\n\n\
         # Apply (merge: create and update, never delete)\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/yaml\" \\\n  \
         --data-binary @vigilant.yml \"$VV_URL/api/v1/config\"\n\n\
         # Replace: also deletes what the document doesn't list (in the sections it contains); needs confirm=1\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/yaml\" \\\n  \
         --data-binary @vigilant.yml \"$VV_URL/api/v1/config?mode=replace&confirm=1\"\n\
         ```\n\n",
    );
}

fn common_tasks(out: &mut String) {
    let monitor = shell_quote(example_body("POST", "/api/v1/monitors"));
    let bulk = shell_quote(example_body("POST", "/api/v1/monitors/bulk"));
    let page = shell_quote(example_body("POST", "/api/v1/status-pages"));
    let _ = write!(
        out,
        "## Common tasks\n\n\
         ### Find monitors that are down\n\n\
         ```bash\n\
         curl -H \"Authorization: Bearer $VV_TOKEN\" \"$VV_URL/api/v1/monitors?status=down\"\n\
         ```\n\n\
         ### Create an HTTP monitor\n\n\
         Tags are names (created if new) or `{{\"name\", \"value\"}}`; notifications are channel ids or \
         `{{\"channel_id\", \"escalation\"}}` (list them with `GET /api/v1/notification-channels`).\n\n\
         ```bash\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/json\" \"$VV_URL/api/v1/monitors\" -d '{monitor}'\n\
         ```\n\n\
         ### Pause and resume a monitor\n\n\
         ```bash\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" \"$VV_URL/api/v1/monitors/7/pause\"\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" \"$VV_URL/api/v1/monitors/7/resume\"\n\
         ```\n\n\
         ### Bulk edit: run on remote agents, change the interval\n\n\
         Pick monitors with `ids` or `filter` (the list's filters: `status`, `type`, `tag`, `q`; `{{}}` is every monitor). \
         Every key of `set` is optional (left out or `null` keeps each monitor's value): `run_on_agents`, \
         `confirm_with_agents` (booleans); `interval_seconds`, `retry_interval_seconds`, `timeout_seconds`, `retries` \
         (integers); `channels` (`{{\"add\": [...], \"remove\": [...]}}` or `{{\"replace\": [...]}}`, entries are ids, names or \
         `{{\"id\"|\"name\", \"escalation\"}}`); `tags` (`{{\"add\": [...], \"remove\": [...], \"clear\": true}}`). Monitors a \
         setting doesn't apply to come back in `skipped` with a reason.\n\n\
         ```bash\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/json\" \"$VV_URL/api/v1/monitors/bulk\" -d '{bulk}'\n\
         ```\n\n\
         ### Create a status page\n\n\
         Then publish it and change its look with `PATCH /api/v1/status-pages/{{id}}`; it is served at `/status/<slug>`.\n\n\
         ```bash\n\
         curl -X POST -H \"Authorization: Bearer $VV_TOKEN\" -H \"Content-Type: application/json\" \"$VV_URL/api/v1/status-pages\" -d '{page}'\n\
         ```\n"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://status.example.com";

    #[test]
    fn lists_every_documented_route() {
        let doc = document(BASE);
        for r in ROUTES {
            assert!(doc.contains(&format!("### {} {}\n", r.method, r.path)), "missing {} {}", r.method, r.path);
        }
        assert!(doc.contains("### POST /api/v1/monitors/bulk\n"));
        assert_eq!(doc.matches("\n### ").count(), ROUTES.len() + 5, "one heading per route plus the common tasks");
    }

    #[test]
    fn explains_authentication_conventions_and_errors() {
        let doc = document(BASE);
        for needle in [
            "## Authentication",
            "Settings → API tokens",
            "Authorization: Bearer $VV_TOKEN",
            "HTTP Basic password",
            "## Conventions",
            "next_cursor",
            "If-None-Match",
            "YYYY-MM-DD HH:MM:SS",
            "\"••••••\"",
            "## Errors",
            "\"error\": {",
            "`validation_failed` | 422",
            "## Config as code",
            "dry_run=1",
            "## Common tasks",
            "## Endpoints: Monitors",
            "`real_browser`",
        ] {
            assert!(doc.contains(needle), "missing {needle:?}");
        }
        assert!(doc.contains(&format!("version {}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn uses_the_instance_base_url() {
        let doc = document("https://vole.example.org/");
        assert!(doc.contains("- API base URL: https://vole.example.org/api/v1\n"));
        assert!(doc.contains("export VV_URL=\"https://vole.example.org\""));
        assert!(!doc.contains("status.example.com/api-docs"));
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(document(BASE), document(BASE));
    }

    #[test]
    fn never_contains_a_token() {
        let doc = document(BASE);
        for prefix in ["vv_", "vva_"] {
            for (i, _) in doc.match_indices(prefix) {
                let rest = &doc[i + prefix.len()..];
                let run = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').count();
                assert!(run < 8, "token-like text at {i}: {}", &doc[i..(i + 20).min(doc.len())]);
            }
        }
    }

    #[test]
    fn curl_examples_quote_bodies_for_the_shell() {
        let incident = ROUTES.iter().find(|r| r.method == "POST" && r.path == "/api/v1/status-pages/{id}/incidents").expect("route");
        let cmd = curl(incident);
        assert!(cmd.starts_with("curl -X POST -H \"Authorization: Bearer $VV_TOKEN\""));
        assert!(cmd.contains("\"$VV_URL/api/v1/status-pages/1/incidents\""));
        assert!(cmd.contains("We'\\''re looking into it."));
        let push = ROUTES.iter().find(|r| r.path == "/api/push/{token}").expect("route");
        assert_eq!(curl(push), "curl \"$VV_URL/api/push/$VV_PUSH_TOKEN\"");
    }
}
