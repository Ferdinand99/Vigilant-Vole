//! The protocol side of an MCP server: lets a product's owner connect their
//! own AI assistant (Claude Code, Claude Desktop, any MCP client).
//!
//! Transport: Streamable HTTP, `POST` with JSON-RPC 2.0 (single messages;
//! batches, which the 2025-06-18 revision dropped, are refused; the server
//! is stateless and offers no server-initiated stream, so `GET` is `405` as
//! the specification allows). Authentication is an API token
//! (`Authorization: Bearer <prefix>…`); there are no session cookies. A
//! browser request must come from the configured public URL, or, without
//! one, from an IP address or `localhost` (DNS-rebinding protection: a name
//! that an attacker rebinds to this server would match the `Host` header, so
//! the `Host` header alone proves nothing).
//!
//! The product supplies the tools and resources through [`Handler`].

use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::auth::guard;
use crate::auth::{AuthMethod, CurrentUser, TokenScope};
use crate::state::Core;

/// Protocol versions we speak, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Largest request body.
pub const MAX_BODY: usize = 256 * 1024;
/// Requests per token and window.
pub const RATE_LIMIT: u32 = 120;
pub const RATE_WINDOW: Duration = Duration::from_secs(60);

/// token id → (window start, requests in it). Keyed by instance and token so
/// separate databases (tests) don't share counters.
type Windows = HashMap<(usize, i64), (Instant, u32)>;
static WINDOWS: LazyLock<Mutex<Windows>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `Some(wait)` when the token has used up its requests for this window.
pub fn rate_limited(instance: usize, token_id: i64, now: Instant) -> Option<Duration> {
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

pub async fn not_offered() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST"), (header::CACHE_CONTROL, "no-store")],
        "This MCP server answers POST requests only (no server-initiated stream).\n",
    )
        .into_response()
}

pub fn http_error(status: StatusCode, message: &str) -> Response {
    let body = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32000, "message": message}});
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

pub fn unauthorized(message: &str) -> Response {
    let mut resp = http_error(StatusCode::UNAUTHORIZED, message);
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_str(&format!(
            "Bearer realm=\"{}\"",
            crate::config::get().product
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("Bearer")),
    );
    resp
}

/// What a request is allowed to do.
pub struct Caller {
    pub user: CurrentUser,
    /// The `Authorization` header to pass on to the REST API.
    pub authorization: HeaderValue,
    /// `true` for a write-scope token held by an editor or admin.
    pub can_write: bool,
}

/// `(scheme, host, port)` of an http(s) URL, with the scheme's default port.
pub fn origin_of(url: &str) -> Option<(String, String, u16)> {
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
pub fn origin_allowed(headers: &HeaderMap, base_url: Option<&str>) -> bool {
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

/// The message for a browser request from an origin that isn't allowed.
pub fn origin_refused(cfg: &crate::config::KitConfig) -> String {
    match cfg.public_url_setting {
        Some(place) => format!(
            "This Origin is not allowed. Browser clients must use the public URL from {place}."
        ),
        None => {
            "This Origin is not allowed. Browser clients must use the instance's public URL.".into()
        }
    }
}

/// Checks everything about a request except its body: the browser origin,
/// the Bearer API token (no cookies, no Basic), the per-token rate limit and
/// the protocol version header. `instance` tells apart apps that share a
/// process (tests).
#[allow(clippy::result_large_err)] // the error is the finished HTTP response, built once per rejected request
pub async fn authenticate(
    core: &Core,
    headers: &HeaderMap,
    instance: usize,
) -> Result<Caller, Response> {
    let prefix = core.cfg.token_prefix;
    // DNS-rebinding protection: a browser page from another origin must not
    // be able to drive this endpoint.
    let base_url = core.base_url().await;
    if !origin_allowed(headers, base_url.as_deref()) {
        return Err(http_error(StatusCode::FORBIDDEN, &origin_refused(core.cfg)));
    }

    let need_token = format!("Send your API token as `Authorization: Bearer {prefix}…`.");
    let Some(authorization) = headers.get(header::AUTHORIZATION).cloned() else {
        return Err(unauthorized(&need_token));
    };
    let token = authorization
        .to_str()
        .ok()
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, t)| t.trim().to_string());
    let Some(token) = token else {
        return Err(unauthorized(&need_token));
    };
    let (user, method) = match guard::authenticate_token_detailed(core, &token).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return Err(unauthorized(
                "The API token is not valid (wrong, expired or revoked).",
            ));
        }
        Err(_) => {
            return Err(http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not check the token.",
            ));
        }
    };
    let (token_id, scope) = match method {
        AuthMethod::Token { token_id, scope } => (token_id, scope),
        AuthMethod::Session => return Err(unauthorized("Use an API token.")),
    };
    if !user.can_read() {
        return Err(http_error(
            StatusCode::FORBIDDEN,
            "This API token can't read, so it can't use MCP.",
        ));
    }
    if let Some(wait) = rate_limited(instance, token_id, Instant::now()) {
        let mut resp = http_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests for this token; slow down.",
        );
        if let Ok(v) = HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return Err(resp);
    }

    if let Some(v) = headers
        .get("mcp-protocol-version")
        .and_then(|v| v.to_str().ok())
        && !PROTOCOL_VERSIONS.contains(&v)
    {
        return Err(http_error(
            StatusCode::BAD_REQUEST,
            &format!("Unsupported MCP-Protocol-Version {v}."),
        ));
    }

    Ok(Caller {
        can_write: scope == TokenScope::Write && user.can_edit(),
        user,
        authorization,
    })
}

