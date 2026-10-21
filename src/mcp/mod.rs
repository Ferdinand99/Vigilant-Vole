//! MCP server: lets the owner's own AI assistant (Claude Code, Claude
//! Desktop, any MCP client) query and steer Vigilant Vole.
//!
//! Vigilant Vole never calls an AI model and never relays AI usage; it only
//! answers the client's requests. Transport: Streamable HTTP, `POST /mcp`
//! with JSON-RPC 2.0 (single messages; batches, which the 2025-06-18
//! revision dropped, are refused; the server is stateless and offers no
//! server-initiated stream, so `GET /mcp` is `405` as the specification
//! allows). Authentication is an API token (`Authorization: Bearer vv_…`):
//! a read token sees only the read tools, a write token also the editing
//! tools, and the user's role still applies. There are no session cookies.
//! A browser request must come from the configured public URL, or, without
//! one, from an IP address or `localhost` (DNS-rebinding protection: a name
//! that an attacker rebinds to this server would match the `Host` header,
//! so the `Host` header alone proves nothing).
//!
//! The tools are thin layers over the REST API: each call is sent to
//! `/api/v1/**` inside the process with the caller's token, so validation,
//! role checks and secret masking are exactly the REST API's. Secret values
//! are masked again on the way out. Nothing here deletes data or touches
//! users, settings, tokens or backups.

mod tools;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Map, Value, json};

use crate::api::AppState;
use crate::auth::{AuthMethod, CurrentUser, TokenScope};

/// Protocol versions we speak, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Largest request body.
const MAX_BODY: usize = 256 * 1024;
/// Requests per token and window.
const RATE_LIMIT: u32 = 120;
const RATE_WINDOW: Duration = Duration::from_secs(60);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mcp", post(handle).get(not_offered).delete(not_offered))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

// ---------------------------------------------------------------------------
// Rate limit
// ---------------------------------------------------------------------------

/// token id → (window start, requests in it). Keyed by instance and token so
/// separate databases (tests) don't share counters.
type Windows = HashMap<(usize, i64), (Instant, u32)>;
static WINDOWS: LazyLock<Mutex<Windows>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `Some(wait)` when the token has used up its requests for this window.
fn rate_limited(instance: usize, token_id: i64, now: Instant) -> Option<Duration> {
    let mut map = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    if map.len() > 1024 {
        map.retain(|_, (start, _)| now.duration_since(*start) < RATE_WINDOW);
    }
    let entry = map.entry((instance, token_id)).or_insert((now, 0));
    if now.duration_since(entry.0) >= RATE_WINDOW {
        *entry = (now, 0);
    }
    entry.1 += 1;
    (entry.1 > RATE_LIMIT).then(|| {
        RATE_WINDOW
            .saturating_sub(now.duration_since(entry.0))
            .max(Duration::from_secs(1))
    })
}

// ---------------------------------------------------------------------------
// HTTP layer
// ---------------------------------------------------------------------------

async fn not_offered() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST"), (header::CACHE_CONTROL, "no-store")],
        "This MCP server answers POST requests only (no server-initiated stream).\n",
    )
        .into_response()
}

fn http_error(status: StatusCode, message: &str) -> Response {
    let body = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32000, "message": message}});
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

fn unauthorized(message: &str) -> Response {
    let mut resp = http_error(StatusCode::UNAUTHORIZED, message);
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"Vigilant Vole\""),
    );
    resp
}

/// What a request is allowed to do.
pub(crate) struct Caller {
    pub user: CurrentUser,
    /// The `Authorization` header to pass on to the REST API.
    pub authorization: HeaderValue,
    /// `true` for a write-scope token held by an editor or admin.
    pub can_write: bool,
}

