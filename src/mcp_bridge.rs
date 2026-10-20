//! stdio bridge for MCP clients that only speak stdio.
//!
//! `vigilant-vole mcp --url https://host` reads newline-delimited JSON-RPC
//! messages from stdin, POSTs each one to the server's `/mcp` endpoint
//! (Streamable HTTP, Bearer token) and writes the answers to stdout. stdout
//! carries protocol messages only; every human message goes to stderr.

use std::error::Error;
use std::time::Duration;

use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// JSON-RPC code: the line was not valid JSON.
pub const CODE_PARSE_ERROR: i64 = -32700;
/// The server refused the token (HTTP 401/403).
pub const CODE_AUTH: i64 = -32001;
/// The server is rate limiting (HTTP 429).
pub const CODE_RATE_LIMITED: i64 = -32002;
/// Any other HTTP error, redirect or unusable answer.
pub const CODE_HTTP: i64 = -32003;
/// The server could not be reached.
pub const CODE_CONNECT: i64 = -32004;

/// Command-line arguments of `vigilant-vole mcp`.
#[derive(Debug, Clone, clap::Args)]
pub struct McpArgs {
    /// Base URL of the Vigilant Vole server, e.g. https://status.example.com (`/mcp` is appended).
    /// The token is read from the environment (VV_MCP_TOKEN, falling back to VV_TOKEN).
    #[arg(long, env = "VV_MCP_URL")]
    pub url: String,
}

/// Settings of one bridge run.
#[derive(Clone)]
pub struct BridgeConfig {
    /// Full endpoint URL (ends in `/mcp`), see [`normalize_url`].
    pub endpoint: String,
    /// Bearer token; never printed.
    pub token: String,
}

impl std::fmt::Debug for BridgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeConfig")
            .field("endpoint", &self.endpoint)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Turn the user-supplied base URL into the MCP endpoint URL.
pub fn normalize_url(raw: &str) -> Result<String, String> {
    let mut url = url::Url::parse(raw.trim()).map_err(|e| format!("not a valid URL ({e})"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("the URL must start with http:// or https://".into());
    }
    if url.host_str().is_none() {
        return Err("the URL has no host".into());
    }
    url.set_fragment(None);
    url.set_query(None);
    let path = url.path().trim_end_matches('/').to_string();
    let path = if path.ends_with("/mcp") { path } else { format!("{path}/mcp") };
    url.set_path(&path);
    Ok(url.to_string())
}

fn token_from_env() -> Option<String> {
    ["VV_MCP_TOKEN", "VV_TOKEN"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty())
}

/// Entry point of the `mcp` subcommand. Exits with code 2 on a usage error.
pub async fn run(args: McpArgs) -> Result<(), Box<dyn Error + Send + Sync>> {
    let Some(token) = token_from_env() else {
        eprintln!("vigilant-vole mcp: no token; set the VV_MCP_TOKEN environment variable (or VV_TOKEN) to an API token");
        std::process::exit(2);
    };
    let endpoint = match normalize_url(&args.url) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("vigilant-vole mcp: invalid --url / VV_MCP_URL: {e}");
            std::process::exit(2);
        }
    };
    crate::tls_roots::init();
    let cfg = BridgeConfig { endpoint, token };
    let input = tokio::io::BufReader::new(tokio::io::stdin());
    run_bridge(cfg, input, tokio::io::stdout()).await?;
    Ok(())
}

type RpcError = (i64, String);

struct Bridge {
    http: reqwest::Client,
    cfg: BridgeConfig,
    protocol_version: Option<String>,
    session_id: Option<String>,
}