// ---------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------

pub fn accepted() -> Response {
    (StatusCode::ACCEPTED, [(header::CACHE_CONTROL, "no-store")]).into_response()
}

pub fn json_response(value: Value) -> Response {
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

pub fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

pub type RpcResult = Result<Value, (i64, String)>;

/// What a product answers to MCP requests: the tools and resources it
/// offers. The kit does the protocol around it.
pub trait Handler: Sync {
    /// Whether the caller's token may change things.
    fn can_write(&self) -> bool;
    /// The `instructions` the server announces on `initialize`.
    fn instructions(&self) -> String;
    /// The `tools/list` entries for this caller.
    fn tools(&self) -> Vec<Value>;
    fn call_tool(
        &self,
        name: &str,
        args: Map<String, Value>,
    ) -> impl Future<Output = RpcResult> + Send;
    fn list_resources(&self) -> impl Future<Output = RpcResult> + Send;
    fn resource_templates(&self) -> Value;
    fn read_resource(&self, uri: &str) -> impl Future<Output = RpcResult> + Send;
}

/// Answers one MCP HTTP request body (after authentication).
pub async fn serve<H: Handler>(handler: &H, body: &[u8]) -> Response {
    let message: Value = match serde_json::from_slice(body) {
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
    match process(handler, message).await {
        Some(reply) => json_response(reply),
        None => accepted(),
    }
}

/// Handles one message. `None` for notifications and client responses.
async fn process<H: Handler>(handler: &H, message: Value) -> Option<Value> {
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
    Some(match dispatch(handler, method, &params).await {
        Ok(result) => rpc_result(id, result),
        Err((code, message)) => rpc_error(id, code, &message),
    })
}

async fn dispatch<H: Handler>(handler: &H, method: &str, params: &Map<String, Value>) -> RpcResult {
    match method {
        "initialize" => Ok(initialize(handler, params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": handler.tools() })),
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
            handler.call_tool(name, args).await
        }
        "resources/list" => handler.list_resources().await,
        "resources/templates/list" => Ok(handler.resource_templates()),
        "resources/read" => {
            let uri = params
                .get("uri")
                .and_then(Value::as_str)
                .ok_or((-32602, "Invalid params: `uri` is required.".to_string()))?;
            handler.read_resource(uri).await
        }
        other => Err((-32601, format!("Method not found: {other}"))),
    }
}

fn initialize<H: Handler>(handler: &H, params: &Map<String, Value>) -> Value {
    let wanted = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == wanted)
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
        },
        "serverInfo": {
            "name": crate::config::get().slug,
            "title": crate::config::get().product,
            "version": crate::config::get().version,
        },
        "instructions": handler.instructions(),
    })
}