/// `(scheme, host, port)` of an http(s) URL, with the scheme's default port.
fn origin_of(url: &str) -> Option<(String, String, u16)> {
    let url = url::Url::parse(url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    Some((
        url.scheme().to_string(),
        url.host_str()?.to_ascii_lowercase(),
        url.port_or_known_default()?,
    ))
}

/// Whether a request may use the endpoint as far as its browser origin goes.
/// Non-browser clients send no `Origin` (or `Referer`) and pass. A browser
/// request must come from the configured public URL; without one, from an
/// address no other site can rebind to this server: an IP address or
/// `localhost`, matching the `Host` it was sent to.
fn origin_allowed(headers: &HeaderMap, base_url: Option<&str>) -> bool {
    let header = |name: header::HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    let Some(source) = header(header::ORIGIN).or_else(|| header(header::REFERER)) else {
        return true;
    };
    let Some(origin) = origin_of(source) else {
        // "null" (sandboxed pages, file://) and anything else unusable.
        return false;
    };
    if let Some(base) = base_url.map(str::trim).filter(|b| !b.is_empty()) {
        return origin_of(base).is_some_and(|b| b == origin);
    }
    let (scheme, host, _) = &origin;
    let fixed = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok();
    fixed
        && header(header::HOST)
            .and_then(|h| origin_of(&format!("{scheme}://{h}/")))
            .is_some_and(|h| h == origin)
}

async fn handle(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    // DNS-rebinding protection: a browser page from another origin must not
    // be able to drive this endpoint.
    let base_url = state.settings().await.ok().and_then(|s| s.base_url);
    if !origin_allowed(&headers, base_url.as_deref()) {
        return http_error(
            StatusCode::FORBIDDEN,
            "This Origin is not allowed. Browser clients must use the public URL from Settings → General.",
        );
    }

    // Bearer tokens only; no cookies, no Basic.
    let Some(authorization) = headers.get(header::AUTHORIZATION).cloned() else {
        return unauthorized("Send your API token as `Authorization: Bearer vv_…`.");
    };
    let token = authorization
        .to_str()
        .ok()
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, t)| t.trim().to_string());
    let Some(token) = token else {
        return unauthorized("Send your API token as `Authorization: Bearer vv_…`.");
    };
    let (user, method) = match crate::api::auth::authenticate_token_detailed(&state, &token).await {
        Ok(Some(found)) => found,
        Ok(None) => return unauthorized("The API token is not valid (wrong, expired or revoked)."),
        Err(_) => {
            return http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not check the token.",
            );
        }
    };
    let (token_id, scope) = match method {
        AuthMethod::Token { token_id, scope } => (token_id, scope),
        AuthMethod::Session => return unauthorized("Use an API token."),
    };
    let instance = std::sync::Arc::as_ptr(&state.config) as usize;
    if let Some(wait) = rate_limited(instance, token_id, Instant::now()) {
        let mut resp = http_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests for this token; slow down.",
        );
        if let Ok(v) = HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return resp;
    }

    if let Some(v) = headers
        .get("mcp-protocol-version")
        .and_then(|v| v.to_str().ok())
        && !PROTOCOL_VERSIONS.contains(&v)
    {
        return http_error(
            StatusCode::BAD_REQUEST,
            &format!("Unsupported MCP-Protocol-Version {v}."),
        );
    }

    let caller = Caller {
        can_write: scope == TokenScope::Write && user.can_edit(),
        user,
        authorization,
    };
    let ctx = tools::Ctx::new(state, caller, &headers);

    let message: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return json_response(rpc_error(
                Value::Null,
                -32700,
                "Parse error: the body is not JSON.",
            ));
        }
    };
    if message.is_array() {
        // MCP 2025-06-18 removed JSON-RPC batching. Answering them would let
        // one request (and one rate-limit unit) carry hundreds of tool calls.
        return (
            StatusCode::BAD_REQUEST,
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(rpc_error(
                Value::Null,
                -32600,
                "Invalid request: batches are not supported; send one JSON-RPC message per request.",
            )),
        )
            .into_response();
    }
    match process(&ctx, message).await {
        Some(reply) => json_response(reply),
        None => accepted(),
    }
}

fn accepted() -> Response {
    (StatusCode::ACCEPTED, [(header::CACHE_CONTROL, "no-store")]).into_response()
}

fn json_response(value: Value) -> Response {
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(value),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Handles one message. `None` for notifications and client responses.
async fn process(ctx: &tools::Ctx, message: Value) -> Option<Value> {
    let Value::Object(obj) = message else {
        return Some(rpc_error(
            Value::Null,
            -32600,
            "Invalid request: expected a JSON object.",
        ));
    };
    let id = obj.get("id").cloned();
    let method = obj.get("method").and_then(Value::as_str);
    // A response from the client to a server request (we send none): ignore.
    if method.is_none() && (obj.contains_key("result") || obj.contains_key("error")) {
        return None;
    }
    let Some(method) = method else {
        return Some(rpc_error(
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid request: no method.",
        ));
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(rpc_error(
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid request: jsonrpc must be \"2.0\".",
        ));
    }
    let params = match obj.get("params") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(p)) => p.clone(),
        Some(_) => {
            return id.map(|id| rpc_error(id, -32602, "Invalid params: expected an object."));
        }
    };
    // Notifications carry no id and get no reply.
    let id = id.filter(|id| !id.is_null())?;
    Some(match dispatch(ctx, method, &params).await {
        Ok(result) => rpc_result(id, result),
        Err((code, message)) => rpc_error(id, code, &message),
    })
}

type RpcResult = Result<Value, (i64, String)>;

async fn dispatch(ctx: &tools::Ctx, method: &str, params: &Map<String, Value>) -> RpcResult {
    match method {
        "initialize" => Ok(initialize(ctx, params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::list(ctx.can_write()) })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or((-32602, "Invalid params: `name` is required.".to_string()))?;
            let args = match params.get("arguments") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(a)) => a.clone(),
                Some(_) => {
                    return Err((
                        -32602,
                        "Invalid params: `arguments` must be an object.".into(),
                    ));
                }
            };
            tools::call(ctx, name, args).await
        }
        "resources/list" => tools::list_resources(ctx).await,
        "resources/templates/list" => Ok(tools::resource_templates()),
        "resources/read" => {
            let uri = params
                .get("uri")
                .and_then(Value::as_str)
                .ok_or((-32602, "Invalid params: `uri` is required.".to_string()))?;
            tools::read_resource(ctx, uri).await
        }
        other => Err((-32601, format!("Method not found: {other}"))),
    }
}

fn initialize(ctx: &tools::Ctx, params: &Map<String, Value>) -> Value {
    let wanted = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == wanted)
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    let mode = if ctx.can_write() {
        "This token can also change things (pause monitors, post incidents, edit monitors); it acts with the rights of its owner."
    } else {
        "This token is read-only: you can look but not change anything."
    };
    json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
        },
        "serverInfo": {
            "name": "vigilant-vole",
            "title": "Vigilant Vole",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": format!(
            "Vigilant Vole is a self-hosted uptime monitor. Start with `instance_summary`, use `why_is_it_down` for a failing monitor and `monitor_history` for trends. {mode} Secrets are always shown as ••••••."
        ),
    })
}

#[cfg(test)]
mod tests;