/// Pump messages from `input` to the server and answers to `output` until EOF.
pub async fn run_bridge<R, W>(cfg: BridgeConfig, input: R, mut output: W) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let http = crate::tls_roots::http_client()
        .user_agent(concat!("VigilantVoleMcpBridge/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(std::io::Error::other)?;
    let mut bridge = Bridge { http, cfg, protocol_version: None, session_id: None };
    let mut input = input;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if input.read_until(b'\n', &mut buf).await? == 0 {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Err(e) => {
                write_value(&mut output, &error_response(&Value::Null, CODE_PARSE_ERROR, &format!("parse error: {e}"))).await?;
            }
            Ok(msg) => bridge.handle(msg, &mut output).await?,
        }
    }
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The id of a JSON-RPC request (a message with a method and an id).
fn request_id(msg: &Value) -> Option<&Value> {
    let obj = msg.as_object()?;
    obj.get("method")?;
    obj.get("id")
}

async fn write_value<W: AsyncWrite + Unpin>(out: &mut W, v: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(v).map_err(std::io::Error::other)?;
    line.push('\n');
    out.write_all(line.as_bytes()).await?;
    out.flush().await
}

impl Bridge {
    async fn handle<W: AsyncWrite + Unpin>(&mut self, msg: Value, out: &mut W) -> std::io::Result<()> {
        let is_initialize = msg.get("method").and_then(Value::as_str) == Some("initialize");
        let Err((code, text)) = self.forward(&msg, is_initialize, out).await? else {
            return Ok(());
        };
        match &msg {
            Value::Array(items) => {
                let errors: Vec<Value> = items
                    .iter()
                    .filter_map(request_id)
                    .map(|id| error_response(id, code, &text))
                    .collect();
                if errors.is_empty() {
                    eprintln!("vigilant-vole mcp: batch failed: {text}");
                } else {
                    write_value(out, &Value::Array(errors)).await?;
                }
            }
            other => match request_id(other) {
                Some(id) => write_value(out, &error_response(id, code, &text)).await?,
                None => eprintln!("vigilant-vole mcp: notification failed: {text}"),
            },
        }
        Ok(())
    }

    /// POST one message. The outer `Result` is an stdout failure (fatal), the inner one a
    /// delivery failure to be reported to the client.
    async fn forward<W: AsyncWrite + Unpin>(
        &mut self,
        msg: &Value,
        is_initialize: bool,
        out: &mut W,
    ) -> std::io::Result<Result<(), RpcError>> {
        let mut req = self
            .http
            .post(&self.cfg.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .bearer_auth(&self.cfg.token)
            .body(msg.to_string());
        if let Some(v) = &self.protocol_version {
            req = req.header("MCP-Protocol-Version", v);
        }
        if let Some(s) = &self.session_id {
            req = req.header("Mcp-Session-Id", s);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return Ok(Err((CODE_CONNECT, format!("could not reach the server: {}", describe_error(&e))))),
        };
        let status = resp.status();
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            self.session_id = Some(s.to_string());
        }
        if !status.is_success() {
            return Ok(Err(http_error(resp).await));
        }
        if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Ok(()));
        }
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ctype.starts_with("text/event-stream") {
            let mut stream = resp.bytes_stream();
            let mut pending: Vec<u8> = Vec::new();
            let mut data = String::new();
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => return Ok(Err((CODE_CONNECT, format!("event stream broke: {}", describe_error(&e))))),
                };
                pending.extend_from_slice(&chunk);
                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let raw: Vec<u8> = pending.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&raw);
                    let line = line.trim_end_matches(['\n', '\r']);
                    if line.is_empty() {
                        if !data.is_empty() {
                            let payload = std::mem::take(&mut data);
                            if let Err(e) = self.emit(&payload, is_initialize, out).await? {
                                return Ok(Err(e));
                            }
                        }
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                    }
                }
            }
            if !data.is_empty() {
                return self.emit(&data, is_initialize, out).await;
            }
            return Ok(Ok(()));
        }
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return Ok(Err((CODE_CONNECT, format!("could not read the response: {}", describe_error(&e))))),
        };
        if body.trim().is_empty() {
            return Ok(Ok(()));
        }
        self.emit(&body, is_initialize, out).await
    }

    /// Write one payload as a compact line (and learn the protocol version from `initialize`).
    async fn emit<W: AsyncWrite + Unpin>(
        &mut self,
        payload: &str,
        is_initialize: bool,
        out: &mut W,
    ) -> std::io::Result<Result<(), RpcError>> {
        let value: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => return Ok(Err((CODE_HTTP, format!("the server sent a response that is not JSON: {e}")))),
        };
        if is_initialize
            && let Some(v) = value.pointer("/result/protocolVersion").and_then(Value::as_str)
        {
            self.protocol_version = Some(v.to_string());
        }
        write_value(out, &value).await?;
        Ok(Ok(()))
    }
}

