//! The API reference as one Markdown document, for AI assistants
//! (`/api-docs.md`, `/llms.txt`, the "Copy for AI" button on the docs page).
//!
//! Built from the product's route table like the page and the OpenAPI
//! document, so the three can't drift apart. Pure and deterministic: the
//! only input is the instance's base URL. English on purpose (field names,
//! values and codes are English); it never contains a real token or secret.

use std::fmt::Write as _;

use axum::http::StatusCode;
use serde_json::Value;

use super::{RouteDoc, credential, error_codes, groups, path_params, routes, spec};

/// The whole reference for the instance at `base_url`.
pub fn document(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let spec = spec();
    let mut out = String::with_capacity(64 * 1024);
    (spec.markdown_intro)(&mut out, base);
    errors(&mut out);
    (spec.markdown_middle)(&mut out);
    endpoints(&mut out);
    (spec.markdown_outro)(&mut out);
    out
}

fn errors(out: &mut String) {
    let _ = write!(
        out,
        "## Errors\n\nEvery error uses one envelope (`fields` only on validation errors):\n\n```json\n{}\n```\n\n",
        spec().error_example
    );
    out.push_str("| Code | HTTP | When |\n|---|---|---|\n");
    for code in error_codes() {
        let _ = writeln!(out, "| `{}` | {} | {} |", code.code, code.status, code.when);
    }
    out.push('\n');
}

fn endpoints(out: &mut String) {
    for group in groups() {
        let _ = write!(out, "## Endpoints: {group}\n\n");
        for r in routes().iter().filter(|r| r.group == group) {
            endpoint(out, r);
        }
    }
}

fn endpoint(out: &mut String, r: &RouteDoc) {
    let _ = write!(
        out,
        "### {} {}\n\n{}\n\n- Access: {}\n",
        r.method,
        r.path,
        r.summary,
        access(r)
    );
    let status = r.success_status();
    let reason = StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or_default();
    let _ = writeln!(out, "- Success: `{status} {reason}`");
    let params = path_params(r.path);
    if !params.is_empty() {
        let list: Vec<String> = params
            .iter()
            .map(|p| format!("`{p}` ({})", r.param_type(p)))
            .collect();
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
        let lang = if r.is_yaml() && r.method == "GET" {
            "yaml"
        } else {
            "json"
        };
        let _ = write!(
            out,
            "Example response:\n\n```{lang}\n{}\n```\n\n",
            r.response
        );
    }
}

/// Who may call a route, in words.
fn access(r: &RouteDoc) -> &'static str {
    match r.access {
        "viewer" => "viewer (any API token, read scope is enough)",
        "editor" => "editor (write token of an editor or admin)",
        "admin" => "admin (write token of an admin)",
        "metrics token" => "API token as Bearer token or HTTP Basic password",
        other => credential(other).map_or("public (no token)", |c| c.words),
    }
}

/// The request body's top-level fields and their JSON types, from the example.
fn body_fields(r: &RouteDoc) -> Option<String> {
    if r.request.is_empty() {
        return None;
    }
    if r.is_yaml()
        && let Some(doc) = spec().config_document
    {
        return Some(doc.body.into());
    }
    if !r.is_json_body() {
        return Some(format!("`{}` (see the example)", r.body_type));
    }
    match serde_json::from_str::<Value>(r.request) {
        Ok(Value::Object(map)) => {
            let fields: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("`{k}` ({})", json_type(v)))
                .collect();
            Some(format!(
                "JSON object; fields used in the example: {}",
                fields.join(", ")
            ))
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

/// `curl` for a route, with `$<PREFIX>_URL` / `$<PREFIX>_TOKEN` placeholders.
pub fn curl(r: &RouteDoc) -> String {
    let p = crate::config::get().env_prefix;
    let file = spec().config_document.map_or("", |doc| doc.file);
    let path: Vec<String> = r
        .path
        .split('/')
        .map(|seg| (spec().curl_path_param)(r, seg).unwrap_or_else(|| seg.to_string()))
        .collect();
    let mut url = format!("${p}_URL{}", path.join("/"));
    let mut cmd = String::from("curl");
    if r.method != "GET" {
        let _ = write!(cmd, " -X {}", r.method);
    }
    match r.access {
        "viewer" | "editor" | "admin" | "metrics token" => {
            let _ = write!(cmd, " -H \"Authorization: Bearer ${p}_TOKEN\"");
        }
        other => match credential(other).map(|c| (c.bearer, c.header)) {
            Some((Some(scheme), _)) => {
                let _ = write!(
                    cmd,
                    " -H \"Authorization: Bearer ${p}_{}\"",
                    scheme.variable
                );
            }
            Some((None, Some(scheme))) => {
                let _ = write!(cmd, " -H \"{}: {}\"", scheme.header, scheme.example);
            }
            _ => {}
        },
    }
    if r.is_yaml() && r.method == "POST" {
        url.push_str("?dry_run=1");
        let _ = write!(
            cmd,
            " -H \"Content-Type: application/yaml\" --data-binary @{file} \"{url}\""
        );
    } else if r.is_yaml() {
        let _ = write!(cmd, " \"{url}\" -o {file}");
    } else if r.request.is_empty() {
        let _ = write!(cmd, " \"{url}\"");
    } else if !r.is_json_body() {
        let _ = write!(
            cmd,
            " -H \"Content-Type: {}\" \"{url}\" --data-binary '{}'",
            r.body_type,
            shell_quote(r.request)
        );
    } else {
        let _ = write!(
            cmd,
            " -H \"Content-Type: application/json\" \"{url}\" -d '{}'",
            shell_quote(r.request)
        );
    }
    cmd
}

/// Text for inside single quotes in a POSIX shell.
pub fn shell_quote(text: &str) -> String {
    text.replace('\'', "'\\''")
}

/// The example request body of a documented route.
pub fn example_body(method: &str, path: &str) -> &'static str {
    routes()
        .iter()
        .find(|r| r.method == method && r.path == path)
        .map_or("{}", |r| r.request)
}