fn describe_error(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        text.push_str(": ");
        text.push_str(&s.to_string());
        src = s.source();
    }
    text
}

async fn http_error(resp: reqwest::Response) -> RpcError {
    let status = resp.status();
    let code = status.as_u16();
    let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    if matches!(code, 401 | 403) {
        return (
            CODE_AUTH,
            format!("the server refused the token (HTTP {code}); check VV_MCP_TOKEN and that the token has the required scope"),
        );
    }
    if code == 429 {
        let mut msg = "the server is rate limiting requests (HTTP 429)".to_string();
        if let Some(r) = header("retry-after") {
            msg.push_str(&format!("; Retry-After: {r}"));
        }
        return (CODE_RATE_LIMITED, msg);
    }
    if status.is_redirection() {
        let loc = header("location").unwrap_or_else(|| "(none)".into());
        return (CODE_HTTP, format!("HTTP {code}: redirect to {loc} (redirects are not followed; check --url)"));
    }
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body.trim().chars().take(200).collect();
    (CODE_HTTP, format!("HTTP {code}: {snippet}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        response::{IntoResponse, Response},
        routing::post,
    };
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<(Value, Option<String>)>>>;

    async fn handler(State(seen): State<Seen>, headers: HeaderMap, body: String) -> Response {
        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer good") {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert!(headers.get("accept").unwrap().to_str().unwrap().contains("text/event-stream"));
        assert!(headers.get("user-agent").unwrap().to_str().unwrap().starts_with("VigilantVoleMcpBridge/"));
        let msg: Value = serde_json::from_str(&body).unwrap();
        let proto = headers.get("mcp-protocol-version").map(|v| v.to_str().unwrap().to_string());
        seen.lock().unwrap().push((msg.clone(), proto));
        if let Some(items) = msg.as_array() {
            let res: Vec<Value> = items.iter().filter_map(|m| m.get("id")).map(|id| json!({"jsonrpc":"2.0","id":id,"result":{}})).collect();
            return axum::Json(res).into_response();
        }
        let Some(id) = msg.get("id").cloned() else {
            return StatusCode::ACCEPTED.into_response();
        };
        match msg["method"].as_str().unwrap() {
            "initialize" => axum::Json(json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18"}})).into_response(),
            "limit" => (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "7")], "slow down").into_response(),
            "redirect" => (StatusCode::FOUND, [(header::LOCATION, "https://login.example/")]).into_response(),
            "fail" => (StatusCode::INTERNAL_SERVER_ERROR, "x".repeat(500)).into_response(),
            "sse" => (
                [(header::CONTENT_TYPE, "text/event-stream")],
                format!("event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":1}}\n\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"n\"}}\n\n"),
            )
                .into_response(),
            _ => axum::Json(json!({"jsonrpc":"2.0","id":id,"result":{}})).into_response(),
        }
    }

    async fn server() -> (String, Seen) {
        let seen: Seen = Arc::default();
        let app = Router::new().route("/mcp", post(handler)).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/mcp"), seen)
    }

    async fn bridge(endpoint: &str, token: &str, input: &str) -> Vec<Value> {
        let cfg = BridgeConfig { endpoint: endpoint.into(), token: token.into() };
        let mut out = Vec::new();
        run_bridge(cfg, tokio::io::BufReader::new(input.as_bytes()), &mut out).await.unwrap();
        String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[tokio::test]
    async fn ping_notification_and_blank_lines() {
        let (url, seen) = server().await;
        let out = bridge(&url, "good", "\n{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n   \n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n").await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 7);
        assert!(out[0]["result"].is_object());
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn initialize_sets_protocol_header() {
        let (url, seen) = server().await;
        let out = bridge(&url, "good", "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n{\"jsonrpc\":\"2.0\",\"id\":\"b\",\"method\":\"ping\"}\n").await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["id"], "b");
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].1, None);
        assert_eq!(seen[1].1.as_deref(), Some("2025-06-18"));
    }

    #[tokio::test]
    async fn wrong_token_is_32001_with_id() {
        let (url, _) = server().await;
        let out = bridge(&url, "bad", "{\"jsonrpc\":\"2.0\",\"id\":\"x\",\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"n\"}\n").await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], "x");
        assert_eq!(out[0]["error"]["code"], CODE_AUTH);
        let msg = out[0]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("VV_MCP_TOKEN") && !msg.contains("bad"));
    }

    #[tokio::test]
    async fn rate_limit_redirect_and_http_errors() {
        let (url, _) = server().await;
        let out = bridge(
            &url,
            "good",
            "{\"id\":1,\"method\":\"limit\"}\n{\"id\":2,\"method\":\"redirect\"}\n{\"id\":3,\"method\":\"fail\"}\n",
        )
        .await;
        assert_eq!(out[0]["id"], 1);
        assert_eq!(out[0]["error"]["code"], CODE_RATE_LIMITED);
        assert!(out[0]["error"]["message"].as_str().unwrap().contains('7'));
        assert_eq!(out[1]["id"], 2);
        assert_eq!(out[1]["error"]["code"], CODE_HTTP);
        assert!(out[1]["error"]["message"].as_str().unwrap().contains("https://login.example/"));
        assert_eq!(out[2]["error"]["code"], CODE_HTTP);
        let m = out[2]["error"]["message"].as_str().unwrap();
        assert!(m.contains("500") && m.len() < 260);
    }

    #[tokio::test]
    async fn sse_events_become_lines() {
        let (url, _) = server().await;
        let out = bridge(&url, "good", "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"sse\"}\n").await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["id"], 5);
        assert_eq!(out[1]["method"], "n");
    }

    #[tokio::test]
    async fn batch_works_and_batch_errors_answer_each_request() {
        let (url, _) = server().await;
        let batch = "[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"},{\"jsonrpc\":\"2.0\",\"method\":\"n\"}]\n";
        let ok = bridge(&url, "good", batch).await;
        assert_eq!(ok[0].as_array().unwrap().len(), 1);
        assert_eq!(ok[0][0]["id"], 1);
        let out = bridge(&url, "bad", batch).await;
        assert_eq!(out.len(), 1);
        let arr = out[0].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], 1);
        assert_eq!(arr[0]["error"]["code"], CODE_AUTH);
    }

    #[tokio::test]
    async fn invalid_json_is_parse_error_without_contacting_server() {
        let (url, seen) = server().await;
        let out = bridge(&url, "good", "{not json\n").await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], Value::Null);
        assert_eq!(out[0]["error"]["code"], CODE_PARSE_ERROR);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn connection_refused_is_32004() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let out = bridge(&format!("http://{addr}/mcp"), "good", "{\"id\":9,\"method\":\"ping\"}\n").await;
        assert_eq!(out[0]["id"], 9);
        assert_eq!(out[0]["error"]["code"], CODE_CONNECT);
    }

    #[test]
    fn url_normalisation() {
        assert_eq!(normalize_url("https://h").unwrap(), "https://h/mcp");
        assert_eq!(normalize_url("https://h/").unwrap(), "https://h/mcp");
        assert_eq!(normalize_url("https://h/mcp").unwrap(), "https://h/mcp");
        assert_eq!(normalize_url("http://h:8080/sub/mcp").unwrap(), "http://h:8080/sub/mcp");
        assert_eq!(normalize_url("http://h:8080/sub/").unwrap(), "http://h:8080/sub/mcp");
        assert!(normalize_url("ftp://x").is_err());
        assert!(normalize_url("garbage").is_err());
        assert!(normalize_url("").is_err());
    }
}
